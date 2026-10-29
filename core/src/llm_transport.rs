//! One in-flight HTTP attempt, driven on the calling thread. Cancelling drops the
//! actual send/body future; there are no detached request workers to keep billing.
use super::{Hooks, LlmError};
use crate::host::Host;
use reqwest::{Client, Response};
use std::future::Future;
use std::time::{Duration, SystemTime};

pub(super) const HEADER_TIMEOUT: Duration = Duration::from_secs(180);
pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(180);
pub(super) const MAX_RETRIES: usize = 2;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const CANCEL_POLL: Duration = Duration::from_millis(25);

pub(super) struct Failure {
    pub error: LlmError,
    pub retryable: bool,
    pub retry_after: Option<Duration>,
    pub rate_limited: bool,
}

impl Failure {
    pub fn new(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            error: LlmError {
                message: message.into(),
            },
            retryable,
            retry_after: None,
            rate_limited: false,
        }
    }

    fn cancelled() -> Self {
        Self::new("LLM 请求已打断", false)
    }
}

impl From<LlmError> for Failure {
    fn from(error: LlmError) -> Self {
        Self {
            error,
            retryable: false,
            retry_after: None,
            rate_limited: false,
        }
    }
}

pub(super) fn client() -> Result<Client, LlmError> {
    // reqwest honours HTTP(S)_PROXY / ALL_PROXY / NO_PROXY. Do not follow a
    // provider's login redirect with a bearer token or mistake HTML for a model.
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("OneTHU-Harness/0.1")
        .build()
        .map_err(|e| LlmError {
            message: format!("LLM 客户端初始化失败：{e}"),
        })
}

async fn cancelled(h: &dyn Host) {
    loop {
        if h.interrupted() {
            return;
        }
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

/// Both futures live in this stack frame. select! drops the losing HTTP future,
/// including while DNS/connect/headers or a stalled SSE body is pending.
pub(super) async fn io<T>(
    h: &dyn Host,
    timeout: Duration,
    operation: impl Future<Output = Result<T, reqwest::Error>>,
) -> Result<T, Failure> {
    if h.interrupted() {
        return Err(Failure::cancelled());
    }
    tokio::select! {
        biased;
        _ = cancelled(h) => Err(Failure::cancelled()),
        result = tokio::time::timeout(timeout, operation) => match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                // Invalid URL/headers and decoding failures are not transient.
                // A connection failure is safe to retry. An ambiguous send/read error
                // may have reached the model already; do not create a second generation.
                let retryable = error.is_connect();
                Err(Failure::new(format!("LLM 网络错误：{}", error.without_url()), retryable))
            }
            Err(_) => Err(Failure::new(format!("LLM 等待响应超时（{} 秒）", timeout.as_secs()), false)),
        }
    }
}

pub(super) async fn wait_retry(h: &dyn Host, delay: Duration) -> Result<(), Failure> {
    tokio::select! {
        biased;
        _ = cancelled(h) => Err(Failure::cancelled()),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

pub(super) fn retry_delay(failure: &mut Failure, retry: usize) -> Option<Duration> {
    if !failure.retryable || retry >= MAX_RETRIES {
        return None;
    }
    if let Some(delay) = failure.retry_after {
        if delay > MAX_RETRY_DELAY {
            failure.error.message.push_str(&format!(
                "；服务端要求等待 {} 秒，请稍后重试（本轮不会提前重发）",
                delay
                    .as_secs()
                    .saturating_add(u64::from(delay.subsec_nanos() > 0))
            ));
            return None;
        }
        return Some(delay);
    }
    // Bounded ±10% jitter, no extra RNG or request worker.
    let jitter = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64
        % 101;
    Some(Duration::from_millis((450 + jitter) * (1 << retry)))
}

pub(super) fn retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|when| when.duration_since(now).unwrap_or_default())
}

pub(super) async fn http_failure(mut response: Response, h: &dyn Host) -> Failure {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| retry_after(v, SystemTime::now()));
    let mut body = Vec::new();
    // Error bodies are diagnostic only; never buffer an unbounded HTML page.
    while body.len() < 64 * 1024 {
        match io(h, IDLE_TIMEOUT, response.chunk()).await {
            Ok(Some(chunk)) => {
                let remaining = 64 * 1024 - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Ok(None) => break,
            Err(error) if h.interrupted() => return error,
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&body);
    let message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error")?.get("message")?.as_str().map(String::from))
        .unwrap_or_else(|| text.into_owned());
    Failure {
        error: LlmError {
            message: format!(
                "LLM HTTP {status}：{}",
                message.chars().take(400).collect::<String>()
            ),
        },
        rate_limited: status == 429,
        retryable: status == 429 || (500..=599).contains(&status),
        retry_after: if status == 429 {
            Some(
                retry_after
                    .unwrap_or(Duration::from_secs(5))
                    .max(Duration::from_secs(5)),
            )
        } else {
            retry_after
        },
    }
}

pub(super) async fn json_body(
    mut response: Response,
    h: &dyn Host,
) -> Result<serde_json::Value, Failure> {
    let mut body = Vec::new();
    while let Some(chunk) = io(h, IDLE_TIMEOUT, response.chunk()).await? {
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|e| Failure::new(format!("响应解析失败：{e}"), body.is_empty()))
}

/// Buffers bytes rather than Strings so a UTF-8 character split between network
/// chunks is decoded only when its complete SSE line is available.
pub(super) struct Lines {
    response: Response,
    buffered: Vec<u8>,
    eof: bool,
}

impl Lines {
    pub fn new(response: Response) -> Self {
        Self {
            response,
            buffered: Vec::new(),
            eof: false,
        }
    }

    pub async fn next(&mut self, h: &dyn Host) -> Result<Option<String>, Failure> {
        loop {
            if h.interrupted() {
                return Err(Failure::cancelled());
            }
            if let Some(end) = self.buffered.iter().position(|&b| b == b'\n') {
                let line: Vec<_> = self.buffered.drain(..=end).collect();
                return String::from_utf8(line)
                    .map(Some)
                    .map_err(|e| Failure::new(format!("流编码错误：{e}"), false));
            }
            if self.eof {
                if self.buffered.is_empty() {
                    return Ok(None);
                }
                return String::from_utf8(std::mem::take(&mut self.buffered))
                    .map(Some)
                    .map_err(|e| Failure::new(format!("流编码错误：{e}"), false));
            }
            match io(h, IDLE_TIMEOUT, self.response.chunk()).await? {
                Some(chunk) => self.buffered.extend_from_slice(&chunk),
                None => self.eof = true,
            }
        }
    }
}

pub(super) fn log_retry(hooks: &mut Hooks, retry: usize, delay: Duration, failure: &Failure) {
    (hooks.on_log)(&format!(
        "↻ LLM 请求失败，{}ms 后串行重试 {}/{}：{}",
        delay.as_millis(),
        retry + 1,
        MAX_RETRIES,
        failure.error.message
    ));
}

// Process-wide school-provider lane, including tool follow-up turns and IM
// conversations. Waiting never owns a request and cancellation stays immediate.
static MADMODEL_LANE: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub(super) fn madmodel_lane(h: &dyn Host) -> Result<std::sync::MutexGuard<'static, ()>, LlmError> {
    loop {
        if h.interrupted() {
            return Err(Failure::cancelled().error);
        }
        match MADMODEL_LANE.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(CANCEL_POLL),
        }
    }
}

static MADMODEL_RETRY_AT: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);
pub(super) fn remember_rate_limit(failure: &Failure) {
    if !failure.rate_limited {
        return;
    }
    let now = std::time::Instant::now();
    let until = now
        .checked_add(failure.retry_after.unwrap_or(Duration::from_secs(5)))
        .unwrap_or(now + Duration::from_secs(86400));
    let mut next = MADMODEL_RETRY_AT.lock().unwrap_or_else(|e| e.into_inner());
    *next = Some(next.map_or(until, |prior| prior.max(until)));
}
pub(super) async fn wait_rate_limit(h: &dyn Host) -> Result<(), Failure> {
    let delay = MADMODEL_RETRY_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .and_then(|until| until.checked_duration_since(std::time::Instant::now()));
    if let Some(delay) = delay {
        wait_retry(h, delay).await?;
    }
    Ok(())
}
