//! OneTHU Harness——大模型驱动的清华校园助手（Rust 骨干插件，课程 R1 合规形态）。
//!
//! 进程模型：被 OneTHU 宿主以 sidecar 方式拉起，stdio 上跑 JSON-RPC：
//!   宿主 → 插件：activate / run / dispose（请求）、interrupt（通知）
//!   插件 → 宿主：onethu.call（请求，权限门禁同 JS 插件）、progress / log（通知）
//! agent 主控循环（LLM 调用、工具编排、token 统计、会话管理）全在本进程内，
//! webview 侧只有宿主胶水。协议细节见 docs/OneTHU-插件与接口指南.md §八。

mod agent;
mod config;
mod conn;
mod llm;
mod session;
mod tools;
mod usage;

use conn::{Conn, Incoming};
use serde_json::{json, Value};

fn main() {
    let mut seq: u64 = 1000;
    let mut c = Conn::spawn_reader();
    eprintln!("[harness] sidecar 已启动，等待 activate 握手");
    loop {
        // 补处理 call() 期间暂存的宿主请求（如 run 进行中到达的 dispose）
        let deferred = c.take_deferred();
        for (id, method, _params) in deferred {
            if method == "dispose" {
                Conn::reply_ok(&id, Value::Null);
                std::process::exit(0);
            }
        }
        match c.poll() {
            Incoming::Request { id, method, params } => handle(&mut c, &mut seq, id, method, params),
            Incoming::Notify { .. } => {}
            Incoming::Response { .. } => {} // 主循环层不期待应答；忽略
        }
    }
}

fn handle(c: &mut Conn, seq: &mut u64, id: Value, method: String, params: Value) {
    match method.as_str() {
        "activate" => {
            eprintln!("[harness] activate：握手（设置会在每次 run 时重新读取）");
            Conn::reply_ok(
                &id,
                json!({
                    "commands": [
                        { "id": "chat", "title": "对话", "inputLabel": "对 Harness 说", "inputPlaceholder": "例：明天图书馆哪有空座？", "dock": true },
                        { "id": "new_session", "title": "新建会话" },
                        { "id": "list_sessions", "title": "会话列表" },
                        { "id": "switch_session", "title": "切换会话", "inputLabel": "会话 id" },
                        { "id": "delete_session", "title": "删除会话", "inputLabel": "会话 id" },
                        { "id": "export_session", "title": "导出会话 JSON", "inputLabel": "会话 id（留空=当前）" },
                        { "id": "import_session", "title": "导入会话 JSON", "inputLabel": "粘贴会话 JSON", "inputPlaceholder": "{\"id\":\"…\",\"messages\":[…]}", "textarea": true },
                        { "id": "usage_report", "title": "用量统计" },
                        { "id": "selftest", "title": "自检" }
                    ]
                }),
            );
        }
        "run" => {
            conn::interrupt_reset();
            let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let input = params.get("input").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let result = dispatch(c, seq, &command, &input);
            Conn::reply_ok(&id, result);
        }
        "dispose" => {
            eprintln!("[harness] dispose：退出");
            Conn::reply_ok(&id, Value::Null);
            std::process::exit(0);
        }
        "interrupt" => {} // 读线程已置标志
        other => {
            eprintln!("[harness] 未知方法：{other}");
            if !id.is_null() {
                Conn::reply_err(&id, &format!("未知方法：{other}"));
            }
        }
    }
}

fn dispatch(c: &mut Conn, seq: &mut u64, command: &str, input: &str) -> Value {
    match command {
        "chat" => agent::chat(c, seq, input),
        "new_session" => agent::new_session(c, seq),
        "list_sessions" => agent::list_sessions(c, seq),
        "switch_session" => agent::switch_session(c, seq, input.trim()),
        "delete_session" => agent::delete_session(c, seq, input.trim()),
        "export_session" => agent::export_session(c, seq, input.trim()),
        "import_session" => agent::import_session(c, seq, input),
        "usage_report" => agent::usage_report(c, seq),
        "selftest" => agent::selftest(),
        "" => json!({ "type": "chat", "ok": false, "error": "缺少命令名" }),
        other => json!({ "type": "chat", "ok": false, "error": format!("未知命令：{other}") }),
    }
}
