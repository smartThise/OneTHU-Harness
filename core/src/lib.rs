//! OneTHU Harness 核心（宿主无关）：agent 主控循环、校园工具集、OpenAI 兼容
//! 流式客户端、多会话与用量。只面向 `host.rs` 的 `Host`/`Emit` 两个 trait——
//! stdio sidecar（bin/）与 App 内嵌宿主（OneTHU src-tauri）各自实现。

pub mod agent;
pub mod config;
pub mod host;
pub mod llm;
pub mod session;
pub mod tools;
pub mod usage;

pub use host::{Emit, Host};

use serde_json::{json, Value};

/// activate 握手应答：管理页命令清单（chat 带 dock 标记 → 宿主对话面板）
pub fn activate_commands() -> Value {
    json!({
        "commands": [
            { "id": "chat", "title": "对话", "inputLabel": "对 Harness 说",
              "inputPlaceholder": "例：明天图书馆哪有空座？", "dock": true },
            { "id": "new_session", "title": "新建会话" },
            { "id": "list_sessions", "title": "会话列表" },
            { "id": "switch_session", "title": "切换会话", "inputLabel": "会话 id" },
            { "id": "delete_session", "title": "删除会话", "inputLabel": "会话 id" },
            { "id": "export_session", "title": "导出会话 JSON", "inputLabel": "会话 id（留空=当前）" },
            { "id": "import_session", "title": "导入会话 JSON", "inputLabel": "粘贴会话 JSON" },
            { "id": "usage_report", "title": "用量统计" },
            { "id": "selftest", "title": "自检" }
        ]
    })
}

/// run 命令分发（stdio 与内嵌宿主共用）
pub fn dispatch(h: &mut dyn Host, emit: &dyn Emit, command: &str, input: &str) -> Value {
    match command {
        "chat" => agent::chat(h, emit, input),
        "new_session" => agent::new_session(h),
        "list_sessions" => agent::list_sessions(h),
        "switch_session" => agent::switch_session(h, input.trim()),
        "delete_session" => agent::delete_session(h, input.trim()),
        "export_session" => agent::export_session(h, input.trim()),
        "import_session" => agent::import_session(h, input),
        "usage_report" => agent::usage_report(h),
        "selftest" => agent::selftest(),
        "" => json!({ "type": "chat", "ok": false, "error": "缺少命令名" }),
        other => json!({ "type": "chat", "ok": false, "error": format!("未知命令：{other}") }),
    }
}
