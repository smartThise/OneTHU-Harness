//! 宿主连接层：stdin 读取线程 + JSON-RPC 消息分发 + onethu.call 请求配对。
//!
//! 线程模型（避开 std 锁不可重入的死锁坑，见接口指南 §8.3）：
//! - 读线程独占 stdin，逐行解析后推入 mpsc 通道；「interrupt」在读取侧立即
//!   置位全局打断标志——即使主循环正阻塞在 LLM SSE 流上，打断也毫秒级生效；
//! - 主循环（含 agent 工具循环）只从通道取消息。onethu.call 的应答按请求 id
//!   配对；等待期间抵达的其他消息按类型暂存，绝不丢失。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

/// 全局打断标志（R4）：读线程置位，agent 循环与 SSE 流逐块检查
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn interrupt_reset() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

pub fn interrupt_take() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

/// 主循环可见的消息
pub enum Incoming {
    /// 宿主请求（activate / run / dispose）
    Request { id: Value, method: String, params: Value },
    /// 宿主通知
    Notify { method: String, params: Value },
    /// onethu.call 的应答（call() 内部消化，主循环层见不到）
    Response { id: u64, ok: bool, value: Value },
}

pub struct Conn {
    rx: mpsc::Receiver<String>,
    /// call() 等待期间抵达的其他应答（防御性暂存，供后续 call 认领）
    orphans: HashMap<u64, Result<Value, String>>,
    /// call() 等待期间抵达的宿主请求（如 dispose）：暂存，主循环事后补处理
    deferred: Vec<(Value, String, Value)>,
}

impl Conn {
    /// 启动 stdin 读线程，返回连接句柄
    pub fn spawn_reader() -> Conn {
        let (tx, rx) = mpsc::channel::<String>();
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
                // 快路径：interrupt 不进队列直接置标志（主循环可能正卡在 SSE 读上）
                if trimmed.contains("\"method\":\"interrupt\"") {
                    INTERRUPTED.store(true, Ordering::SeqCst);
                }
                if tx.send(trimmed.to_string()).is_err() {
                    break;
                }
            }
        });
        Conn { rx, orphans: HashMap::new(), deferred: Vec::new() }
    }

    /// 写一行 JSON 到 stdout（唯一出口）
    pub fn send(v: &Value) {
        use std::io::Write;
        let mut s = serde_json::to_string(v).unwrap_or_default();
        s.push('\n');
        let out = std::io::stdout();
        let mut h = out.lock();
        let _ = h.write_all(s.as_bytes());
        let _ = h.flush();
    }

    pub fn notify(method: &str, params: Value) {
        Self::send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn reply_ok(id: &Value, result: Value) {
        Self::send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn reply_err(id: &Value, message: &str) {
        Self::send(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": message } }));
    }

    /// 取下一条可见消息（阻塞）。
    pub fn poll(&mut self) -> Incoming {
        loop {
            let raw = match self.rx.recv() {
                Ok(m) => m,
                Err(_) => std::process::exit(0), // stdin 关闭 = 宿主已走，自行了断
            };
            let Ok(msg) = serde_json::from_str::<Value>(&raw) else {
                continue; // 非 JSON 行忽略
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
            // onethu.call 应答
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

    /// 发起一次 onethu.call 并等待配对应答。等待期间：
    /// - 其他 id 的应答 → 暂存 orphans（供后续 call 认领）；
    /// - 宿主请求 → 暂存 deferred（主循环在 run 结束后补处理）；
    /// - 通知 → 忽略（interrupt 标志已由读线程置位）。
    pub fn call(&mut self, seq: &mut u64, ns: &str, method: &str, args: Value) -> Result<Value, String> {
        *seq += 1;
        let rid = *seq;
        Self::send(&json!({
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

    /// 主循环补处理 call() 期间暂存的宿主请求
    pub fn take_deferred(&mut self) -> Vec<(Value, String, Value)> {
        std::mem::take(&mut self.deferred)
    }
}

fn value_to_msg(v: &Value) -> String {
    match v.as_str() {
        Some(s) => s.to_string(),
        None => v.to_string(),
    }
}
