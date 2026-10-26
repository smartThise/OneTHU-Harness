//! 本机 MCP server（2026-10-13，OH 自我进化第一批）。
//!
//! 让**本机任何 MCP 客户端**（其他 agent、Claude Desktop、脚本）直接调用 OneTHU 的接口：
//! - 监听 127.0.0.1:OH_MCP_PORT（默认 8787），仅本机回环，无跨机暴露；
//! - Streamable-HTTP 形态：POST /mcp 收 JSON-RPC 2.0（initialize / tools/list / tools/call）；
//! - tools = 经审查的旧只读工具 + OneTHU CLI 两个统一入口；
//! - tools/call 经同一张桥回执表进 TS 应用（与 chat 工作线程同通道，天然并发安全）。
//!
//! 手搓最小 HTTP/1.1：零新依赖（harness 的依赖纪律）。

use crate::host::{Host, PendingMap};
use crate::tools;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[derive(Clone)]
struct BridgeHost {
    pending: PendingMap,
    seq: Arc<AtomicU64>,
}

impl Host for BridgeHost {
    fn call(&mut self, ns: &str, method: &str, args: Value) -> Result<Value, String> {
        crate::host::bridge_call(&self.pending, &self.seq, ns, method, args)
    }
    fn call_timeout(&mut self, ns: &str, method: &str, args: Value, timeout_ms: u64) -> Result<Value, String> {
        crate::host::bridge_call_timeout(&self.pending, &self.seq, ns, method, args, timeout_ms)
    }
    fn interrupted(&self) -> bool {
        false
    }
    fn reset_interrupt(&self) {}
}

/// 默认关；activate、独立 mcp_sync 和 chat 保险路径从宿主读取授权后的设置。
/// HTTP 客户端和 RPC 参数都不能直接开启服务。
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static AUTHORIZATION_EPOCH: AtomicU64 = AtomicU64::new(0);
type HostFactory = Arc<dyn Fn() -> Box<dyn Host + Send> + Send + Sync>;

#[derive(Default)]
struct RuntimeState {
    sync_revision: u64,
    requested: Option<bool>,
    authorized: Option<bool>,
    bound_port: Option<u16>,
    listening: bool,
    listener_error: Option<String>,
    settings_error: Option<String>,
    host_factory: Option<HostFactory>,
    validation_started: bool,
}

fn runtime() -> &'static Mutex<RuntimeState> {
    static STATE: OnceLock<Mutex<RuntimeState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(RuntimeState::default()))
}

fn set_gate(on: bool) {
    let was = ENABLED.swap(on, std::sync::atomic::Ordering::SeqCst);
    if was != on {
        AUTHORIZATION_EPOCH.fetch_add(1, Ordering::SeqCst);
        eprintln!("[mcp] 本机 MCP server {}", if on { "已开启" } else { "已关闭（连接一律 403）" });
    }
}

/// 宿主停止插件时关闸，并使未完成的设置读取失效。
pub fn set_enabled(on: bool) {
    let mut state = runtime().lock().unwrap_or_else(|e| e.into_inner());
    state.sync_revision += 1;
    set_gate(on);
}

pub fn is_enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::SeqCst)
}

/// listener 是否真的绑上了（`OH_MCP_PORT=0` 或绑定失败 → false）。
/// 与总闸分开报：用户容易把「闸门关着」当成「服务没起」，mcp_status 要把两件事说清。
pub fn is_listening() -> bool {
    runtime().lock().unwrap_or_else(|e| e.into_inner()).listening
}

/// 端口口径**唯一处**（bin 与报告共用）：`OH_MCP_PORT`，解析失败/未设 → 8787。
/// 注意 0 在 bin 侧表示「连监听都不起」，这里照样把它读出来（报告要说实话）。
pub fn port() -> u16 {
    std::env::var("OH_MCP_PORT")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(8787)
}

pub fn endpoint() -> String {
    format!("http://127.0.0.1:{}/mcp", port())
}

fn setting_bool(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(v)) => Some(*v),
        Some(Value::String(v)) => Some(matches!(v.trim().to_lowercase().as_str(), "true" | "on" | "1" | "开")),
        _ => None,
    }
}

/// 只信宿主 settings.get 的有效授权值。代次使较早的开启回执不能覆盖较新的关闭。
/// 也给 chat 复用，避免它拿到旧 settings 后再次把已关的服务打开。
pub fn read_settings_and_sync(h: &mut dyn Host) -> Result<Value, String> {
    let revision = {
        let mut state = runtime().lock().unwrap_or_else(|e| e.into_inner());
        state.sync_revision += 1;
        state.sync_revision
    };
    let result = h.call_timeout("settings", "get", json!([]), 5_000)
        .and_then(|settings| {
            if settings.is_object() { Ok(settings) } else { Err("宿主返回的设置格式无效".into()) }
        });
    apply_settings_result(revision, &result);
    result
}

fn apply_settings_result(revision: u64, result: &Result<Value, String>) {
    let mut state = runtime().lock().unwrap_or_else(|e| e.into_inner());
    if state.sync_revision != revision { return; }
    match result {
        Ok(settings) => {
            state.requested = setting_bool(settings.get("mcpServerRequested"));
            state.authorized = setting_bool(settings.get("ohAuthorized"));
            state.settings_error = None;
            // mcpServer 已由宿主过滤授权；显式 false 的 OH 授权也始终优先关闸。
            set_gate(setting_bool(settings.get("mcpServer")).unwrap_or(false)
                && state.authorized != Some(false));
        }
        Err(error) => {
            set_gate(false);
            state.settings_error = Some(error.clone());
        }
    }
}

pub fn status() -> Value {
    let state = runtime().lock().unwrap_or_else(|e| e.into_inner());
    let port = state.bound_port.unwrap_or_else(port);
    let enabled = is_enabled();
    let (code, error) = if let Some(error) = &state.settings_error {
        ("settings-unavailable", Some(error.as_str()))
    } else if state.authorized == Some(false) {
        ("authorization-required", Some("尚未授权 OH 智能体服务"))
    } else if port == 0 {
        ("listener-disabled", Some("OH_MCP_PORT=0：本机 MCP 监听已禁用"))
    } else if let Some(error) = &state.listener_error {
        ("listener-unavailable", Some(error.as_str()))
    } else if !state.listening {
        ("listener-not-started", Some("本机 MCP 监听尚未启动"))
    } else { ("", None) };
    let mut out = json!({
        "enabled": enabled, "listening": state.listening, "port": port,
        "endpoint": if port == 0 { String::new() } else { format!("http://127.0.0.1:{port}/mcp") },
        "cliEndpoint": if port == 0 { String::new() } else { format!("http://127.0.0.1:{port}/cli/v1") },
        "supported": true, "reachableNow": enabled && state.listening && port > 0,
    });
    if let Some(v) = state.requested { out["requested"] = json!(v); }
    if let Some(v) = state.authorized { out["authorized"] = json!(v); }
    if let Some(error) = error { out["error"] = json!(error); out["errorCode"] = json!(code); }
    out
}

/// 旧 OH 的 confirm 标志属于对话交互协议，不能用作外部调用权限边界。
/// 每个旧入口和它能触达的宿主操作均显式列出；新增 OH 工具默认不对 MCP 开放。
pub(crate) fn legacy_read_tool(name: &str) -> bool {
    matches!(name,
        "mcp_status" | "query_profile" | "query_schedule" | "query_agenda" |
        "query_mails" | "read_mail" | "search_mails" | "query_cloud_files" | "search_cloud_files" |
        "query_exams" | "query_deadlines" | "query_grades" | "query_calendar" |
        "query_card_balance" | "query_card_transactions" |
        "query_learn_courses" | "query_learn_semesters" | "query_learn_bbs" |
        "query_venue_scenes" | "query_venue_slots" | "query_venue_records" |
        "query_kongjian" | "query_kongjian_my" | "query_xk_catalog" | "query_xk_selected" |
        "query_xk_reviews" | "query_coursex" | "query_invoices" | "query_bank_payments" |
        "query_graduate_income" | "query_dorm_score" | "query_physical_exam" |
        "query_assessments" | "query_dorm_ele_records" | "query_network" |
        "query_news" | "query_subscribed_news" | "read_news_article" | "query_empty_classrooms" |
        "query_library" | "query_library_seats" | "my_library_bookings" |
        "query_librooms" | "my_libroom_bookings")
}

fn legacy_read_operation(operation: &str) -> bool {
    matches!(operation,
        "session.status" | "session.username" | "user.info" | "info.schedule" | "cal.agenda" |
        "mail.list" | "mail.read" | "mail.search" | "cloud.repos" | "cloud.list" | "cloud.search" |
        "info.exams" | "info.deadlines" | "info.report" | "info.schoolCalendar" |
        "card.info" | "card.transactions" | "learn.courses" | "learn.semesters" |
        "learn.bbsBoards" | "learn.bbsThreads" | "learn.bbsThread" | "learn.bbsPosts" |
        "venue.scenes" | "venue.currentPage" | "venue.myRecords" | "kongjian.page" | "kongjian.my" |
        "xk.search" | "xk.selected" | "xk.reviews" | "coursex.search" | "coursex.detail" |
        "info.invoices" | "info.bankPayments" | "info.graduateIncome" | "info.dormScore" |
        "info.physicalExam" | "info.assessmentList" | "dorm.elePayRecord" |
        "network.accountInfo" | "network.balance" | "network.deviceCount" | "network.devices" |
        "info.news" | "info.searchNews" | "info.newsSub" | "info.newsDetail" |
        "info.classroomList" | "info.classroomState" |
        "library.list" | "library.floors" | "library.sections" | "library.seats" | "library.records" |
        "libroom.list" | "libroom.resources" | "libroom.records")
}

struct LegacyReadHost<'a> { inner: &'a mut dyn Host }
impl Host for LegacyReadHost<'_> {
    fn call(&mut self, ns: &str, method: &str, mut args: Value) -> Result<Value, String> {
        let operation = format!("{ns}.{method}");
        if !legacy_read_operation(&operation) {
            return Err("MCP_LEGACY_UNAVAILABLE: use onethu_services and onethu_call".into());
        }
        // 旧工具用 null 表示可选参数；CLI 的严格 schema 使用缺省字段。
        if let Some(items) = args.as_array_mut() {
            while items.last().map(Value::is_null) == Some(true) { items.pop(); }
            if operation == "xk.search" {
                if let Some(Value::Object(filter)) = items.first_mut() { filter.retain(|_, v| !v.is_null()); }
            }
            if matches!(operation.as_str(), "library.sections" | "library.seats") {
                if let Some(item) = items.first_mut() {
                    *item = json!({"id": item["id"], "zhNameTrace": item["zhNameTrace"]});
                }
            }
        }
        // 不传 confirm：旧只读适配器即使以后误接到写操作，也仍由 CLI 拒绝。
        let mut out = self.inner.call_timeout("cli", "call", json!([{"operation": operation, "args": args}]), 180_000)?;
        if operation == "learn.courses" && out.get("rows").is_some() {
            out["courses"] = out["rows"].clone();
        }
        Ok(out)
    }
    fn call_timeout(&mut self, ns: &str, method: &str, args: Value, _: u64) -> Result<Value, String> {
        self.call(ns, method, args)
    }
    fn interrupted(&self) -> bool { self.inner.interrupted() }
}

pub fn exposed_tool_count() -> usize { tool_defs().len() }

fn tool_defs() -> Vec<(String, String, Value, bool)> {
    let mut defs: Vec<_> = tools::all_tools()
        .into_iter()
        .filter(|t| legacy_read_tool(t.name))
        .map(|t| (t.name.to_string(), t.desc.to_string(), t.params, t.confirm))
        .collect();
    defs.push(("onethu_services".into(), "列出 OneTHU CLI 可调用的校园服务及参数、权限和确认要求；无需模型。".into(), json!({
        "type": "object", "properties": {}, "additionalProperties": false,
    }), false));
    defs.push(("onethu_call".into(), "调用 OneTHU CLI 服务并返回完整结构化结果。operation 来自 onethu_services；写操作需要 confirm=true 且通过应用内确认。".into(), json!({
        "type": "object", "properties": {
            "operation": {"type": "string"}, "args": {"type": "array", "items": {}},
            "confirm": {"type": "boolean"},
            "deadlineUnixMs": {"type": "number", "description": "Optional absolute deadline in Unix milliseconds; host caps request lifetime."},
        }, "required": ["operation", "args"], "additionalProperties": false,
    }), false));
    defs
}

/// 由 sidecar main 在 StdioHost::spawn 后调用（桥 Arc 共享给本线程组）。
///
/// 2026-10-15 起：**起监听 ≠ 对外可用**。listener 常驻绑定（Rust 标准库没有可移植的
/// 「温和收回端口」写法），真正决定接不接受请求的是下面那个闸门 —— 由用户在
/// OH 设置 → 插件 → OneTHU Harness → 高级配置里的「OH MCP 服务器」开关经 settings
/// 泵进来（activate / mcp_sync，chat 保险同步）。
pub fn spawn(pending: PendingMap, seq: Arc<AtomicU64>, port: u16) {
    spawn_with_host(port, move || Box::new(BridgeHost { pending: pending.clone(), seq: seq.clone() }));
}

/// 两个宿主共用同一 HTTP 实现：stdio 桥与 Android 内嵌桥仅提供 Host 工厂。
/// 同步 bind，activate 返回的 listening 是实际绑定结果；重复启动不会挤占自己的端口。
pub fn spawn_with_host<F>(port: u16, factory: F)
where F: Fn() -> Box<dyn Host + Send> + Send + Sync + 'static {
    let listener = {
        let mut state = runtime().lock().unwrap_or_else(|e| e.into_inner());
        // 内嵌插件重启时换到新桥，监听可复用；旧桥不再接新请求。
        state.sync_revision += 1;
        set_gate(false);
        state.host_factory = Some(Arc::new(factory));
        if state.listening { return; }
        state.bound_port = Some(port);
        state.listener_error = None;
        if port == 0 { return; }
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => { state.listening = true; l },
            Err(e) => {
                eprintln!("[mcp] 127.0.0.1:{} 绑定失败（{}）——本机 MCP 关闭", port, e);
                state.listener_error = Some(format!("127.0.0.1:{port} 监听失败：{e}"));
                return;
            }
        }
    };
    std::thread::spawn(move || {
        eprintln!(
            "[mcp] OneTHU CLI / MCP 本机监听已就位：127.0.0.1:{}（当前{}）",
            port,
            if is_enabled() { "已开启" } else { "关闭中：等设置开关，连接一律 403" }
        );
        for stream in listener.incoming() {
            if let Ok(stream) = stream {
                let factory = runtime().lock().unwrap_or_else(|e| e.into_inner()).host_factory.clone();
                if let Some(factory) = factory {
                    let mut host = factory();
                    std::thread::spawn(move || serve(stream, host.as_mut()));
                }
            }
        }
        runtime().lock().unwrap_or_else(|e| e.into_inner()).listening = false;
    });
    let start_validation = {
        let mut state = runtime().lock().unwrap_or_else(|e| e.into_inner());
        let start = !state.validation_started;
        state.validation_started = true;
        start
    };
    if start_validation {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_secs(10));
            // UI 重载或退出后旧闸门不能无限存活。只续验已开启的服务，关闭后由显式同步恢复。
            if !is_enabled() { continue; }
            let factory = runtime().lock().unwrap_or_else(|e| e.into_inner()).host_factory.clone();
            if let Some(factory) = factory {
                let mut host = factory();
                let _ = read_settings_and_sync(host.as_mut());
            }
        });
    }
}

struct AuthorizedHost<'a> { inner: &'a mut dyn Host, epoch: u64 }

impl Host for AuthorizedHost<'_> {
    fn call(&mut self, ns: &str, method: &str, args: Value) -> Result<Value, String> {
        if self.interrupted() { return Err("MCP_DISABLED: 本机 MCP 授权已关闭或变更".into()); }
        let result = self.inner.call(ns, method, args);
        if self.interrupted() { return Err("MCP_DISABLED: 本机 MCP 授权已关闭或变更".into()); }
        result
    }
    fn call_timeout(&mut self, ns: &str, method: &str, args: Value, timeout_ms: u64) -> Result<Value, String> {
        if self.interrupted() { return Err("MCP_DISABLED: 本机 MCP 授权已关闭或变更".into()); }
        let result = self.inner.call_timeout(ns, method, args, timeout_ms);
        if self.interrupted() { return Err("MCP_DISABLED: 本机 MCP 授权已关闭或变更".into()); }
        result
    }
    fn interrupted(&self) -> bool {
        !is_enabled() || AUTHORIZATION_EPOCH.load(Ordering::SeqCst) != self.epoch
    }
}

fn serve(mut stream: TcpStream, host: &mut dyn Host) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    buf.push(byte[0]);
                    if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
                        break;
                    }
                    if buf.len() > 16384 {
                        return;
                    }
                }
            }
        }
        let head = String::from_utf8_lossy(&buf);
        let request_line = head.lines().next().unwrap_or("");
        let path = match request_line {
            "POST /mcp HTTP/1.1" | "POST /mcp HTTP/1.0" => Some("/mcp"),
            "POST /cli/v1 HTTP/1.1" | "POST /cli/v1 HTTP/1.0" => Some("/cli/v1"),
            _ => None,
        };
        // 浏览器来源不能借本机 MCP 读取学校数据。原生 MCP 客户端无需 Origin。
        if head.lines().any(|line| line.to_ascii_lowercase().starts_with("origin:")) {
            let _ = write_resp(&mut stream, 403, "{\"error\":\"Browser-origin requests are not accepted\"}");
            return;
        }
        let authority = head.lines().find_map(|line| line.split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.trim().to_ascii_lowercase()));
        if !authority.as_deref().is_some_and(|value| {
            value == "localhost" || value == "127.0.0.1"
                || value.strip_prefix("localhost:").or_else(|| value.strip_prefix("127.0.0.1:"))
                    .is_some_and(|port| port.parse::<u16>().is_ok())
        }) {
            let _ = write_resp(&mut stream, 403, "{\"error\":\"A loopback Host header is required\"}");
            return;
        }
        let len = head
            .to_ascii_lowercase()
            .split("\r\n")
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if len > 1024 * 1024 {
            let _ = write_resp(&mut stream, 413, "{\"error\":\"MCP request exceeds 1 MiB\"}");
            return;
        }
        let mut body = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut body).is_err() {
            return;
        }
        let Some(path) = path else {
            let _ = write_resp(&mut stream, 405, "{\"error\":\"POST /mcp or /cli/v1\"}");
            return;
        };
        /* 总闸（2026-10-15 用户点名）：OH 设置高级档那个「OH MCP 服务器」开关。
           关 = 立刻 403 断开 —— 外部客户端看到的是明确的"未开启"，而不是超时或空工具表。 */
        if !is_enabled() {
            let _ = write_resp(
                &mut stream,
                403,
                "{\"error\":\"OH MCP 服务器未开启：请在 OneTHU 的设置 → 插件 → OneTHU Harness → 高级配置里打开\"}",
            );
            return;
        }
        let mut authorized = AuthorizedHost { inner: host, epoch: AUTHORIZATION_EPOCH.load(Ordering::SeqCst) };
        let reply = if path == "/cli/v1" {
            handle_cli_rpc(&String::from_utf8_lossy(&body), &mut authorized)
        } else {
            handle_rpc(&String::from_utf8_lossy(&body), &mut authorized)
        };
        if authorized.interrupted() {
            let _ = write_resp(&mut stream, 403, "{\"error\":\"MCP_DISABLED: authorization changed\"}");
            return;
        }
        if write_resp(&mut stream, if reply.is_empty() { 202 } else { 200 }, &reply).is_err() {
            return;
        }
    }
}

fn write_resp(stream: &mut TcpStream, code: u16, body: &str) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "ERR",
    };
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn rpc_result(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } }).to_string()
}

fn handle_cli_rpc(raw: &str, host: &mut dyn Host) -> String {
    let value: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return rpc_error(Value::Null, -32700, format!("解析失败：{e}")),
    };
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    let result = match value.get("method").and_then(Value::as_str).unwrap_or("") {
        "service/list" => host.call_timeout("cli", "list", json!([]), 15_000),
        "service/call" => {
            let params = value.get("params").cloned().unwrap_or(json!({}));
            host.call_timeout("cli", "call", json!([params]), 180_000)
        }
        _ => return rpc_error(id, -32601, "OneTHU CLI 支持 service/list 和 service/call"),
    };
    match result {
        Ok(out) => rpc_result(id, out),
        Err(error) => rpc_error(id, -32000, error),
    }
}

fn handle_rpc(raw: &str, host: &mut dyn Host) -> String {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return rpc_error(Value::Null, -32700, format!("解析失败：{e}")),
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
    let params = v.get("params").cloned().unwrap_or(json!({}));
    match method.as_str() {
        "initialize" => rpc_result(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "OneTHU CLI", "version": env!("CARGO_PKG_VERSION") }
            }),
        ),
        /* 客户端的通知不需要响应（原先回 -32601 错误会让标准 MCP 客户端握手报警）。 */
        m if m.starts_with("notifications/") => String::new(),
        "tools/list" => {
            let list: Vec<Value> = tool_defs()
                .into_iter()
                .filter(|t| !t.3)
                .map(|t| json!({"name": t.0, "description": t.1, "inputSchema": t.2}))
                .collect();
            rpc_result(id, json!({ "tools": list }))
        }
        "tools/call" => {
            let name = params.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            if name == "onethu_services" || name == "onethu_call" {
                let result = if name == "onethu_services" {
                    host.call_timeout("cli", "list", json!([]), 15_000)
                } else {
                    host.call_timeout("cli", "call", json!([args]), 180_000)
                };
                return match result {
                    Ok(out) => rpc_result(id, json!({
                        "content": [{"type": "text", "text": out.to_string()}],
                        "structuredContent": if out.is_object() { out } else { json!({"result": out}) },
                    })),
                    Err(error) => rpc_error(id, -32000, error),
                };
            }
            if !legacy_read_tool(&name) {
                rpc_error(id, -32602, "MCP_LEGACY_UNAVAILABLE: 此旧工具未开放；请使用 onethu_services 和 onethu_call（use onethu_call），写操作由应用内确认")
            } else {
                let mut readonly = LegacyReadHost { inner: host };
                let mut ctx = crate::tools::Ctx { h: &mut readonly };
                match tools::execute(&mut ctx, &name, &args, false, &[]) {
                    Ok(out) => {
                        let text = match out {
                            crate::tools::ToolOut::Text(t) => t,
                            crate::tools::ToolOut::ConfirmNeeded { summary, .. } => summary,
                        };
                        rpc_result(id, json!({ "content": [{ "type": "text", "text": text }] }))
                    }
                    Err(e) => rpc_error(id, -32000, e),
                }
            }
        }
        other => rpc_error(id, -32601, format!("未知方法：{other}")),
    }
}

#[cfg(test)]
pub(crate) static TEST_STATE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> BridgeHost {
        BridgeHost { pending: Default::default(), seq: Arc::new(AtomicU64::new(1)) }
    }

    fn rpc(raw: &str) -> Value {
        serde_json::from_str(&handle_rpc(raw, &mut host())).expect("响应必须是 JSON")
    }

    #[test]
    fn initialize_and_tools_list_are_jsonrpc_envelopes() {
        let v = rpc(r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        assert!(v["result"]["protocolVersion"].is_string(), "initialize 必须回协议版本：{v}");
        assert_eq!(v["result"]["serverInfo"]["name"], "OneTHU CLI");

        let v = rpc(r#"{"jsonrpc":"2.0","id":8,"method":"tools/list","params":{}}"#);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 8);
        let tools = v["result"]["tools"].as_array().expect("tools/list 要有 tools 数组");
        assert_eq!(tools.len(), exposed_tool_count(), "列表包含显式只读白名单与两个受门面校验的 CLI 工具");
        assert!(tools.iter().all(|t| t["inputSchema"].is_object()), "每个工具都要带 inputSchema");
        assert!(tools.iter().all(|t| legacy_read_tool(t["name"].as_str().unwrap()) || matches!(t["name"].as_str(), Some("onethu_services" | "onethu_call"))));
    }

    #[test]
    fn notifications_get_no_body_unknown_methods_get_error() {
        assert!(handle_rpc(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &mut host()).is_empty());
        let v = rpc(r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#);
        assert_eq!(v["error"]["code"], -32601, "未知方法要回 JSON-RPC 错误：{v}");
    }

    #[test]
    fn confirm_tools_are_rejected_by_inbound_server() {
        let name = crate::tools::all_tools().into_iter().find(|t| t.confirm).map(|t| t.name).expect("至少要有一个 confirm 工具");
        let raw = json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":name,"arguments":{}}}).to_string();
        let v = rpc(&raw);
        assert_eq!(v["error"]["code"], -32602, "confirm 工具必须被扣下：{v}");
    }

    #[test]
    fn every_unreviewed_legacy_tool_is_rejected_before_host_dispatch() {
        struct NoHost;
        impl Host for NoHost {
            fn call(&mut self, _: &str, _: &str, _: Value) -> Result<Value, String> { panic!("unreviewed tool reached host"); }
            fn interrupted(&self) -> bool { false }
        }
        let mut host = NoHost;
        for tool in crate::tools::all_tools().into_iter().filter(|t| !legacy_read_tool(t.name)) {
            let raw = json!({"id":1,"method":"tools/call","params":{"name":tool.name,"arguments":{"confirmed":true,"confirm":true,"submit":true,"code":"mail.send()"}}}).to_string();
            let result: Value = serde_json::from_str(&handle_rpc(&raw, &mut host)).unwrap();
            assert_eq!(result["error"]["code"], -32602, "{}", tool.name);
            assert!(result["error"]["message"].as_str().unwrap().contains("use onethu_call"));
        }
        for name in ["ext_flow_create", "ext_flow_run", "ext_tool_create", "ext_tool_run", "run_plugin_cmd", "skill_run", "visit_fill", "memory_refresh"] {
            assert!(!legacy_read_tool(name), "{name}");
        }
    }

    #[test]
    fn legacy_queries_use_cli_without_confirmation_and_adapt_strict_schemas() {
        struct ReadHost { calls: Vec<Value> }
        impl Host for ReadHost {
            fn call(&mut self, _: &str, _: &str, _: Value) -> Result<Value, String> { panic!("must set CLI timeout"); }
            fn call_timeout(&mut self, ns: &str, method: &str, args: Value, timeout: u64) -> Result<Value, String> {
                assert_eq!((ns, method, timeout), ("cli", "call", 180_000));
                assert!(args[0].get("confirm").is_none());
                self.calls.push(args.clone());
                Ok(if args[0]["operation"] == "learn.courses" { json!({"semester":"current","rows":[{"id":"c1","name":"Math"}]}) } else { json!([]) })
            }
            fn interrupted(&self) -> bool { false }
        }
        let mut host = ReadHost { calls: vec![] };
        let raw = json!({"id":1,"method":"tools/call","params":{"name":"query_learn_courses","arguments":{}}}).to_string();
        let result: Value = serde_json::from_str(&handle_rpc(&raw, &mut host)).unwrap();
        let data: Value = serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(data["rows"][0]["id"], "c1");
        assert_eq!(host.calls[0], json!([{"operation":"learn.courses","args":[]}]));
        let mut legacy = LegacyReadHost { inner: &mut host };
        legacy.call("library", "sections", json!([{"id":7,"zhNameTrace":"floor","unexpected":"server metadata"},0])).unwrap();
        legacy.call("libroom", "list", json!([])).unwrap();
        assert!(legacy.call("mail", "send", json!([])).unwrap_err().contains("MCP_LEGACY_UNAVAILABLE"));
        assert!(legacy.call("ext", "run", json!([])).is_err());
        assert_eq!(host.calls[1], json!([{"operation":"library.sections","args":[{"id":7,"zhNameTrace":"floor"},0]}]));
        assert_eq!(host.calls[2][0]["operation"], "libroom.list");
    }

    struct SettingsHost(Result<Value, String>);
    impl Host for SettingsHost {
        fn call(&mut self, ns: &str, method: &str, args: Value) -> Result<Value, String> {
            assert_eq!((ns, method, args), ("settings", "get", json!([])));
            self.0.clone()
        }
        fn call_timeout(&mut self, ns: &str, method: &str, args: Value, timeout: u64) -> Result<Value, String> {
            assert_eq!(timeout, 5_000);
            self.call(ns, method, args)
        }
        fn interrupted(&self) -> bool { false }
    }

    fn reset_runtime() {
        set_enabled(false);
        *runtime().lock().unwrap() = RuntimeState::default();
    }

    #[test]
    fn activate_and_sync_need_only_authorized_settings_without_a_model() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        reset_runtime();
        let mut h = SettingsHost(Ok(json!({ "mcpServer": "true", "mcpServerRequested": "true", "ohAuthorized": "true" })));
        let out = crate::activate(&mut h);
        assert_eq!(out["mcpStatus"]["enabled"], true);
        assert_eq!(out["mcpStatus"]["listening"], false);
        assert_eq!(out["mcpStatus"]["requested"], true);
        assert_eq!(out["mcpStatus"]["authorized"], true);
        assert!(out["commands"].as_array().unwrap().iter().any(|c| c["id"] == "chat"));
        h.0 = Ok(json!({ "mcpServer": "false", "mcpServerRequested": "true", "ohAuthorized": "false" }));
        let closed = crate::sync_mcp_settings(&mut h);
        assert_eq!(closed["enabled"], false);
        assert_eq!(closed["errorCode"], "authorization-required");
        reset_runtime();
    }

    #[test]
    fn missing_invalid_and_failed_settings_close_the_gate() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        reset_runtime();
        for settings in [Ok(json!({})), Ok(json!([])), Err("webview unavailable".into())] {
            set_enabled(true);
            let out = crate::sync_mcp_settings(&mut SettingsHost(settings));
            assert_eq!(out["enabled"], false);
        }
        assert_eq!(status()["errorCode"], "settings-unavailable");
        assert_eq!(status()["error"], "webview unavailable");
        reset_runtime();
    }

    #[test]
    fn older_settings_and_disposed_host_cannot_reenable_after_close() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        reset_runtime();
        let old_revision = runtime().lock().unwrap().sync_revision;
        crate::sync_mcp_settings(&mut SettingsHost(Ok(json!({"mcpServer": "false"}))));
        apply_settings_result(old_revision, &Ok(json!({"mcpServer": "true"})));
        assert!(!is_enabled());
        let pending_revision = runtime().lock().unwrap().sync_revision;
        set_enabled(false);
        apply_settings_result(pending_revision, &Ok(json!({"mcpServer": "true"})));
        assert!(!is_enabled());
        reset_runtime();
    }

    #[test]
    fn close_during_host_call_discards_the_result_even_if_reopened() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        reset_runtime();
        struct RevokingHost;
        impl Host for RevokingHost {
            fn call(&mut self, _: &str, _: &str, _: Value) -> Result<Value, String> {
                set_enabled(false);
                set_enabled(true);
                Ok(json!({"private": "old account data"}))
            }
            fn interrupted(&self) -> bool { false }
        }
        set_enabled(true);
        let mut inner = RevokingHost;
        let mut guarded = AuthorizedHost { inner: &mut inner, epoch: AUTHORIZATION_EPOCH.load(Ordering::SeqCst) };
        assert!(guarded.call("learn", "courses", json!([])).unwrap_err().starts_with("MCP_DISABLED"));
        reset_runtime();
    }

    #[test]
    fn bind_collision_is_visible_and_never_claims_listening() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        reset_runtime();
        let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy_port = occupied.local_addr().unwrap().port();
        spawn_with_host(busy_port, || Box::new(SettingsHost(Ok(json!({})))));
        let out = status();
        assert_eq!(out["port"], busy_port);
        assert_eq!(out["listening"], false);
        assert_eq!(out["errorCode"], "listener-unavailable");
        assert!(out["error"].as_str().unwrap().contains(&busy_port.to_string()));
        reset_runtime();
    }

    #[test]
    fn cli_and_mcp_share_explicit_service_timeouts_and_preserve_arguments() {
        struct ServiceHost;
        impl Host for ServiceHost {
            fn call(&mut self, _: &str, _: &str, _: Value) -> Result<Value, String> {
                panic!("服务调用必须明确给出超时，不能退回默认桥窗口");
            }
            fn call_timeout(&mut self, ns: &str, method: &str, args: Value, timeout: u64) -> Result<Value, String> {
                assert_eq!(ns, "cli");
                assert_eq!(timeout, if method == "list" { 15_000 } else { 180_000 });
                assert_eq!(args, if method == "list" { json!([]) } else {
                    json!([{"operation":"library.reserve","args":["seat-1"],"confirm":true}])
                });
                Ok(json!({"full": args}))
            }
            fn interrupted(&self) -> bool { false }
        }
        let params = json!({"operation":"library.reserve","args":["seat-1"],"confirm":true});
        let mut host = ServiceHost;
        for (method, tool) in [("service/list", "onethu_services"), ("service/call", "onethu_call")] {
            let cli: Value = serde_json::from_str(&handle_cli_rpc(&json!({"id":1,"method":method,"params":params}).to_string(), &mut host)).unwrap();
            let mcp: Value = serde_json::from_str(&handle_rpc(&json!({"id":2,"method":"tools/call","params":{"name":tool,"arguments":params}}).to_string(), &mut host)).unwrap();
            assert_eq!(cli["result"], mcp["result"]["structuredContent"]);
        }
    }
}
