//! MCP（Model Context Protocol）stdio 客户端——最小实现。
//!
//! 设计取舍：**冷启动模式**。每次工具调用都重新 spawn MCP server 子进程，
//! 走 initialize → tools/call → 读结果 → kill 生命周期。慢（每次 ~1s）但
//! 无常驻状态与会话管理，崩溃/挂死影响面为零；OH 的工具调用频次下开销可接受。
//! 设置项 `mcpServers`（JSON 数组）：[{"name":"fs","command":"npx","args":["-y","@modelcontextprotocol/server-filesystem","/path"]}]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct McpServerDef {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct McpToolDef {
    pub server: String,
    pub name: String,
    pub desc: String,
    pub params: Value,
}

/// 解析设置项 mcpServers（JSON 字符串）；非法条目跳过，不阻断其余 server。
pub fn parse_servers(raw: &str) -> Vec<McpServerDef> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.as_array() {
        Some(a) => a.clone(),
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for item in arr {
        let name = item.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let command = item.get("command").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if name.is_empty() || command.is_empty() {
            continue;
        }
        let args = item
            .get("args")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_else(Vec::new);
        let env = item
            .get("env")
            .and_then(|x| x.as_object())
            .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
            .unwrap_or_else(Vec::new);
        out.push(McpServerDef { name, command, args, env });
    }
    out
}

/// 工具全名前缀：mcp_<server>_<tool>（server/tool 名清洗为安全字符）
pub fn tool_full_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
            .collect()
    };
    format!("mcp_{}_{}", clean(server), clean(tool))
}

struct Proc {
    child: Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
}

impl Proc {
    fn spawn(def: &McpServerDef) -> Result<Proc, String> {
        let mut cmd = Command::new(&def.command);
        cmd.args(&def.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in &def.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| format!("MCP server「{}」启动失败：{e}", def.name))?;
        let stdin = child.stdin.take().ok_or("stdin 不可用")?;
        let stdout = child.stdout.take().ok_or("stdout 不可用")?;
        Ok(Proc { child, stdin, reader: BufReader::new(stdout) })
    }

    /// 发请求并读取直到拿到同 id 的响应行（跳过 notification / 其他 id）
    fn request(&mut self, id: u64, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let mut line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("MCP 写入失败：{e}"))?;
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() > deadline {
                return Err("MCP 响应超时（20s）".into());
            }
            let mut buf = String::new();
            // BufReader 借用 self.reader 的可变借用冲突——用行读取闭包绕开
            let n = {
                let r = &mut self.reader;
                match r.read_line(&mut buf) {
                    Ok(n) => n,
                    Err(e) => return Err(format!("MCP 读取失败：{e}")),
                }
            };
            if n == 0 {
                return Err("MCP server 提前退出".into());
            }
            let v: Value = match serde_json::from_str(buf.trim()) {
                Ok(v) => v,
                Err(_) => continue, // 非 JSON 行（server 日志）忽略
            };
            if v.get("id").and_then(|x| x.as_u64()) == Some(id) {
                if let Some(err) = v.get("error") {
                    return Err(format!("MCP 错误：{err}"));
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
            // 通知/其他 id：继续读
        }
    }
}

fn initialize(p: &mut Proc) -> Result<(), String> {
    p.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "OneTHU", "version": "1.0.0" }
        }),
        Duration::from_secs(20),
    )
    .map(|_| ())?;
    // initialized 通知（无 id、无响应）
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let mut line = serde_json::to_string(&note).map_err(|e| e.to_string())?;
    line.push('\n');
    p.stdin.write_all(line.as_bytes()).and_then(|_| p.stdin.flush()).map_err(|e| format!("MCP 写入失败：{e}"))
}

fn content_text(result: &Value) -> String {
    let mut out = String::new();
    if let Some(arr) = result.get("content").and_then(|x| x.as_array()) {
        for c in arr {
            if c.get("type").and_then(|x| x.as_str()) == Some("text") {
                if let Some(t) = c.get("text").and_then(|x| x.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
    }
    if let Some(err) = result.get("isError").and_then(|x| x.as_bool()) {
        if err && !out.is_empty() {
            out = format!("[工具报错] {out}");
        }
    }
    if out.is_empty() {
        out = serde_json::to_string_pretty(result).unwrap_or_default();
    }
    out
}

/// 列出某 server 的全部工具（冷启动一轮）
pub fn list_tools(def: &McpServerDef) -> Result<Vec<McpToolDef>, String> {
    let mut p = Proc::spawn(def)?;
    let r = (|| -> Result<Vec<McpToolDef>, String> {
        initialize(&mut p)?;
        let result = p.request(2, "tools/list", json!({}), Duration::from_secs(20))?;
        let mut out = Vec::new();
        if let Some(tools) = result.get("tools").and_then(|x| x.as_array()) {
            for t in tools {
                let name = t.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                out.push(McpToolDef {
                    server: def.name.clone(),
                    name,
                    desc: t.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    params: t.get("inputSchema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                });
            }
        }
        Ok(out)
    })();
    let _ = p.child.kill();
    let _ = p.child.wait();
    r
}

/// 执行某 server 的工具（冷启动一轮）
pub fn call_tool(def: &McpServerDef, tool: &str, args: &Value) -> Result<String, String> {
    let mut p = Proc::spawn(def)?;
    let r = (|| -> Result<String, String> {
        initialize(&mut p)?;
        let result = p.request(
            3,
            "tools/call",
            json!({ "name": tool, "arguments": args }),
            Duration::from_secs(60),
        )?;
        Ok(content_text(&result))
    })();
    let _ = p.child.kill();
    let _ = p.child.wait();
    r
}
