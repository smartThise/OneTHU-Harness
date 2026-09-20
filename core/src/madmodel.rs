//! 清华 MadModel（校园网免费 DeepSeek）token 的 Rust 侧消费常量。
//!
//! token 的签发/续期全权由宿主 JS 泵负责（apps/desktop/src/state/madmodel.ts）：
//! 那边有成熟的 transport（webvpn 包装、SSO 重放、逐跳 cookie），Rust 不重复造轮子。
//! Rust 只通过 settings.get()（R3 每次 run 实时读取）拿 `madmodelToken`，
//! 与 `BASE`/`MODEL` 一起构成免费档的完整请求参数。

/// OpenAI 兼容端点（chat/completions 拼接用）
pub const BASE: &str = "https://madmodel.cs.tsinghua.edu.cn/v1";
/// 免费档模型（150k 上下文）
pub const MODEL: &str = "DeepSeek-V4-Flash-0731";
