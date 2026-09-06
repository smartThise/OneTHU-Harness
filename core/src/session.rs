//! 会话与历史管理（R5）：多轮对话 / 任务状态持久化在插件私有存储
//! （宿主 onethu.call storage.*，落 localStorage，重启恢复）。
//! - 多会话：新建 / 切换 / 删除 / 列表；
//! - 完整上下文可导出/导入（JSON）；
//! - 轨迹（trace）随会话保存——用户能看到 agent 背后的工作流，非黑盒；
//! - 上下文裁剪：超预算先截断旧工具结果、再丢最老消息（保持 tool 配对完整）。

use crate::config::est_tokens;
use crate::host::Host;
use crate::usage::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const STORE_KEY: &str = "harness.v1";
const MAX_SESSIONS: usize = 24;
pub const MAX_TRACE: usize = 150;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Msg {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 思考内容（v4 协议：仅 tool-call 轮回传 reasoning_content，普通轮丢弃省 token）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Msg {
    pub fn user(text: &str) -> Msg {
        Msg { role: "user".into(), content: Some(text.into()), reasoning: None, tool_calls: None, tool_call_id: None, name: None }
    }
    pub fn assistant(text: &str) -> Msg {
        Msg { role: "assistant".into(), content: Some(text.into()), reasoning: None, tool_calls: None, tool_call_id: None, name: None }
    }
    pub fn tool_result(call_id: &str, text: &str) -> Msg {
        Msg {
            role: "tool".into(),
            content: Some(text.into()),
            reasoning: None,
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            name: None,
        }
    }
    pub fn to_value(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("role".into(), Value::String(self.role.clone()));
        // assistant 恒有 content：reasoning-only 轮用空串（v4 网关拒 null，空串安全）
        if self.role == "assistant" {
            m.insert(
                "content".into(),
                Value::String(self.content.clone().unwrap_or_default()),
            );
        } else if let Some(c) = &self.content {
            m.insert("content".into(), Value::String(c.clone()));
        }
        // 官方 passback 规则：reasoning_content 仅 tool-call 轮回传，普通轮忽略（省 token）
        if self.role == "assistant" && self.tool_calls.is_some() {
            if let Some(r) = &self.reasoning {
                if !r.is_empty() {
                    m.insert("reasoning_content".into(), Value::String(r.clone()));
                }
            }
        }
        if let Some(tc) = &self.tool_calls {
            m.insert("tool_calls".into(), tc.clone());
        }
        if let Some(id) = &self.tool_call_id {
            m.insert("tool_call_id".into(), Value::String(id.clone()));
        }
        if let Some(n) = &self.name {
            m.insert("name".into(), Value::String(n.clone()));
        }
        Value::Object(m)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub messages: Vec<Msg>,
    /// 工作流轨迹（给用户看的，非黑盒）
    pub trace: Vec<String>,
    pub usage: Usage,
    pub cost_usd: f64,
    /// 待确认的预约动作（场景定制③：写操作两段式确认）
    pub pending: Option<PendingAction>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PendingAction {
    pub tool: String,
    pub args: Value,
    pub summary: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Store {
    pub v: u32,
    pub active: String,
    pub seq: u64,
    pub totals: Usage,
    pub totals_cost_usd: f64,
    pub sessions: Vec<Session>,
}

impl Store {
    pub fn load(h: &mut dyn Host) -> Store {
        let raw: Value = h.call("storage", "get", serde_json::json!([STORE_KEY])).unwrap_or(Value::Null);
        match raw {
            Value::Null => Store { v: 1, ..Default::default() },
            v => serde_json::from_value(v).unwrap_or_default(),
        }
    }

    pub fn save(&self, h: &mut dyn Host) {
        let payload = match serde_json::to_value(self) {
            Ok(v) => v,
            Err(_) => return,
        };
        // 容量兜底：序列化 > 1.2MB 时逐步丢最老会话（不动 active）
        let mut payload = payload;
        let active_id = payload
            .get("active")
            .and_then(|a| a.as_str())
            .map(String::from)
            .unwrap_or_default();
        for _ in 0..MAX_SESSIONS {
            let size = serde_json::to_string(&payload).map(|s| s.len()).unwrap_or(0);
            if size < 1_200_000 {
                break;
            }
            let arr = match payload.get_mut("sessions").and_then(|s| s.as_array_mut()) {
                Some(a) if a.len() > 1 => a,
                _ => break,
            };
            let idx = arr
                .iter()
                .position(|s| s.get("id").and_then(|i| i.as_str()) != Some(active_id.as_str()));
            match idx {
                Some(i) => {
                    arr.remove(i);
                }
                None => break,
            }
        }
        let _ = h.call("storage", "set", serde_json::json!([STORE_KEY, payload]));
    }

    pub fn active_mut(&mut self) -> Option<&mut Session> {
        let id = self.active.clone();
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    /// 取当前会话，不存在则建新会话
    pub fn ensure_active(&mut self) -> &mut Session {
        if self.active.is_empty() || !self.sessions.iter().any(|s| s.id == self.active) {
            self.new_session("新会话");
        }
        let id = self.active.clone();
        self.sessions.iter_mut().find(|s| s.id == id).unwrap()
    }

    pub fn new_session(&mut self, title: &str) -> &mut Session {
        self.seq += 1;
        let id = format!("s{}{}", chrono::Utc::now().timestamp_millis(), self.seq);
        let s = Session {
            id: id.clone(),
            title: title.chars().take(24).collect(),
            created_at: now_ms(),
            updated_at: now_ms(),
            messages: Vec::new(),
            trace: Vec::new(),
            usage: Usage::default(),
            cost_usd: 0.0,
            pending: None,
        };
        self.sessions.push(s);
        if self.sessions.len() > MAX_SESSIONS {
            // 丢最老的非 active
            if let Some(idx) = self
                .sessions
                .iter()
                .position(|s| s.id != self.active && s.id != id)
            {
                self.sessions.remove(idx);
            } else {
                self.sessions.remove(0);
            }
        }
        self.active = id;
        self.sessions.last_mut().unwrap()
    }

    pub fn switch(&mut self, id: &str) -> bool {
        if self.sessions.iter().any(|s| s.id == id) {
            self.active = id.to_string();
            true
        } else {
            false
        }
    }

    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|s| s.id != id);
        if self.active == id {
            self.active = self.sessions.last().map(|s| s.id.clone()).unwrap_or_default();
        }
        self.sessions.len() != before
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn msg_tokens(m: &Msg) -> usize {
    let mut n = m.content.as_deref().map(est_tokens).unwrap_or(0);
    if let Some(tc) = &m.tool_calls {
        n += est_tokens(&tc.to_string());
    }
    n + 4
}

pub fn history_tokens(msgs: &[Msg]) -> usize {
    msgs.iter().map(msg_tokens).sum()
}

/// 上下文裁剪：超出预算时，先截旧工具结果，再丢最老的非 system 消息
/// （丢 assistant(tool_calls) 时连同其后的 tool 结果一起丢，保持配对合法）。
pub fn trim_history(msgs: &mut Vec<Msg>, budget: usize) {
    loop {
        if history_tokens(msgs) <= budget || msgs.len() <= 3 {
            return;
        }
        let idx = msgs.iter().position(|m| m.role != "system");
        let Some(i) = idx else { return };
        if msgs[i].role == "tool" {
            // 截断旧工具结果内容（保留配对）
            if let Some(c) = &mut msgs[i].content {
                *c = format!("{}（早前工具结果已省略）", c.chars().take(60).collect::<String>());
            }
            if history_tokens(msgs) <= budget {
                return;
            }
        }
        // 丢第 i 条；若是带 tool_calls 的 assistant，再连丢其后连续 tool 条
        let had_calls = msgs[i].tool_calls.is_some();
        msgs.remove(i);
        if had_calls {
            while msgs.get(i).map(|m| m.role == "tool").unwrap_or(false) {
                msgs.remove(i);
            }
        }
    }
}
