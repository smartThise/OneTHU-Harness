//! LLM 客户端（OpenAI 兼容 /chat/completions）：SSE 流式 + 工具调用增量累计
//! + usage 取数；非流式回退；逐块回调驱动（R4 实时渲染），打断标志逐块检查。

use crate::host::Host;
use crate::usage::ApiUsage;
use serde_json::{json, Value};
use std::io::BufRead;

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 一轮 LLM 输出
pub struct Turn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<ApiUsage>,
}

pub struct Hooks<'a> {
    /// 回答增量（显示用）
    pub on_delta: Box<dyn FnMut(&str) + 'a>,
    /// 思考增量（deepseek-reasoner reasoning_content）
    pub on_think: Box<dyn FnMut(&str) + 'a>,
    /// 结构化运行日志（R7）：LLM 请求/响应/错误全程上日志事件
    pub on_log: Box<dyn FnMut(&str) + 'a>,
}

pub struct LlmError {
    pub message: String,
}

impl From<String> for LlmError {
    fn from(s: String) -> Self {
        LlmError { message: s }
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(15))
        // 单次 socket 读的空闲上限：首 token / 流间歇都受它约束——
        // 端点悬挂（错误模型名、假端点）最多 75s 必报错，而不是无限等
        .timeout_read(std::time::Duration::from_secs(75))
        .user_agent("OneTHU-Harness/0.1")
        .build()
}

fn http_err(e: ureq::Error) -> LlmError {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(400).collect();
            // OpenAI 风格错误尽量抽出 message
            let msg = serde_json::from_str::<Value>(&snippet)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .map(String::from)
                })
                .unwrap_or(snippet);
            LlmError { message: format!("LLM HTTP {code}：{msg}") }
        }
        ureq::Error::Transport(t) => LlmError { message: format!("LLM 网络错误：{t}") },
    }
}

/// 一轮对话（流式或非流式由 cfg.stream 决定）
pub fn chat_turn(
    h: &mut dyn Host,
    cfg: &crate::config::Config,
    messages: &[crate::session::Msg],
    tools: &[Value],
    hooks: &mut Hooks,
) -> Result<Turn, LlmError> {
    let url = format!("{}/chat/completions", cfg.base_url);
    let mut body = json!({
        "model": cfg.model,
        "messages": messages.iter().map(|m| m.to_value()).collect::<Vec<_>>(),
        "stream": cfg.stream,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
        body["tool_choice"] = json!("auto");
    }
    if cfg.stream {
        body["stream_options"] = json!({ "include_usage": true });
    }
    cfg.apply_thinking(&mut body);
    (hooks.on_log)(&format!(
        "→ LLM 请求 {} · msgs={} · stream={} · thinking={} · {}",
        cfg.model,
        messages.len(),
        cfg.stream,
        body.get("thinking").map(|t| t.to_string()).unwrap_or_else(|| "default".into()),
        url
    ));
    let t0 = std::time::Instant::now();
    let resp = match agent()
        .post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
    {
        Ok(r) => {
            (hooks.on_log)(&format!(
                "← LLM 应答头 HTTP {} · {}ms",
                r.status(),
                t0.elapsed().as_millis()
            ));
            r
        }
        Err(e) => {
            let err = http_err(e);
            (hooks.on_log)(&format!("✗ LLM 失败（{}ms）：{}", t0.elapsed().as_millis(), err.message));
            return Err(err);
        }
    };

    if cfg.stream {
        let ctype = resp.content_type().to_string();
        if ctype.contains("event-stream") {
            return stream_turn(resp, hooks, h);
        }
        // 端点不支持流式却没报错：按普通 JSON 解析
        let v: Value = resp.into_json().map_err(|e| LlmError { message: format!("响应解析失败：{e}") })?;
        return parse_completion(v);
    }
    let v: Value = resp.into_json().map_err(|e| LlmError { message: format!("响应解析失败：{e}") })?;
    parse_completion(v)
}

fn parse_completion(v: Value) -> Result<Turn, LlmError> {
    if let Some(err) = v.get("error") {
        return Err(LlmError { message: format!("LLM 错误：{}", err) });
    }
    let choice = v.get("choices").and_then(|c| c.get(0)).cloned().unwrap_or(Value::Null);
    let msg = choice.get("message").cloned().unwrap_or(Value::Null);
    let mut content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
    // 非流式也要剥 <think> 内联思考（GLM 等端点；剥离后进 reasoning，dock 思考链可见）
    let mut reasoning = msg
        .get("reasoning_content")
        .or_else(|| msg.get("reasoning"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    if let Some(a) = content.find("<think>") {
        if let Some(b) = content.find("</think>") {
            reasoning.push_str(&content[a + 7..b]);
            content = format!("{}{}", &content[..a], content[b + 8..].trim_start());
        }
    }
    let tool_calls = msg
        .get("tool_calls")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let f = tc.get("function")?;
                    Some(ToolCall {
                        id: tc.get("id").and_then(|i| i.as_str()).unwrap_or("call_0").to_string(),
                        name: f.get("name").and_then(|n| n.as_str())?.to_string(),
                        arguments: f.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let usage = v.get("usage").and_then(|u| serde_json::from_value::<ApiUsage>(u.clone()).ok());
    Ok(Turn { content, reasoning, tool_calls, usage })
}

/// SSE 流解析：逐块回调 delta/think；tool_calls 按 index 增量拼接
fn stream_turn(resp: ureq::Response, hooks: &mut Hooks, h: &dyn Host) -> Result<Turn, LlmError> {
    // 参考 DeepSeek Harness 的块式组装：思考先行（reasoning_content / <think> 内联双兼容），
    // tool_calls 按 index 增量累积；事件按时间窗批处理——逐 token 发事件会把 IPC 与日志刷爆。
    let reader = resp.into_reader();
    let mut br = std::io::BufReader::new(reader);
    let mut line = String::new();

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls: std::collections::BTreeMap<u64, ToolCall> = std::collections::BTreeMap::new();
    let mut usage: Option<ApiUsage> = None;

    // <think> 路由态：GLM 等端点把思考以 <think>…</think> 内联在 content 里
    let mut raw = String::new();
    let mut scan = 0usize;
    let mut in_think = false;

    // 事件批处理：首个片段立即出，之后每 ~120ms 或 >400 字节一刷
    let mut delta_buf = String::new();
    let mut think_buf = String::new();
    let mut last_flush = std::time::Instant::now();
    let mut first_out = true;
    const BATCH_MS: u128 = 120;

    macro_rules! flush {
        () => {
            if !delta_buf.is_empty() {
                (hooks.on_delta)(&delta_buf.clone());
                delta_buf.clear();
            }
            if !think_buf.is_empty() {
                (hooks.on_think)(&think_buf.clone());
                think_buf.clear();
            }
            last_flush = std::time::Instant::now();
        };
    }

    // 把 raw[scan..] 里已确定路由的部分吐出；hold 字节尾巴防标签跨 chunk 撕裂
    macro_rules! pump {
        ($final:expr) => {{
            let hold = if $final { 0 } else { "</think>".len() };
            loop {
                if raw.len() <= scan + hold {
                    break;
                }
                let rest = &raw[scan..];
                match rest.find(if in_think { "</think>" } else { "<think>" }) {
                    Some(0) => {
                        in_think = !in_think;
                        scan += if in_think { "<think>".len() } else { "</think>".len() };
                    }
                    Some(i) => {
                        let seg = &rest[..i];
                        if in_think {
                            reasoning.push_str(seg);
                            think_buf.push_str(seg);
                        } else {
                            content.push_str(seg);
                            delta_buf.push_str(seg);
                        }
                        scan += i;
                    }
                    None => {
                        let mut take = rest.len() - hold;
                        while take > 0 && !raw.is_char_boundary(scan + take) {
                            take -= 1;
                        }
                        let seg = &rest[..take];
                        if in_think {
                            reasoning.push_str(seg);
                            think_buf.push_str(seg);
                        } else {
                            content.push_str(seg);
                            delta_buf.push_str(seg);
                        }
                        scan += take;
                        break;
                    }
                }
                let elapsed = last_flush.elapsed().as_millis();
                if first_out || elapsed >= BATCH_MS
                    || delta_buf.len() > 400
                    || think_buf.len() > 400
                {
                    flush!();
                    first_out = false;
                }
            }
        }};
    }

    loop {
        // 打断：SSE 逐块检查（R4）
        if h.interrupted() {
            flush!();
            return Ok(Turn { content, reasoning, tool_calls: calls.into_values().collect(), usage });
        }
        line.clear();
        match br.read_line(&mut line) {
            Ok(0) => break, // 流结束
            Ok(_) => {}
            Err(e) => return Err(LlmError { message: format!("流读取中断：{e}") }),
        }
        let t = line.trim();
        if t.is_empty() || t.starts_with(':') {
            continue;
        }
        let Some(data) = t.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else { continue };
        // 端点把错误塞进 200 SSE 流（OpenAI 兼容变体）：data: {"error": {...}}
        if let Some(err) = chunk.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .map(String::from)
                .unwrap_or_else(|| err.to_string());
            return Err(LlmError { message: format!("LLM 错误：{msg}") });
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            usage = serde_json::from_value::<ApiUsage>(u.clone()).ok();
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else { continue };
        if choice.get("finish_reason").and_then(|f| f.as_str()).is_some() {
            // finish_reason 出现后通常还有 usage 尾包，不 break，等 [DONE]/EOF
        }
        let Some(delta) = choice.get("delta") else { continue };
        // 思考：DeepSeek 系 reasoning_content / 部分网关 reasoning 字段
        if let Some(rc) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(|c| c.as_str())
        {
            if !rc.is_empty() {
                reasoning.push_str(rc);
                think_buf.push_str(rc);
            }
        }
        // 正文：进 <think> 路由（内联思考不再混进回答）
        if let Some(dc) = delta.get("content").and_then(|c| c.as_str()) {
            if !dc.is_empty() {
                raw.push_str(dc);
                pump!(false);
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let entry = calls.entry(idx).or_insert_with(|| ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                    if !id.is_empty() {
                        entry.id = id.to_string();
                    }
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                        if !n.is_empty() {
                            entry.name = n.to_string();
                        }
                    }
                    if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                        entry.arguments.push_str(a);
                    }
                }
            }
        }
    }
    // 收尾：清掉撕裂尾巴与未闭合的 <think>
    pump!(true);
    flush!();
    Ok(Turn { content, reasoning, tool_calls: calls.into_values().collect(), usage })
}

/// 空轮次兜底：EOF 无任何输出且非打断 → 视为端点异常（模型名错误常此相），报错而不是让 UI 干等
pub fn ensure_nonempty(turn: Turn, was_interrupted: bool) -> Result<Turn, LlmError> {
    if turn.content.is_empty()
        && turn.reasoning.is_empty()
        && turn.tool_calls.is_empty()
        && !was_interrupted
    {
        return Err(LlmError { message: "模型无任何输出——请检查「模型」名称与 API Endpoint 是否正确".into() });
    }
    Ok(turn)
}
