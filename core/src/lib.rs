//! OneTHU Harness 核心（宿主无关）：agent 主控循环、校园工具集、OpenAI 兼容
//! 流式客户端、多会话与用量。只面向 `host.rs` 的 `Host`/`Emit` 两个 trait——
//! stdio sidecar（bin/）与 App 内嵌宿主（OneTHU src-tauri）各自实现。

pub mod agent;
pub mod config;
pub mod host;
pub mod llm;
pub mod madmodel;
pub mod mcp;
pub mod mcp_server;
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

/* ═══════════════ 附件（run 的可选 attachments；2026-10-11 多模态） ═══════════════
 * 链路：前端 `lib/attach.ts` 把可读到的图片字节读成 base64（只带 `image/…` 且 ≤6MB）
 * → `im/oh.ts` 塞进 run 的 params.attachments → 宿主原样透传（sidecar / harness_embed）
 * → 这里解析 → `agent::chat` 按「多模态开关」决定进图像模态还是退化成文字说明。
 * ⚠️ 本文件的块注释里**不许出现「斜杠 + 星号」这两个字符连着写**（Rust 块注释可嵌套，
 *    会被当成新注释开始，整段注释再也不闭合 → E0758，2026-10-11 真踩过）。 */

/// 单条消息最多随图几张（超出的按文字说明带出，**绝不静默丢件**）
pub const MAX_ATTACH_IMAGES: usize = 4;
/// 单张图 base64 长度上限：≈6MB 原始字节（base64 膨胀 4/3）再留 padding 余量。
/// 与前端 `lib/attach.ts` 的 ≤6MB 原始字节过滤口径对齐，避免两侧判断打架。
pub const MAX_ATTACH_B64: usize = 8 * 1024 * 1024 + 8192;

/// run 的 `attachments[]` 条目（JS 侧形状：`{name, mime, dataBase64}`）
#[derive(Clone, Debug, Default)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub data_base64: String,
}

/// 解析 RPC params 里的 `attachments`。容忍 snake_case（`data_base64`）与大小写；
/// 空 base64 的条目没有意义，直接丢弃（前端不会发，纯防御）。
pub fn parse_attachments(v: Option<&Value>) -> Vec<Attachment> {
    let Some(arr) = v.and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|it| {
            let s = |k: &str| it.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
            let data_base64 = {
                let a = s("dataBase64");
                if a.is_empty() { s("data_base64") } else { a }
            };
            if data_base64.is_empty() {
                return None;
            }
            Some(Attachment { name: s("name"), mime: s("mime"), data_base64 })
        })
        .collect()
}

/// 文本 + 附件 → (进模型的文本, 进模型的图片 data URL 列表)。
///
/// 规则（task-12 定案）：
/// - `mime` 以 `image/` 开头 **且** 多模态开着 **且** 不超张数/体积上限 → 进 images；
/// - 其余一律拼成 `[附件] 名称（mime, N KB）：原因` 追加到文本末尾——**绝不静默丢件**；
/// - 非图片附件的**字节不会被上传**（只有名字/类型/大小进文本），与既有口径一致。
pub fn split_attachments(input: &str, atts: &[Attachment], multimodal: bool) -> (String, Vec<String>) {
    if atts.is_empty() {
        return (input.to_string(), Vec::new());
    }
    let mut text = input.to_string();
    let mut images: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for a in atts {
        let name = if a.name.trim().is_empty() { "未命名附件".to_string() } else { a.name.trim().to_string() };
        let mime = a.mime.trim().to_ascii_lowercase();
        let kb = (a.data_base64.len() as f64 * 0.75 / 1024.0).round() as u64;
        let mime_disp = if mime.is_empty() { "未知类型" } else { mime.as_str() };
        if !mime.starts_with("image/") {
            notes.push(format!(
                "[附件] {name}（{mime_disp}, {kb}KB）：非图片，未按图像发送（大文件请先用「转存云盘」，或把内容贴进对话）"
            ));
        } else if !multimodal {
            notes.push(format!(
                "[附件] {name}（{mime_disp}, {kb}KB）：多模态未开启，未按图像发送（可在 设置 → 插件 → OneTHU Harness 打开「多模态」）"
            ));
        } else if images.len() >= MAX_ATTACH_IMAGES {
            notes.push(format!(
                "[附件] {name}（{mime_disp}, {kb}KB）：本条消息图片数已达上限 {MAX_ATTACH_IMAGES} 张，这张未随图发送"
            ));
        } else if a.data_base64.len() > MAX_ATTACH_B64 {
            notes.push(format!(
                "[附件] {name}（{mime_disp}, {kb}KB）：图片过大（超过 ~6MB），未随图发送——请压缩后重发，或先转存云盘"
            ));
        } else {
            images.push(format!("data:{mime};base64,{}", a.data_base64));
        }
    }
    if !notes.is_empty() {
        text.push_str("\n\n");
        text.push_str(&notes.join("\n"));
    }
    (text, images)
}

/// run 命令分发（stdio 与内嵌宿主共用）
pub fn dispatch(
    h: &mut dyn Host,
    emit: &dyn Emit,
    command: &str,
    input: &str,
    attachments: &[Attachment],
) -> Value {
    match command {
        "chat" => agent::chat(h, emit, input, attachments),
        "new_session" => agent::new_session(h),
        "list_sessions" => agent::list_sessions(h),
        "switch_session" => agent::switch_session(h, input.trim()),
        "delete_session" => agent::delete_session(h, input.trim()),
        "export_session" => agent::export_session(h, input.trim()),
        "import_session" => agent::import_session(h, input),
        "restore_main" => agent::restore_main(h, input),
        "usage_report" => agent::usage_report(h),
        "selftest" => agent::selftest(),
        "" => json!({ "type": "chat", "ok": false, "error": "缺少命令名" }),
        other => json!({ "type": "chat", "ok": false, "error": format!("未知命令：{other}") }),
    }
}

/// run 的**统一入口**：从 RPC params 里取 `command` / `input` / `attachments` 再分发。
/// sidecar（bin/src/main.rs）与内嵌宿主（src-tauri/harness_embed.rs）都调这一个——
/// 参数解析只此一处，两个宿主不再各写一遍（也就不会再出现「一个宿主接了新字段、另一个没接」）。
pub fn dispatch_run(h: &mut dyn Host, emit: &dyn Emit, params: &Value) -> Value {
    let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let input = params.get("input").and_then(|v| v.as_str()).unwrap_or("");
    let atts = parse_attachments(params.get("attachments"));
    dispatch(h, emit, command, input, &atts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(name: &str, b64_len: usize) -> Attachment {
        Attachment { name: name.into(), mime: "image/jpeg".into(), data_base64: "A".repeat(b64_len) }
    }

    #[test]
    fn parse_attachments_shape() {
        let p = json!({"attachments": [
            {"name": "a.jpg", "mime": "image/jpeg", "dataBase64": "AAA"},
            {"name": "b.pdf", "mime": "application/pdf", "data_base64": "BBB"},
            {"name": "空", "mime": "image/png", "dataBase64": ""},
            "垃圾条目"
        ]});
        let a = parse_attachments(p.get("attachments"));
        assert_eq!(a.len(), 2, "空 base64 与非对象条目应被丢弃");
        assert_eq!(a[0].name, "a.jpg");
        assert_eq!(a[1].data_base64, "BBB");
        assert!(parse_attachments(None).is_empty());
    }

    /// 多模态关（默认）：图片一颗字节都不进请求体，退化成文字说明。
    #[test]
    fn blind_mode_degrades_to_text() {
        let (text, images) = split_attachments("看这张", &[img("手写.jpg", 1024)], false);
        assert!(images.is_empty(), "关开关时不许有任何图");
        assert!(text.starts_with("看这张"), "原文必须保留在最前");
        assert!(text.contains("[附件] 手写.jpg（image/jpeg, 1KB）：多模态未开启"), "要带可操作的原因：{text}");
    }

    /// 多模态开 + 图片：进 images（data URL），文本不追加废话。
    #[test]
    fn multimodal_image_becomes_part() {
        let (text, images) = split_attachments("看这张", &[img("手写.jpg", 1024)], true);
        assert_eq!(images.len(), 1);
        assert!(images[0].starts_with("data:image/jpeg;base64,"));
        assert_eq!(text, "看这张", "成功随图时不该往正文塞附件说明");
    }

    /// 非图片：无论开关如何都只给文字（字节不上传）。
    #[test]
    fn non_image_never_sent_as_image() {
        let pdf = Attachment { name: "作业.pdf".into(), mime: "application/pdf".into(), data_base64: "QQ".into() };
        let (text, images) = split_attachments("提交", &[pdf], true);
        assert!(images.is_empty());
        assert!(text.contains("非图片，未按图像发送"));
    }

    /// 上限：第 5 张退化成文字；超大图退化成文字（都要给出原因，不能静默）。
    #[test]
    fn limits_degrade_with_reason() {
        let four: Vec<Attachment> = (0..4).map(|i| img(&format!("p{i}.jpg"), 100)).collect();
        let mut five = four.clone();
        five.push(img("p5.jpg", 100));
        let (text, images) = split_attachments("", &five, true);
        assert_eq!(images.len(), MAX_ATTACH_IMAGES);
        assert!(text.contains("图片数已达上限 4 张"), "{text}");

        let (t2, i2) = split_attachments("", &[img("巨图.jpg", MAX_ATTACH_B64 + 1)], true);
        assert!(i2.is_empty());
        assert!(t2.contains("图片过大"), "{t2}");
    }

    /// 纯图无文字：文本保持空串（agent 侧会补一条占位，见 chat）。
    #[test]
    fn image_only_keeps_empty_text() {
        let (text, images) = split_attachments("", &[img("a.jpg", 10)], true);
        assert_eq!(text, "");
        assert_eq!(images.len(), 1);
    }
}
