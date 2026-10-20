//! LLM 客户端（OpenAI 兼容 /chat/completions）：SSE 流式 + 工具调用增量累计
//! + usage 取数；非流式回退；逐块回调驱动（R4 实时渲染），打断标志逐块检查。

use crate::host::Host;
use crate::usage::ApiUsage;
use serde_json::{json, Value};
#[path = "llm_transport.rs"]
mod transport;
use transport::Failure;

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

/// 一轮对话（流式或非流式由 cfg.stream 决定）
pub fn chat_turn(
    h: &mut dyn Host,
    cfg: &crate::config::Config,
    messages: &[crate::session::Msg],
    tools: &[Value],
    hooks: &mut Hooks,
) -> Result<Turn, LlmError> {
    // MadModel 免费档预检（2026-09-19 兜底定案）：校外且无自费 Key → 不发请求，
    // 直接给出引导；免费档 token 尚未就位（泵首签还没完成）→ 提示稍候。
    // 2026-10-12 修正：token 已就位时不再因「可达性标记为 0」提前拒绝——标记只来自探针，
    // 校外 + webvpn 会话仍有效的情形应真实发一次请求，由下方 307/HTML 分类给出准确文案。
    if url_contains_madmodel(cfg) {
        if cfg.api_key.is_empty() && !cfg.madmodel_ok {
            return Err(LlmError {
                message: "清华免费 DeepSeek 当前不可用：直连被校园网 IP 门禁拦截，且 webvpn 包装通道未就绪。\n                          · 在 OneTHU 里登录一次清华账号（建立 webvpn 会话后会自动走包装通道）；\n                          · 或连校园网/学校 VPN（EasyConnect）；\n                          · 或在 插件页 → OneTHU Harness → 设置 里切到「自费 API」。"
                    .into(),
            });
        }
        if cfg.api_key.is_empty() {
            return Err(LlmError {
                message: "清华免费档 token 尚未就位（刚启动时泵首签需几秒），稍等片刻重试；                          或在设置里填自费 API Key 切到自费 API。".into(),
            });
        }
    }
    let url = format!("{}/chat/completions", cfg.base_url);
    // 多模态（2026-10-11）：只有「开关开 + 这条消息带图」时 content 才变 parts 数组；
    // 其余情况与加多模态之前完全一致（形状契约见 session::Msg::to_value 注释）。
    let mut body = json!({
        "model": cfg.model,
        "messages": messages.iter().map(|m| m.to_value(cfg.multimodal)).collect::<Vec<_>>(),
        "stream": cfg.stream,
    });
    let img_n: usize = if cfg.multimodal {
        messages.iter().map(|m| m.images.len()).sum()
    } else {
        0
    };
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
        body["tool_choice"] = json!("auto");
    }
    if cfg.stream {
        body["stream_options"] = json!({ "include_usage": true });
    }
    cfg.apply_thinking(&mut body);
    (hooks.on_log)(&format!(
        "→ LLM 请求 {} · msgs={} · stream={} · thinking={} · imgs={} · {}",
        cfg.model,
        messages.len(),
        cfg.stream,
        body.get("thinking")
            .map(|t| t.to_string())
            .unwrap_or_else(|| "default".into()),
        img_n,
        url
    ));
    let t0 = std::time::Instant::now();
    let body_s = body.to_string();
    if let Ok(dp) = std::env::var("ONETHU_DUMP_BODY") {
        let _ = std::fs::write(&dp, &body_s);
    }
    // Host/Hooks need not be Send: the async transport is driven on this same
    // calling thread. No request threads survive cancellation or a completed turn.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| LlmError {
            message: format!("LLM 网络运行时初始化失败：{e}"),
        })?;
    runtime.block_on(async {
        let client = transport::client()?;
        for retry in 0..=transport::MAX_RETRIES {
            let mut observed_output = false;
            let attempt =
                request_once(&client, &url, &body_s, cfg, h, hooks, &mut observed_output).await;
            let mut failure = match attempt {
                Ok(turn)
                    if turn.content.is_empty()
                        && turn.reasoning.is_empty()
                        && turn.tool_calls.is_empty() =>
                {
                    Failure::new(
                        "模型无任何输出——请检查「模型」名称与 API Endpoint 是否正确",
                        true,
                    )
                }
                Ok(turn) => return Ok(turn),
                Err(failure) => failure,
            };
            // Even buffered content, reasoning, or a partial tool call makes a
            // replay unsafe. Never emit a second answer over an already started one.
            if h.interrupted() || observed_output {
                failure.retryable = false;
            }
            if let Some(delay) = transport::retry_delay(&mut failure, retry) {
                transport::log_retry(hooks, retry, delay, &failure);
                transport::wait_retry(h, delay).await.map_err(|e| e.error)?;
                continue;
            }
            (hooks.on_log)(&format!(
                "✗ LLM 失败（{}ms）：{}",
                t0.elapsed().as_millis(),
                failure.error.message
            ));
            return Err(failure.error);
        }
        unreachable!("bounded retry loop always returns on its final attempt")
    })
}

async fn request_once(
    client: &reqwest::Client,
    url: &str,
    body: &str,
    cfg: &crate::config::Config,
    h: &dyn Host,
    hooks: &mut Hooks<'_>,
    observed_output: &mut bool,
) -> Result<Turn, Failure> {
    let started = std::time::Instant::now();
    let mut request = client
        .post(url)
        .bearer_auth(&cfg.api_key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_owned());
    if !cfg.madmodel_cookie.is_empty() {
        request = request.header(reqwest::header::COOKIE, &cfg.madmodel_cookie);
    }
    let response = transport::io(h, transport::HEADER_TIMEOUT, request.send()).await?;
    let status = response.status();
    (hooks.on_log)(&format!(
        "← LLM 应答头 HTTP {} · {}ms",
        status.as_u16(),
        started.elapsed().as_millis()
    ));
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if status.is_redirection() || (status.is_success() && content_type.contains("text/html")) {
        let via_channel =
            cfg.base_url.contains("webvpn.tsinghua.edu.cn") || cfg.base_url.contains("127.0.0.1");
        let message = if via_channel && cfg.uses_free_tier {
            "webvpn 会话已失效：免费档请求被弹回登录页（校外包装通道）。宿主会自动重签并刷新会话票；若反复出现，请在 设置 → 插件 → OneTHU Harness 重新登录清华账号。"
        } else if url_contains_madmodel(cfg) {
            "MadModel 拒绝了当前网络（IP 门禁 307，token 校验前就拦截）。该免费服务仅限校园网内 IP：请连校园网/学校 VPN（如 EasyConnect），或在插件设置把模型源切回自费 API。"
        } else {
            "模型接口返回了重定向或网页，请检查 API Endpoint 是否为 OpenAI 兼容 API 地址。"
        };
        return Err(Failure::new(message, false));
    }
    if !status.is_success() {
        return Err(transport::http_failure(response, h).await);
    }
    if cfg.stream && content_type.contains("event-stream") {
        stream_turn(response, hooks, h, observed_output).await
    } else {
        parse_completion(transport::json_body(response, h).await?).map_err(Failure::from)
    }
}

/// 免费档判定：请求将打向 madmodel（含被回退前的默认 base）
fn url_contains_madmodel(cfg: &crate::config::Config) -> bool {
    cfg.base_url.contains("madmodel")
}

fn parse_completion(v: Value) -> Result<Turn, LlmError> {
    if let Some(err) = v.get("error") {
        return Err(LlmError {
            message: format!("LLM 错误：{}", err),
        });
    }
    let choice = v
        .get("choices")
        .and_then(|c| c.get(0))
        .cloned()
        .unwrap_or(Value::Null);
    let msg = choice.get("message").cloned().unwrap_or(Value::Null);
    let mut content = msg
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
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
                        id: tc
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("call_0")
                            .to_string(),
                        name: f.get("name").and_then(|n| n.as_str())?.to_string(),
                        arguments: f
                            .get("arguments")
                            .and_then(|a| a.as_str())
                            .unwrap_or("{}")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let usage = v
        .get("usage")
        .and_then(|u| serde_json::from_value::<ApiUsage>(u.clone()).ok());
    Ok(Turn {
        content,
        reasoning,
        tool_calls,
        usage,
    })
}

/// SSE 流解析：逐块回调 delta/think；tool_calls 按 index 增量拼接
async fn stream_turn(
    resp: reqwest::Response,
    hooks: &mut Hooks<'_>,
    h: &dyn Host,
    observed_output: &mut bool,
) -> Result<Turn, Failure> {
    // 参考 DeepSeek Harness 的块式组装：思考先行（reasoning_content / <think> 内联双兼容），
    // tool_calls 按 index 增量累积；事件按时间窗批处理——逐 token 发事件会把 IPC 与日志刷爆。
    let mut lines = transport::Lines::new(resp);

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls: std::collections::BTreeMap<u64, ToolCall> = std::collections::BTreeMap::new();
    let mut usage: Option<ApiUsage> = None;
    let mut finished = false;

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
                        scan += if in_think {
                            "<think>".len()
                        } else {
                            "</think>".len()
                        };
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
                if first_out
                    || elapsed >= BATCH_MS
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
        let line = match lines.next(h).await {
            Ok(Some(line)) => line,
            Ok(None) => {
                (hooks.on_log)(&format!(
                    "✓ 流读取完毕（EOF）：content {} 字 / tools {}",
                    content.chars().count(),
                    calls.len()
                ));
                break;
            }
            Err(error) => {
                pump!(true);
                flush!();
                return Err(error);
            }
        };
        let t = line.trim();
        if t.is_empty() || t.starts_with(':') {
            continue;
        }
        let Some(data) = t.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            finished = true;
            (hooks.on_log)(&format!(
                "✓ 流读取完毕（[DONE]）：content {} 字 / tools {} / reasoning {} 字",
                content.chars().count(),
                calls.len(),
                reasoning.chars().count()
            ));
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        // 端点把错误塞进 200 SSE 流（OpenAI 兼容变体）：data: {"error": {...}}
        if let Some(err) = chunk.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .map(String::from)
                .unwrap_or_else(|| err.to_string());
            pump!(true);
            flush!();
            let code = err.get("code").or_else(|| err.get("type"));
            let retryable = code.is_some_and(|c| {
                matches!(c.as_u64(), Some(408 | 429 | 500..=599))
                    || matches!(
                        c.as_str(),
                        Some(
                            "rate_limit_exceeded" | "server_error" | "timeout" | "overloaded_error"
                        )
                    )
            });
            let label = code
                .map(|c| {
                    c.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| c.to_string())
                })
                .unwrap_or_else(|| "unknown".into());
            return Err(Failure::new(
                format!("LLM 错误（{label}）：{msg}"),
                retryable,
            ));
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            usage = serde_json::from_value::<ApiUsage>(u.clone()).ok();
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
            continue;
        };
        if choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .is_some()
        {
            finished = true;
            // finish_reason 出现后通常还有 usage 尾包，不 break，等 [DONE]/EOF
        }
        let Some(delta) = choice.get("delta") else {
            continue;
        };
        // 思考：DeepSeek 系 reasoning_content / 部分网关 reasoning 字段
        if let Some(rc) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(|c| c.as_str())
        {
            if !rc.is_empty() {
                *observed_output = true;
                reasoning.push_str(rc);
                think_buf.push_str(rc);
            }
        }
        // 正文：进 <think> 路由（内联思考不再混进回答）
        if let Some(dc) = delta.get("content").and_then(|c| c.as_str()) {
            if !dc.is_empty() {
                *observed_output = true;
                raw.push_str(dc);
                pump!(false);
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                *observed_output = true;
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
        if !delta_buf.is_empty() || !think_buf.is_empty() {
            if first_out
                || last_flush.elapsed().as_millis() >= BATCH_MS
                || delta_buf.len() > 400
                || think_buf.len() > 400
            {
                flush!();
                first_out = false;
            }
        }
    }
    // 收尾：清掉撕裂尾巴与未闭合的 <think>
    pump!(true);
    flush!();
    if *observed_output && !finished {
        // A clean socket EOF is not a completed model turn. In particular, do
        // not hand an unfinished tool call to the agent for execution.
        return Err(Failure::new(
            "模型流在完成前断开；已收到部分内容，本轮不会自动重试",
            false,
        ));
    }
    Ok(Turn {
        content,
        reasoning,
        tool_calls: calls.into_values().collect(),
        usage,
    })
}

/// 空轮次兜底：EOF 无任何输出且非打断 → 视为端点异常（模型名错误常此相），报错而不是让 UI 干等
pub fn ensure_nonempty(turn: Turn, was_interrupted: bool) -> Result<Turn, LlmError> {
    if turn.content.is_empty()
        && turn.reasoning.is_empty()
        && turn.tool_calls.is_empty()
        && !was_interrupted
    {
        return Err(LlmError {
            message: "模型无任何输出——请检查「模型」名称与 API Endpoint 是否正确".into(),
        });
    }
    Ok(turn)
}

#[cfg(test)]
#[path = "llm_tests.rs"]
mod tests;
