//! 宿主边界抽象：核心 crate 只面向 `Host`（数据调用 + 打断）与 `Emit`（通知）。
//! - stdio sidecar（bin/）：`StdioHost` + `StdioEmit`——JSON-RPC over stdin/stdout；
//! - App 内嵌（OneTHU src-tauri）：`TauriHost` + `TauriEmit`——事件桥到 webview 门面。
//! 两个实现都不在核心 crate 内，核心保持平台无关（Android 可编译）。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

/// 数据面：onethu.* 原子调用（权限门禁在宿主侧）+ 打断标志
pub trait Host {
    fn call(&mut self, ns: &str, method: &str, args: Value) -> Result<Value, String>;
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

pub struct StdioHost {
    rx: mpsc::Receiver<String>,
    orphans: HashMap<u64, Result<Value, String>>,
    deferred: Vec<(Value, String, Value)>,
    seq: u64,
    interrupt: Arc<AtomicBool>,
}

/// 通知到达时的回调：宿主主循环用它收「call 期间抵达的宿主请求」
pub type Deferred = Vec<(Value, String, Value)>;

impl StdioHost {
    /// 启动 stdin 读线程，返回主机句柄。
    /// interrupt 快路径：读线程看到 interrupt 立即置标志——主循环哪怕阻塞在
    /// LLM SSE 流上，打断也毫秒级生效（agent 循环逐块检查）。
    pub fn spawn() -> StdioHost {
        let (tx, rx) = mpsc::channel::<String>();
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
                if tx.send(trimmed.to_string()).is_err() {
                    break;
                }
            }
        });
        StdioHost { rx, orphans: HashMap::new(), deferred: Vec::new(), seq: 1000, interrupt }
    }

    pub fn reply_ok(&self, id: &Value, result: Value) {
        send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn reply_err(&self, id: &Value, message: &str) {
        send(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": message } }));
    }

    /// 取下一条可见消息（阻塞）。
    pub fn poll(&mut self) -> Incoming {
        loop {
            let raw = match self.rx.recv() {
                Ok(m) => m,
                Err(_) => std::process::exit(0), // stdin 关闭 = 宿主已走
            };
            let Ok(msg) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if let Some(method) = msg.get("method").and_then(|m| m.as_str()).map(String::from) {
                if method == "interrupt" {
                    continue; // 读线程已置标志；此处仅消费
                }
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                match msg.get("id").cloned() {
                    Some(id) if !id.is_null() => return Incoming::Request { id, method, params },
                    _ => return Incoming::Notify { method, params },
                }
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
                let ok = out.is_ok();
                return Incoming::Response { id, ok, value: out.unwrap_or(Value::Null) };
            }
        }
    }

    /// call() 期间暂存的宿主请求（如 dispose），主循环事后补处理
    pub fn take_deferred(&mut self) -> Deferred {
        std::mem::take(&mut self.deferred)
    }
}

impl Host for StdioHost {
    fn call(&mut self, ns: &str, method: &str, args: Value) -> Result<Value, String> {
        self.seq += 1;
        let rid = self.seq;
        send(&json!({
            "jsonrpc": "2.0", "id": rid,
            "method": "onethu.call",
            "params": { "ns": ns, "method": method, "args": args }
        }));
        loop {
            if let Some(out) = self.orphans.remove(&rid) {
                return out;
            }
            match self.poll() {
                Incoming::Response { id, ok, value } => {
                    if id == rid {
                        return if ok { Ok(value) } else { Err(value_to_msg(&value)) };
                    }
                    self.orphans.insert(id, if ok { Ok(value) } else { Err(value_to_msg(&value)) });
                }
                Incoming::Request { id, method, params } => self.deferred.push((id, method, params)),
                Incoming::Notify { .. } => {}
            }
        }
    }

    fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst)
    }

    fn reset_interrupt(&self) {
        self.interrupt.store(false, Ordering::SeqCst);
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
