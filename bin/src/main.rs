//! OneTHU Harness stdio sidecar（桌面端形态）：宿主以独立进程拉起，
//! JSON-RPC over stdin/stdout。核心逻辑全在 onethu-harness-core（R1）。

use incoming::Incoming;
use onethu_harness_core::{activate_commands, dispatch, host::{StdioEmit, StdioHost}, Host};

/// 从 core 重新导出，保持 use 树干净
mod incoming {
    pub use onethu_harness_core::host::Incoming;
}

/// chat 串行护栏：同刻至多一个 chat 工作线程（重复发起立即拒绝而非排队）
static CHAT_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
                    let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let input = params.get("input").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    // R9 关键教训：主循环绝不能 inline dispatch——chat 一跑几十秒，
                    // 后续 run（新会话/历史/导入导出/打断后重试）全部排在队尾形同死键
                    //（此前「控制快速路」要在主循环读到请求后才生效，而主循环恰恰被 chat 占死；
                    //  sim 假服务器毫秒级返回掩盖了这一点）。现在所有 run 一律进工作线程，
                    // 主循环只做收发；chat 串行由 CHAT_IN_FLIGHT 护栏快速拒绝，不排队。
                    if command == "chat" && CHAT_IN_FLIGHT.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        h.reply_ok(
                            &id,
                            serde_json::json!({
                                "type": "chat", "ok": false,
                                "error": "已有对话在运行：请等它完成，或点打断后再发。"
                            }),
                        );
                        continue;
                    }
                    let bridge = h.bridge();
                    let cmd = command.clone();
                    std::thread::spawn(move || {
                        let mut bh = bridge;
                        if cmd == "chat" {
                            bh.reset_interrupt();
                        }
                        let result = dispatch(&mut bh, &StdioEmit, &cmd, &input);
                        if cmd == "chat" {
                            CHAT_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
                        }
                        bh.reply_ok(&id, result);
                    });
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
