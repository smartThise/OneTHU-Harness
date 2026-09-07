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
    /// 一轮 run 开始时清打断标志（stdio 侧实现；内嵌侧由宿主管理）
    fn reset_interrupt(&self) {}
}

/// 通知面：progress / log 等（实现须可在多线程上下文调用）
pub trait Emit {
    fn notify(&self, method: &str, params: Value);
}

/* ═══════════════ stdio 实现 ═══════════════ */

pub enum Incoming {
    Request { id: Value, method: String, params: Value },
    Notify { method: String, params: Value },
    Response { id: u64, ok: bool, value: Value },
}

/// 通知到达时的回调：宿主主循环用它收「call 期间抵达的宿主请求」
pub type Deferred = Vec<(Value, String, Value)>;

type PendingMap = Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>>;

/// 桥回执等待上限：工具链路 TS 侧自带 45s，600s 是「宿主整个没了」级兜底
/// 桥调用是本地 IPC（毫秒级）——30s 已是极限宽限；挂死必须尽快显形而非拖 10 分钟
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct StdioHost {
    /// 泵线程路由来的插件 RPC 请求/通知（主循环 poll 阻塞在此，无锁竞争）；
    /// chat 进行中抵达的 run/dispose 在通道里排队——与旧 deferred 列表同语义同时序
    req_rx: mpsc::Receiver<Incoming>,
    /// 桥回执表：按序号投递，主循环与控制线程的桥调用天然并发
    pending: PendingMap,
    seq: Arc<AtomicU64>,
    interrupt: Arc<AtomicBool>,
}

impl StdioHost {
    /// 启动 stdin 读线程 + 泵线程，返回主机句柄。
    /// interrupt 快路径：读线程看到 interrupt 立即置标志——主循环哪怕阻塞在
    /// LLM SSE 流上，打断也毫秒级生效（agent 循环逐块检查）。
    pub fn spawn() -> StdioHost {
        let (line_tx, line_rx) = mpsc::channel::<String>();
        let interrupt = Arc::new(AtomicBool::new(false));
        let flag = interrupt.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut lock = stdin.lock(); // 读线程全程唯一锁
            let mut line = String::new();
            loop {
                line.clear();
                match lock.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.contains("\"method\":\"interrupt\"") {
                    flag.store(true, Ordering::SeqCst);
                }
                if line_tx.send(trimmed.to_string()).is_err() {
                    break;
                }
            }
        });

        let (req_tx, req_rx) = mpsc::channel::<Incoming>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        // 泵线程：stdin 行的唯一路由器——回执按序号投递给等它的桥调用，
        // 宿主→插件请求走 deferred 通道，通知交主循环。无锁竞争、无饥饿。
        {
            let pending = pending.clone();
            std::thread::spawn(move || {
                while let Ok(raw) = line_rx.recv() {
                    let Ok(msg) = serde_json::from_str::<Value>(&raw) else {
                        continue;
                    };
                    if let Some(method) = msg.get("method").and_then(|m| m.as_str()).map(String::from) {
                        if method == "interrupt" {
                            continue; // 读线程已置标志；此处仅消费
                        }
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);
                        // 带 id = 宿主→插件的 RPC 请求（activate/run/dispose），交主循环；
                        // 无 id = 通知（onethu.event 进度流），同样交主循环消费
                        let incoming = match msg.get("id").cloned() {
                            Some(id) if !id.is_null() => Incoming::Request { id, method, params },
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
            });
        }

        StdioHost {
            req_rx,
            pending,
            seq: Arc::new(AtomicU64::new(1000)),
            interrupt,
        }
    }

    /// 控制线程用的独立桥句柄：与主循环共享回执表与打断标志——
    /// 控制命令（会话管理/导入导出）另线程执行，不被在跑的 chat 卡住。
    pub fn bridge(&self) -> BridgeHandle {
        BridgeHandle {
            pending: Arc::clone(&self.pending),
            seq: Arc::clone(&self.seq),
            interrupt: Arc::clone(&self.interrupt),
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
fn bridge_call(
    pending: &PendingMap,
    seq: &AtomicU64,
    ns: &str,
    method: &str,
    args: Value,
) -> Result<Value, String> {
    bridge_call_timeout(pending, seq, ns, method, args, BRIDGE_TIMEOUT.as_millis() as u64)
}

#[allow(clippy::too_many_arguments)]
fn bridge_call_timeout(
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
    let out = rx
        .recv_timeout(Duration::from_millis(timeout_ms))
        .unwrap_or_else(|_| Err(format!("宿主桥应答超时（{}s）", timeout_ms / 1000)));
    done.store(true, Ordering::Relaxed);
    out
}

impl Host for StdioHost {
    fn call(&mut self, ns: &str, method: &str, args: Value) -> Result<Value, String> {
        bridge_call(&self.pending, &self.seq, ns, method, args)
    }

    fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst)
    }

    fn reset_interrupt(&self) {
        self.interrupt.store(false, Ordering::SeqCst);
    }
}

/// 工作线程桥（chat / 控制命令共用）。reset_interrupt 只有 chat 工作线程
/// 在开局调用；控制命令绝不调它——纪律约定而非类型隔离。
pub struct BridgeHandle {
    pending: PendingMap,
    seq: Arc<AtomicU64>,
    interrupt: Arc<AtomicBool>,
}

impl BridgeHandle {
    /// 仅 chat 工作线程开局调用：清上一轮残留的打断标志
    pub fn reset_interrupt(&self) {
        self.interrupt.store(false, Ordering::SeqCst);
    }

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
