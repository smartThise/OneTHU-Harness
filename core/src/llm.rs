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
        .timeout_read(std::time::Duration::from_secs(180))
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
let _ = h.call("log", "log", json!([format!("[harness] LLM \u{2190} {} ({} msgs, stream={})", cfg.model, messages.len(), cfg.stream)]));
    let resp = agent()
        .post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(http_err)?;

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
    let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
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
    Ok(Turn { content, reasoning: String::new(), tool_calls, usage })
}

/// SSE 流解析：逐块回调 delta/think；tool_calls 按 index 增量拼接
fn stream_turn(resp: ureq::Response, hooks: &mut Hooks, h: &dyn Host) -> Result<Turn, LlmError> {
    let reader = resp.into_reader();
    let mut br = std::io::BufReader::new(reader);
    let mut line = String::new();

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls: std::collections::BTreeMap<u64, ToolCall> = std::collections::BTreeMap::new();
    let mut usage: Option<ApiUsage> = None;

    loop {
        // 打断：SSE 逐块检查（R4）
        if h.interrupted() {
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
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            usage = serde_json::from_value::<ApiUsage>(u.clone()).ok();
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else { continue };
        if choice.get("finish_reason").and_then(|f| f.as_str()).is_some() {
            // finish_reason 出现后通常还有 usage 尾包，不 break，等 [DONE]/EOF
        }
        let Some(delta) = choice.get("delta") else { continue };
        if let Some(rc) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
            if !rc.is_empty() {
                reasoning.push_str(rc);
                (hooks.on_think)(rc);
            }
        }
        if let Some(dc) = delta.get("content").and_then(|c| c.as_str()) {
            if !dc.is_empty() {
                content.push_str(dc);
                (hooks.on_delta)(dc);
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
    Ok(Turn { content, reasoning, tool_calls: calls.into_values().collect(), usage })
}
