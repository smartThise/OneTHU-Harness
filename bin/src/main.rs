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
                    let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let input = params.get("input").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if command == "chat" {
                        // 长任务：主循环直跑（打断快路径靠读线程标志，主循环阻塞在 LLM 流上也毫秒级生效）
                        h.reset_interrupt();
                        let result = dispatch(&mut h, &emit, "chat", &input);
                        h.reply_ok(&id, result);
                    } else {
                        // 控制命令快速路（会话管理/导入导出/自检）：另线程以共享桥泵执行——
                        // chat 在跑时主循环阻塞在 dispatch 里，否则新会话/历史/导入导出会排到队尾形同死键。
                        // 桥泵按序号配对天然支持并发；与在跑 chat 的存储写竞态由「打断优先」约定兜底。
                        let bridge = h.bridge();
                        std::thread::spawn(move || {
                            let mut bh = bridge;
                            let result = dispatch(&mut bh, &StdioEmit, &command, &input);
                            bh.reply_ok(&id, result);
                        });
                    }
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
