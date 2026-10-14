//! 本机 MCP server（2026-10-13，OH 自我进化第一批）。
//!
//! 让**本机任何 MCP 客户端**（其他 agent、Claude Desktop、脚本）直接调用 OneTHU 的接口：
//! - 监听 127.0.0.1:OH_MCP_PORT（默认 8787），仅本机回环，无跨机暴露；
//! - Streamable-HTTP 形态：POST /mcp 收 JSON-RPC 2.0（initialize / tools/list / tools/call）；
//! - tools = OH 全部**无需交互确认**的工具（confirm=true 的写操作不上墙）；
//! - tools/call 经同一张桥回执表进 TS 应用（与 chat 工作线程同通道，天然并发安全）。
//!
//! 手搓最小 HTTP/1.1：零新依赖（harness 的依赖纪律）。

use crate::host::{Host, PendingMap};
use crate::tools;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct BridgeHost {
    pending: PendingMap,
    seq: Arc<AtomicU64>,
}

impl Host for BridgeHost {
    fn call(&mut self, ns: &str, method: &str, args: Value) -> Result<Value, String> {
        crate::host::bridge_call(&self.pending, &self.seq, ns, method, args)
    }
    fn interrupted(&self) -> bool {
        false
    }
    fn reset_interrupt(&self) {}
}

/// 总闸（2026-10-15 用户点名「OH MCP 是把我们自己暴露成 MCP」）：
/// `core::agent::chat` 每轮按设置同步一次（幂等，值没变不打印）。
/// 默认**关** —— 本机暴露必须由用户显式打开；旧行为是环境变量默认就开。
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_enabled(on: bool) {
    let was = ENABLED.swap(on, std::sync::atomic::Ordering::SeqCst);
    if was != on {
        eprintln!("[mcp] 本机 MCP server {}", if on { "已开启" } else { "已关闭（连接一律 403）" });
    }
}

pub fn is_enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::SeqCst)
}

/// listener 是否真的绑上了（`OH_MCP_PORT=0` 或绑定失败 → false）。
/// 与总闸分开报：用户容易把「闸门关着」当成「服务没起」，mcp_status 要把两件事说清。
static LISTENING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn is_listening() -> bool {
    LISTENING.load(std::sync::atomic::Ordering::SeqCst)
}

/// 端口口径**唯一处**（bin 与报告共用）：`OH_MCP_PORT`，解析失败/未设 → 8787。
/// 注意 0 在 bin 侧表示「连监听都不起」，这里照样把它读出来（报告要说实话）。
pub fn port() -> u16 {
    std::env::var("OH_MCP_PORT")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(8787)
}

pub fn endpoint() -> String {
    format!("http://127.0.0.1:{}/mcp", port())
}

fn tool_defs() -> Vec<(String, String, Value, bool)> {
    tools::all_tools()
        .into_iter()
        .map(|t| (t.name.to_string(), t.desc.to_string(), t.params, t.confirm))
        .collect()
}

/// 由 sidecar main 在 StdioHost::spawn 后调用（桥 Arc 共享给本线程组）。
///
/// 2026-10-15 起：**起监听 ≠ 对外可用**。listener 常驻绑定（Rust 标准库没有可移植的
/// 「温和收回端口」写法），真正决定接不接受请求的是下面那个闸门 —— 由用户在
/// OH 设置 → 插件 → OneTHU Harness → 高级配置里的「OH MCP 服务器」开关经 settings
/// 泵进来（core::agent::chat 每轮同步一次）。
pub fn spawn(pending: PendingMap, seq: Arc<AtomicU64>, port: u16) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[mcp] 127.0.0.1:{} 绑定失败（{}）——本机 MCP 关闭", port, e);
                return;
            }
        };
        eprintln!(
            "[mcp] 本机 MCP 监听已就位：POST http://127.0.0.1:{}/mcp（当前{}）",
            port,
            if is_enabled() { "已开启" } else { "关闭中：等设置开关，连接一律 403" }
        );
        LISTENING.store(true, std::sync::atomic::Ordering::SeqCst);
        for stream in listener.incoming() {
            if let Ok(stream) = stream {
                let host = BridgeHost { pending: pending.clone(), seq: seq.clone() };
                std::thread::spawn(move || serve(stream, &host));
            }
        }
    });

    fn serve(mut stream: TcpStream, host: &BridgeHost) {
        let _ = stream.set_nodelay(true);
        loop {
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        buf.push(byte[0]);
                        if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
                            break;
                        }
                        if buf.len() > 16384 {
                            return;
                        }
                    }
                }
            }
            let head = String::from_utf8_lossy(&buf);
            let is_post = head.starts_with("POST");
            let len = head
                .to_ascii_lowercase()
                .split("\r\n")
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; len];
            if len > 0 && stream.read_exact(&mut body).is_err() {
                return;
            }
            if !is_post {
                let _ = write_resp(&mut stream, 405, "{\"error\":\"POST /mcp\"}");
                continue;
            }
            /* 总闸（2026-10-15 用户点名）：OH 设置高级档那个「OH MCP 服务器」开关。
               关 = 立刻 403 断开 —— 外部客户端看到的是明确的"未开启"，而不是超时或空工具表。 */
            if !is_enabled() {
                let _ = write_resp(
                    &mut stream,
                    403,
                    "{\"error\":\"OH MCP 服务器未开启：请在 OneTHU 的设置 → 插件 → OneTHU Harness → 高级配置里打开\"}",
                );
                return;
            }
            let reply = handle_rpc(&String::from_utf8_lossy(&body), host);
            if write_resp(&mut stream, 200, &reply).is_err() {
                return;
            }
        }
    }

    fn write_resp(stream: &mut TcpStream, code: u16, body: &str) -> std::io::Result<()> {
        let reason = if code == 200 { "OK" } else { "ERR" };
        write!(
            stream,
            "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn handle_rpc(raw: &str, host: &BridgeHost) -> String {
        let v: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                return json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("解析失败：{e}")}})
                    .to_string()
            }
        };
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let params = v.get("params").cloned().unwrap_or(json!({}));
        let result: Value = match method.as_str() {
            "initialize" => json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "onethu", "version": env!("CARGO_PKG_VERSION")}
            }),
            "tools/list" => {
                let list: Vec<Value> = tool_defs()
                    .into_iter()
                    .filter(|t| !t.3)
                    .map(|t| json!({"name": t.0, "description": t.1, "inputSchema": t.2}))
                    .collect();
                json!({"tools": list})
            }
            "tools/call" => {
                let name = params.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                if tool_defs().into_iter().find(|t| t.0 == name).map(|t| t.3) == Some(true) {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"该工具需要交互确认，本机 MCP 不暴露"}})
                } else {
                    let mut bh = host.clone();
                    let mut ctx = crate::tools::Ctx { h: &mut bh };
                    match tools::execute(&mut ctx, &name, &args, true, &[]) {
                        Ok(out) => {
                            let text = match out {
                                crate::tools::ToolOut::Text(t) => t,
                                crate::tools::ToolOut::ConfirmNeeded { summary, .. } => summary,
                            };
                            json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":text}]}})
                        }
                        Err(e) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":e}}),
                    }
                }
            }
            other => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("未知方法：{other}")}}),
        };
        result.to_string()
    }
}
