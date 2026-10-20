//! OneTHU Harness stdio sidecar（桌面端形态）：宿主以独立进程拉起，
//! JSON-RPC over stdin/stdout。核心逻辑全在 onethu-harness-core（R1）。

use incoming::Incoming;
use onethu_harness_core::{activate_commands, dispatch_run, host::{RunEmit, StdioEmit, StdioHost}};

/// 从 core 重新导出，保持 use 树干净
mod incoming {
    pub use onethu_harness_core::host::Incoming;
}

fn main() {
    let mut h = StdioHost::spawn();
    // 本机 MCP server（2026-10-13）：127.0.0.1 回环，端口 OH_MCP_PORT（默认 8787），
    // OH_MCP_PORT=0 = 连监听都不起（开发期/偏执模式）。只暴露无需交互确认的工具；
    // tools/call 走同一张桥回执表。
    // 2026-10-15 改口径（用户点名「OH MCP 是把我们自己暴露成 MCP」）：**起监听 ≠ 对外可用**。
    // 这里只负责把 listener 常驻绑上；接不接受请求由 core::mcp_server 的闸门决定，
    // 而闸门由设置里那个「OH MCP 服务器」开关驱动（core::agent::chat 每轮同步，默认关）。
    {
        /* 端口口径在 core::mcp_server::port()（与 mcp_status 工具同一处读法，改一处即可） */
        let port = onethu_harness_core::mcp_server::port();
        if port > 0 {
            let (pending, seq) = h.bridge_arcs();
            onethu_harness_core::mcp_server::spawn(pending, seq, port);
        }
    }
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
            Incoming::Request { id, method, params, chat } => match method.as_str() {
                "activate" => {
                    eprintln!("[harness] activate：握手（设置在每次 run 时重新读取）");
                    h.reply_ok(&id, activate_commands());
                }
                "run" => {
                    // stdin 路由器已按输入顺序认领 chat 票；忙时在那里拒绝，不排队。
                    // 工作线程持票直到 dispatch 真正结束，时间经过或取消本身都不释放互斥。
                    let mut bridge = h.bridge(chat.as_ref());
                    std::thread::spawn(move || {
                        let emit = RunEmit::new(&StdioEmit, params.get("runId").and_then(serde_json::Value::as_str));
                        let result = dispatch_run(&mut bridge, &emit, &params);
                        // 先释放再回 RPC，调用者收到完成后即可安全发起下一轮。
                        drop(chat);
                        bridge.reply_ok(&id, result);
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
