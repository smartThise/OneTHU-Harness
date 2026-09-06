//! OneTHU Harness stdio sidecar（桌面端形态）：宿主以独立进程拉起，
//! JSON-RPC over stdin/stdout。核心逻辑全在 onethu-harness-core（R1）。

use incoming::Incoming;
use onethu_harness_core::{activate_commands, dispatch, host::{StdioEmit, StdioHost}, Host};

/// 从 core 重新导出，保持 use 树干净
mod incoming {
    pub use onethu_harness_core::host::Incoming;
}

fn main() {
    let mut h = StdioHost::spawn();
    let emit = StdioEmit;
    eprintln!("[harness] sidecar 已启动，等待 activate 握手");
    loop {
        // 补处理 call() 期间暂存的宿主请求（如 run 进行中到达的 dispose）
        for (id, method, _params) in h.take_deferred() {
            if method == "dispose" {
                h.reply_ok(&id, serde_json::Value::Null);
                std::process::exit(0);
            }
        }
        match h.poll() {
            Incoming::Request { id, method, params } => match method.as_str() {
                "activate" => {
                    eprintln!("[harness] activate：握手（设置在每次 run 时重新读取）");
                    h.reply_ok(&id, activate_commands());
                }
                "run" => {
                    h.reset_interrupt();
                    let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let input = params.get("input").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let result = dispatch(&mut h, &emit, &command, &input);
                    h.reply_ok(&id, result);
                }
                "dispose" => {
                    eprintln!("[harness] dispose：退出");
                    h.reply_ok(&id, serde_json::Value::Null);
                    std::process::exit(0);
                }
                "interrupt" => {}
                other => {
                    eprintln!("[harness] 未知方法：{other}");
                    if !id.is_null() {
                        h.reply_err(&id, &format!("未知方法：{other}"));
                    }
                }
            },
            Incoming::Notify { .. } => {}
            Incoming::Response { .. } => {}
        }
    }
}
