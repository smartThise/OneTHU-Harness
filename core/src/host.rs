//! 宿主边界抽象：核心 crate 只面向 `Host`（数据调用 + 打断）与 `Emit`（通知）。
//! - stdio sidecar（bin/）：`StdioHost` + `StdioEmit`——JSON-RPC over stdin/stdout；
//! - App 内嵌（OneTHU src-tauri）：`TauriHost` + `TauriEmit`——事件桥到 webview 门面。
//! 两个实现都不在核心 crate 内，核心保持平台无关（Android 可编译）。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 数据面：onethu.* 原子调用（权限门禁在宿主侧）+ 打断标志
pub trait Host {
    fn call(&mut self, ns: &str, method: &str, args: Value) -> Result<Value, String>;
    /// 带短超时的调用（落盘等可跳过的旁路操作用）：默认与 call 相同
    fn call_timeout(&mut self, ns: &str, method: &str, args: Value, _timeout_ms: u64) -> Result<Value, String> {
        let _ = _timeout_ms;
        self.call(ns, method, args)
    }
    fn interrupted(&self) -> bool;
    /// 兼容旧宿主；新的运行票只允许置为取消，不能被下一轮重置。
    fn reset_interrupt(&self) {}
}

/// 同一宿主一次只允许一轮 chat 或会话写操作。取消只标记当前票；持票线程真正退出才释放。
/// 不使用“运行超过多少秒”的接管，也不因被拒绝的新请求改变旧票。
#[derive(Clone, Default)]
pub struct ChatRunGate {
    active: Arc<Mutex<Option<ActiveChatRun>>>,
}

struct ActiveChatRun {
    owner: Option<String>,
    interrupt: Arc<AtomicBool>,
}

impl ChatRunGate {
    pub fn try_begin(&self) -> Option<ChatRunPermit> {
        self.try_begin_for(None)
    }

    pub fn try_begin_for(&self, owner: Option<&str>) -> Option<ChatRunPermit> {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if active.is_some() {
            return None;
        }
        let interrupt = Arc::new(AtomicBool::new(false));
        *active = Some(ActiveChatRun { owner: owner.map(str::to_owned), interrupt: interrupt.clone() });
        Some(ChatRunPermit { gate: self.clone(), interrupt })
    }

    pub fn cancel(&self) {
        self.cancel_for(None);
    }

    /// 新宿主只取消自己那轮；旧宿主没传 runId 时保留“取消当前轮”的兼容语义。
    pub fn cancel_for(&self, owner: Option<&str>) -> bool {
        if let Some(run) = self.active.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            if owner.is_none() || owner == run.owner.as_deref() {
                run.interrupt.store(true, Ordering::SeqCst);
                return true;
            }
        }
        false
    }

    pub fn is_busy(&self) -> bool {
        self.active.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn interrupted(&self) -> bool {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
            .as_ref().is_some_and(|run| run.interrupt.load(Ordering::SeqCst))
    }
}

pub struct ChatRunPermit {
    gate: ChatRunGate,
    interrupt: Arc<AtomicBool>,
}

impl ChatRunPermit {
    pub fn interrupt_flag(&self) -> Arc<AtomicBool> {
        self.interrupt.clone()
    }
}

impl Drop for ChatRunPermit {
    fn drop(&mut self) {
        let mut active = self.gate.active.lock().unwrap_or_else(|e| e.into_inner());
        if active.as_ref().is_some_and(|run| Arc::ptr_eq(&run.interrupt, &self.interrupt)) {
            *active = None;
        }
    }
}

pub fn chat_busy_result() -> Value {
    json!({
        "type": "chat", "ok": false,
        "error": "已有对话在运行：请等它完成；如已打断，请等待当前请求结束后再发。"
    })
}

/// 这些命令都写共享会话存档；只读列表、导出和用量查询仍可并行。
pub fn requires_run_permit(command: &str) -> bool {
    matches!(command, "chat" | "new_session" | "switch_session" | "delete_session" | "import_session" | "restore_main")
}

/// 通知面：progress / log 等（实现须可在多线程上下文调用）
pub trait Emit {
    fn notify(&self, method: &str, params: Value);
}

/// 为一次 dispatch 的所有对象事件附上归属；不改消息内容，也不创建无归属的替代事件。
pub struct RunEmit<'a> {
    inner: &'a dyn Emit,
    run_id: Option<&'a str>,
}

impl<'a> RunEmit<'a> {
    pub fn new(inner: &'a dyn Emit, run_id: Option<&'a str>) -> Self {
        Self { inner, run_id }
    }
}

impl Emit for RunEmit<'_> {
    fn notify(&self, method: &str, mut params: Value) {
        if let (Some(run_id), Some(object)) = (self.run_id, params.as_object_mut()) {
            object.insert("runId".into(), Value::String(run_id.to_owned()));
        }
        self.inner.notify(method, params);
    }
}

/* ═══════════════ stdio 实现 ═══════════════ */

pub enum Incoming {
    Request { id: Value, method: String, params: Value, chat: Option<ChatRunPermit> },
    Notify { method: String, params: Value },
    Response { id: u64, ok: bool, value: Value },
}

/// 通知到达时的回调：宿主主循环用它收「call 期间抵达的宿主请求」
pub type Deferred = Vec<(Value, String, Value)>;

pub type PendingMap = Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>>;

/// 桥回执等待上限：工具链路 TS 侧自带 45s，600s 是「宿主整个没了」级兜底
/// 桥调用是本地 IPC（毫秒级）——30s 已是极限宽限；挂死必须尽快显形而非拖 10 分钟
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(60); // R10: webvpn 自愈链（token 重取+复核）可能 30s+

pub struct StdioHost {
    /// 泵线程路由来的插件 RPC 请求/通知（主循环 poll 阻塞在此，无锁竞争）；
    /// chat 进行中抵达的 run/dispose 在通道里排队——与旧 deferred 列表同语义同时序
    req_rx: mpsc::Receiver<Incoming>,
    /// 桥回执表：按序号投递，主循环与控制线程的桥调用天然并发
    pending: PendingMap,
    seq: Arc<AtomicU64>,
    chat_runs: ChatRunGate,
}

impl StdioHost {
    /// stdin 由单个路由线程按输入顺序处理。先为 chat 认领运行票，再接收其 interrupt，
    /// 不让“请求已排队、工作线程未启动”的早到打断被开局 reset 吞掉。
    /// 桥 Arc 的共享句柄（本机 MCP server 线程用：同一张 pending 表并发服务外部 agent）
    pub fn bridge_arcs(&self) -> (PendingMap, Arc<AtomicU64>) {
        (self.pending.clone(), self.seq.clone())
    }

    pub fn spawn() -> StdioHost {
        let (req_tx, req_rx) = mpsc::channel::<Incoming>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let chat_runs = ChatRunGate::default();

        // 回执直投桥调用方；chat 的 admission 与 interrupt 在同一线程按序生效。
        {
            let pending = pending.clone();
            let runs = chat_runs.clone();
            std::thread::spawn(move || {
                let stdin = std::io::stdin();
                let mut input = stdin.lock();
                let mut raw = String::new();
                loop {
                    raw.clear();
                    match input.read_line(&mut raw) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let Ok(msg) = serde_json::from_str::<Value>(&raw) else {
                        continue;
                    };
                    if let Some(method) = msg.get("method").and_then(|m| m.as_str()).map(String::from) {
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);
                        if method == "interrupt" {
                            runs.cancel_for(params.get("runId").and_then(Value::as_str));
                            continue;
                        }
                        // 带 id = 宿主→插件的 RPC 请求（activate/run/dispose），交主循环；
                        // 无 id = 通知（onethu.event 进度流），同样交主循环消费
                        let incoming = match msg.get("id").cloned() {
                            Some(id) if !id.is_null() => {
                                let chat = if method == "run" && params.get("command").and_then(Value::as_str).is_some_and(requires_run_permit) {
                                    let Some(permit) = runs.try_begin_for(params.get("runId").and_then(Value::as_str)) else {
                                        send(&json!({ "jsonrpc": "2.0", "id": id, "result": chat_busy_result() }));
                                        continue;
                                    };
                                    Some(permit)
                                } else {
                                    None
                                };
                                Incoming::Request { id, method, params, chat }
                            }
                            _ => Incoming::Notify { method, params },
                        };
                        let _ = req_tx.send(incoming);
                        continue;
                    }
                    if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                        let out = if let Some(err) = msg.get("error") {
                            Err(err
                                .get("message")
                                .and_then(|m| m.as_str())
                                .map(String::from)
                                .unwrap_or_else(|| err.to_string()))
                        } else {
                            Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                        };
                        if let Some(tx) = pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                            let _ = tx.send(out);
                        }
                    }
                }
                runs.cancel();
            });
        }

        StdioHost {
            req_rx,
            pending,
            seq: Arc::new(AtomicU64::new(1000)),
            chat_runs,
        }
    }

    /// chat 只读自己的取消票；控制命令用独立空票，不能取消或复活正在运行的对话。
    pub fn bridge(&self, chat: Option<&ChatRunPermit>) -> BridgeHandle {
        BridgeHandle {
            pending: Arc::clone(&self.pending),
            seq: Arc::clone(&self.seq),
            interrupt: chat.map(ChatRunPermit::interrupt_flag).unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
        }
    }

    pub fn reply_ok(&self, id: &Value, result: Value) {
        send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn reply_err(&self, id: &Value, message: &str) {
        send(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": message } }));
    }

    /// 取下一条请求/通知（阻塞；回执不经过这里——泵线程按序号直投桥调用方）
    pub fn poll(&mut self) -> Incoming {
        match self.req_rx.recv() {
            Ok(i) => i,
            Err(_) => std::process::exit(0), // stdin 关闭 = 宿主已走
        }
    }

    /// 兼容桩：旧「call 期间暂存宿主请求」的语义现在由 req_rx 天然承载——
    /// chat 进行中抵达的 run/dispose 在通道里排队，主循环回到 poll 后按序处理。
    /// 保留空实现以维持 bin 侧调用面不变。
    pub fn take_deferred(&mut self) -> Deferred {
        Vec::new()
    }
}

/// 桥调用共用体：发请求 → 等泵线程按序号投递回执
pub fn bridge_call(
    pending: &PendingMap,
    seq: &AtomicU64,
    ns: &str,
    method: &str,
    args: Value,
) -> Result<Value, String> {
    bridge_call_timeout(pending, seq, ns, method, args, BRIDGE_TIMEOUT.as_millis() as u64)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn bridge_call_timeout(
    pending: &PendingMap,
    seq: &AtomicU64,
    ns: &str,
    method: &str,
    args: Value,
    timeout_ms: u64,
) -> Result<Value, String> {
    let rid = seq.fetch_add(1, Ordering::SeqCst) + 1;
    let (tx, rx) = mpsc::channel::<Result<Value, String>>();
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(rid, tx);
    send(&json!({
        "jsonrpc": "2.0", "id": rid,
        "method": "onethu.call",
        "params": { "ns": ns, "method": method, "args": args }
    }));
    // 探针：桥调用 5s 无回执就留痕（stderr 逐行转发 UI 日志面板）——定位回执丢失层
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let done = done.clone();
        let ns2 = ns.to_string();
        let m2 = method.to_string();
        std::thread::spawn(move || {
            for _ in 0..5 {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            if !done.load(Ordering::Relaxed) {
                eprintln!("[harness] ⏸ 桥调用 {}.{}（#{}）已等 5s 仍无回执", ns2, m2, rid);
            }
        });
    }
    let t0 = std::time::Instant::now();
    let out = match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(v) => {
            eprintln!(
                "[harness] ✓ 桥回执 #{}（{}ms）",
                rid,
                t0.elapsed().as_millis()
            );
            v
        }
        Err(_) => {
            eprintln!(
                "[harness] ✗ 桥调用 {}.{}（#{}）超时 {}s",
                ns,
                method,
                rid,
                timeout_ms / 1000
            );
            Err(format!("宿主桥应答超时（{}s）", timeout_ms / 1000))
        }
    };
    // 超时的控制面探针也要回收票据；设置页轮询不能累积永远无回执的 sender。
    pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&rid);
    done.store(true, Ordering::Relaxed);
    out
}

impl Host for StdioHost {
    fn call(&mut self, ns: &str, method: &str, args: Value) -> Result<Value, String> {
        bridge_call(&self.pending, &self.seq, ns, method, args)
    }

    /// 短超时调用（ui_navigate 等即时性 UI 动作用）：stdio 侧尊重传入的窗口，
    /// 超时立即回错误显形，而不是退化到 BRIDGE_TIMEOUT 长等。
    fn call_timeout(
        &mut self,
        ns: &str,
        method: &str,
        args: Value,
        timeout_ms: u64,
    ) -> Result<Value, String> {
        bridge_call_timeout(&self.pending, &self.seq, ns, method, args, timeout_ms)
    }

    fn interrupted(&self) -> bool {
        self.chat_runs.interrupted()
    }

}

/// 工作线程桥（chat / 控制命令共用），取消票只读且永不复位。
pub struct BridgeHandle {
    pending: PendingMap,
    seq: Arc<AtomicU64>,
    interrupt: Arc<AtomicBool>,
}

impl BridgeHandle {
    pub fn reply_ok(&self, id: &Value, result: Value) {
        send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn reply_err(&self, id: &Value, message: &str) {
        send(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": message } }));
    }
}

impl Host for BridgeHandle {
    fn call(&mut self, ns: &str, method: &str, args: Value) -> Result<Value, String> {
        bridge_call(&self.pending, &self.seq, ns, method, args)
    }

    fn call_timeout(
        &mut self,
        ns: &str,
        method: &str,
        args: Value,
        timeout_ms: u64,
    ) -> Result<Value, String> {
        bridge_call_timeout(&self.pending, &self.seq, ns, method, args, timeout_ms)
    }

    fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst)
    }
}

pub struct StdioEmit;

impl Emit for StdioEmit {
    fn notify(&self, method: &str, params: Value) {
        send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }
}

fn send(v: &Value) {
    use std::io::Write;
    let mut s = serde_json::to_string(v).unwrap_or_default();
    s.push('\n');
    let out = std::io::stdout();
    let mut h = out.lock();
    let _ = h.write_all(s.as_bytes());
    let _ = h.flush();
}

fn value_to_msg(v: &Value) -> String {
    match v.as_str() {
        Some(s) => s.to_string(),
        None => v.to_string(),
    }
}

#[cfg(test)]
mod chat_gate_tests {
    use super::*;

    #[test]
    fn busy_rejection_does_not_change_or_release_the_active_run() {
        let gate = ChatRunGate::default();
        let first = gate.try_begin().unwrap();
        for _ in 0..8 {
            assert!(gate.try_begin().is_none());
        }
        assert!(!first.interrupt_flag().load(Ordering::SeqCst));
        drop(first);
        assert!(gate.try_begin().is_some(), "busy replies must not leave the gate stuck");
    }

    #[test]
    fn early_cancel_is_not_reset_and_next_run_has_its_own_flag() {
        let gate = ChatRunGate::default();
        let first = gate.try_begin().unwrap();
        let old_flag = first.interrupt_flag();
        // Cancellation may arrive after admission but before a worker starts.
        gate.cancel();
        assert!(old_flag.load(Ordering::SeqCst));
        assert!(gate.try_begin().is_none(), "cancel is not proof that the old request ended");
        drop(first);
        let next = gate.try_begin().unwrap();
        assert!(!next.interrupt_flag().load(Ordering::SeqCst));
        assert!(old_flag.load(Ordering::SeqCst), "a new run must not revive an old token");
        gate.cancel();
        assert!(next.interrupt_flag().load(Ordering::SeqCst));
    }

    #[test]
    fn rejected_or_finished_run_cannot_cancel_another_owners_request() {
        let gate = ChatRunGate::default();
        let original = gate.try_begin_for(Some("im-run")).unwrap();
        assert!(gate.try_begin_for(Some("dock-rejected")).is_none());
        assert!(!gate.cancel_for(Some("dock-rejected")));
        assert!(!original.interrupt_flag().load(Ordering::SeqCst));
        assert!(gate.cancel_for(Some("im-run")));
        assert!(original.interrupt_flag().load(Ordering::SeqCst));
        drop(original);
        let next = gate.try_begin_for(Some("next-run")).unwrap();
        assert!(!gate.cancel_for(Some("im-run")), "late cancellation must not target the next turn");
        assert!(!next.interrupt_flag().load(Ordering::SeqCst));
        assert!(gate.cancel_for(None), "legacy hosts can still cancel the current turn");
        assert!(next.interrupt_flag().load(Ordering::SeqCst));
    }

    #[test]
    fn concurrent_admission_has_only_one_owner() {
        let gate = ChatRunGate::default();
        let start = Arc::new(std::sync::Barrier::new(9));
        let (tx, rx) = mpsc::channel();
        let threads: Vec<_> = (0..8).map(|_| {
            let gate = gate.clone();
            let start = start.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                start.wait();
                tx.send(gate.try_begin()).unwrap();
            })
        }).collect();
        start.wait();
        let permits: Vec<_> = (0..8).map(|_| rx.recv().unwrap()).collect();
        assert_eq!(permits.iter().filter(|p| p.is_some()).count(), 1);
        for worker in threads { worker.join().unwrap(); }
        assert!(gate.is_busy());
        drop(permits);
        assert!(!gate.is_busy());
    }

    #[test]
    fn unwinding_worker_releases_gate_and_idle_cancel_does_not_poison_next_run() {
        let gate = ChatRunGate::default();
        let worker_gate = gate.clone();
        let result = std::panic::catch_unwind(move || {
            let _permit = worker_gate.try_begin().unwrap();
            panic!("simulated worker failure");
        });
        assert!(result.is_err());
        assert!(!gate.is_busy());
        gate.cancel();
        assert!(!gate.try_begin().unwrap().interrupt_flag().load(Ordering::SeqCst));
    }

    #[test]
    fn scoped_events_keep_each_runs_id_and_original_payload() {
        #[derive(Default)]
        struct Capture(Mutex<Vec<(String, Value)>>);
        impl Emit for Capture {
            fn notify(&self, method: &str, params: Value) {
                self.0.lock().unwrap().push((method.to_owned(), params));
            }
        }
        let capture = Capture::default();
        let first = RunEmit::new(&capture, Some("im-run"));
        let second = RunEmit::new(&capture, Some("dock-run"));
        let kinds = ["delta", "think", "tool", "log", "notice", "usage"];
        for kind in kinds {
            let payload = json!({ "kind": kind, "text": "unchanged", "payload": { "calls": 1 } });
            first.notify("progress", payload.clone());
            second.notify("progress", payload);
        }
        let events = capture.0.lock().unwrap();
        assert_eq!(events.len(), kinds.len() * 2);
        for (i, (method, params)) in events.iter().enumerate() {
            assert_eq!(method, "progress");
            assert_eq!(params["runId"], if i % 2 == 0 { "im-run" } else { "dock-run" });
            assert_eq!(params["kind"], kinds[i / 2]);
            assert_eq!(params["text"], "unchanged");
            assert_eq!(params["payload"]["calls"], 1);
        }
    }

}
