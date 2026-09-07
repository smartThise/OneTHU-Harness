//! agent 主控循环（R1 核心）：LLM 工具循环、上下文与预算管理、进度/打断（R4）。
//! 命令入口：chat（对话）、new_session、list_sessions、switch_session、
//! delete_session、export_session、import_session、usage_report、selftest。

use crate::config::{self, est_tokens, now_line, Config};
use crate::host::{Emit, Host};
use crate::llm::{self, Hooks};
use crate::session::{self, Msg, PendingAction, Store};
use crate::tools::{self, ToolOut, Ctx};
use crate::usage::{self, Usage};
use serde_json::{json, Value};

fn progress(emit: &dyn Emit, kind: &str, text: &str, payload: Option<Value>) {
    let mut p = json!({ "text": text, "kind": kind });
    if let Some(pl) = payload {
        p["payload"] = pl;
    }
    emit.notify("progress", p);
}

fn usage_progress(emit: &dyn Emit, sess: &session::Session, store: &Store, cfg: &Config) {
    let budget_left = (cfg.budget_usd - store.totals_cost_usd).max(0.0);
    progress(emit,
        "usage",
        &usage::fmt_usd(store.totals_cost_usd),
        Some(json!({
            "sessionCostUsd": sess.cost_usd, "totalCostUsd": store.totals_cost_usd,
            "budgetUsd": cfg.budget_usd, "budgetLeftUsd": budget_left,
            "sessionPrompt": sess.usage.prompt, "sessionCompletion": sess.usage.completion,
            "totalPrompt": store.totals.prompt, "totalCompletion": store.totals.completion,
            "totalCalls": store.totals.calls
        })),
    );
}

fn friendly_llm(e: String) -> String {
    if e.contains("会话未能建立") {
        "登录会话已失效，请重新打开 OneTHU 登录后继续。".into()
    } else if e.contains("401") || e.to_lowercase().contains("unauthorized") {
        "模型 API 鉴权失败（401）：请检查 API Key。".into()
    } else if e.contains("404") {
        "模型接口 404：请检查 API Endpoint（一般应以 /v1 结尾）。".into()
    } else {
        e
    }
}

fn system_prompt(cfg: &Config) -> String {
    format!(
        "你是小OH（OneTHU Harness）——清华园校园助手，运行在 OneTHU 应用内。用简体中文、简洁准确地回答。\n\
         当前时间：{now}。\n\
         \n\
         能力（通过工具调用）：课表/考试/成绩/校历/重要事项、校园卡余额与流水、宿舍电费\
         （缴费记录；剩余电量暂未开放）、校园网用量、新闻检索与订阅流（每条附 onethu-news://\
         站内链接）、空教室、图书馆座位与研讨间查询/预约/取消、网络学堂全覆盖（跨学期课程/\
         作业/通知/文件/讨论区版面/帖子/回帖）、体育场馆场景/可约场地/我的预约查询与取消\
         （预约在官方页面完成，jump_venue_booking 跳转）、宿舍公共空间查询/预约/取消、\
         CourseX 任意学期课程时间地点、选课目录/已选/选课社区评价（评课独立可查）、\
         电子发票/银行代发/研究生收入/宿舍卫生/体测/教学评估、应用内跳转、toast 提示。\n\
         \n\
         工作规范：\n\
         1. 涉及数据的问题一律先调用工具查询，绝不编造；查询结果要如实转述，失败时说明原因。\n\
         2. 日期表达（今天/明天/下周三等）直接传给工具即可，工具会用本地时钟精确换算；\
         图书馆座位只支持今天或明天。\n\
         3. 图书馆与研讨间的预订工具使用索引定位：必须先用对应的 query 工具拿到 \
         libIdx/floorIdx/sectionIdx/seatIdx（或 kindIdx/roomIdx），再把索引原样传给预订工具。\n\
         4. 预订/取消是写操作：你调用后系统会让用户确认，确认通过才真正执行。\
         你应在回答里向用户交代将要做的事，并等用户确认。\n\
         5. 需要多个信息时尽量合并调用工具，减少往返；查询结果较长时总结要点回答，不要全文粘贴。\n\
         6. 涉及资金（饭卡/网费/电费）只读不写、绝不代充——但允许导航到官方充值\
         入口（校园卡充值界面 = navigate life + lifeTab=card），支付由用户本人完成。\n\
         7. 回答新闻类问题时每条用 markdown 链接给 link 字段（[标题](link)，\
         onethu-news:// 应用内直达新闻详情页），不要给外部原文 URL。\n\
         \n\
         模型：{model}；上下文预算约 {ctx} tokens；单次任务最多 {steps} 步工具调用。",
        now = now_line(),
        model = cfg.model,
        ctx = cfg.max_context,
        steps = cfg.max_steps,
    )
}

fn run_usage_payload(u: &Usage, cost: f64) -> Value {
    json!({ "prompt": u.prompt, "completion": u.completion, "calls": u.calls, "costUsd": cost })
}
fn session_usage_payload(store: &Store, id: &str) -> Value {
    store
        .sessions
        .iter()
        .find(|s| s.id == id)
        .map(|s| json!({ "prompt": s.usage.prompt, "completion": s.usage.completion, "costUsd": s.cost_usd }))
        .unwrap_or(Value::Null)
}
fn total_usage_payload(store: &Store, cfg: &Config) -> Value {
    json!({
        "prompt": store.totals.prompt, "completion": store.totals.completion,
        "calls": store.totals.calls, "costUsd": store.totals_cost_usd,
        "budgetUsd": cfg.budget_usd,
        "budgetLeftUsd": (cfg.budget_usd - store.totals_cost_usd).max(0.0)
    })
}

/// chat 命令主入口：返回 dock 约定的 JSON 结构
pub fn chat(h: &mut dyn Host, emit: &dyn Emit, input: &str) -> Value {
    // 设置每次取最新（R3：用户在管理页改完即生效）
    let settings: Value = match h.call("settings", "get", json!([])) {
        Ok(v) => v,
        Err(e) => return json!({ "type": "chat", "ok": false, "error": format!("读取设置失败：{e}") }),
    };
    let map = settings.as_object().cloned().unwrap_or_default();
    let cfg = Config::from_settings(&map);
    if cfg.api_key.is_empty() {
        return json!({ "type": "chat", "ok": false, "error": "尚未配置 API Key：请到 设置→插件→OneTHU Harness 展开卡片填写后重试。" });
    }

    let mut store = Store::load(h);
    let title_from = input.chars().take(24).collect::<String>();
    {
        let s = store.ensure_active();
        if s.title == "新会话" || s.messages.is_empty() {
            s.title = title_from;
        }
    }

    // ── 待确认动作拦截（场景定制③：写操作两段式确认）──
    let pending = store.active_mut().and_then(|s| s.pending.clone());
    if let Some(p) = pending {
        let t = input.trim();
        let yes = matches!(t, "确认" | "确定" | "是" | "y" | "Y" | "yes" | "ok" | "OK" | "好" | "同意" | "执行");
        let no = matches!(t, "取消" | "不" | "n" | "N" | "no" | "算了" | "放弃" | "不了");
        if yes || no {
            let sess_id = store.active.clone();
            if no {
                let ans = format!("已取消：{}", p.summary);
                let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
                sess.pending = None;
                sess.messages.push(Msg::user("取消"));
                sess.messages.push(Msg::assistant(&ans));
                sess.updated_at = session::now_ms();
                store.save(h);
                return json!({ "type": "chat", "ok": true, "answer": ans, "sessionId": sess_id, "confirm": Value::Null });
            }
            progress(emit, "tool", &format!("执行已确认：{}", p.summary), None);
            let outcome = {
                let mut ctx = Ctx { h };
                tools::execute(&mut ctx, &p.tool, &p.args, true)
            };
            let (ans, ok) = match outcome {
                Ok(ToolOut::Text(t)) => (format!("✅ 已执行：{}\n{}", p.summary, humanize_tool_text(&t)), true),
                Ok(ToolOut::ConfirmNeeded { .. }) => (format!("已处理：{}", p.summary), true),
                Err(e) => (format!("❌ 执行失败：{}\n操作未生效，可重试或换一个目标。", e), false),
            };
            let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
            sess.pending = None;
            sess.messages.push(Msg::user("确认"));
            sess.messages.push(Msg::assistant(&ans));
            sess.trace.push(format!("✔ {}", p.summary));
            sess.updated_at = session::now_ms();
            store.save(h);
            return json!({ "type": "chat", "ok": true, "answer": ans, "sessionId": sess_id, "executed": ok, "confirm": Value::Null });
        }
        // 用户没接茬而说了别的 → 清 pending，正常进 agent 循环
        if let Some(s) = store.active_mut() {
            s.pending = None;
        }
    }

    // ── 预算闸门（R6：到量自动停）──
    if store.totals_cost_usd >= cfg.budget_usd {
        return json!({
            "type": "chat", "ok": false,
            "error": format!("已达 Token 预算上限（已用 {:.4} $ / 预算 {:.2} $）。可在插件设置里调高「预算」。", store.totals_cost_usd, cfg.budget_usd)
        });
    }

    // ── 快照（LLM 失败可回滚，不污染历史）──
    let sess_id = store.active.clone();
    let snapshot: Vec<Msg> = store.sessions.iter().find(|s| s.id == sess_id).map(|s| s.messages.clone()).unwrap_or_default();
    {
        let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
        sess.messages.push(Msg::user(input));
        sess.trace.push(format!("👤 {}", input.chars().take(40).collect::<String>()));
        if sess.trace.len() > session::MAX_TRACE {
            let cut = sess.trace.len() - session::MAX_TRACE;
            sess.trace.drain(0..cut);
        }
    }

    let sys = Msg {
        role: "system".into(),
        content: Some(system_prompt(&cfg)),
        reasoning: None,
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    let tools_schema = tools::schema();

    let mut run_usage = Usage::default();
    let mut run_cost = 0.0f64;
    let mut interrupted = false;
    let mut answer = String::new();
    let mut step: u32 = 0;

    loop {
        if h.interrupted() {
            interrupted = true;
            answer = "（已打断）".to_string();
            break;
        }
        step += 1;
        if step > cfg.max_steps {
            answer = "已达到单次任务的工具调用步数上限，任务中止。你可以让我继续或换个问法。".to_string();
            break;
        }
        let mut msgs: Vec<Msg> = vec![sys.clone()];
        {
            let sess = store.sessions.iter().find(|s| s.id == sess_id).unwrap();
            msgs.extend(sess.messages.iter().cloned());
        }
        session::trim_history(&mut msgs, cfg.max_context);

        progress(emit,
            "notice",
            &format!("思考中…（第 {step} 步）"),
            Some(json!({ "step": step, "total": cfg.max_steps })),
        );

        // 流式钩子（R4）：核心侧已按 ~120ms 批处理增量事件，这里全部直通——
        // 二次缓冲只会徒增首字延迟（之前 160 字缓冲把流式感拖没了）
        let mut hooks = Hooks {
            on_log: Box::new(|l: &str| progress(emit, "log", l, None)),
            on_delta: Box::new(|d: &str| progress(emit, "delta", d, None)),
            on_think: Box::new(|d: &str| progress(emit, "think", d, None)),
        };
        let turn = llm::chat_turn(h, &cfg, &msgs, &tools_schema, &mut hooks)
            .map_err(|e| friendly_llm(e.message));

        // 打断先行判定：stream 可能因打断提前返回——回滚本轮输入，明确报「已打断」
        if h.interrupted() {
            let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
            sess.messages = snapshot.clone();
            sess.trace.pop();
            store.save(h);
            return json!({ "type": "chat", "ok": true, "answer": "（已打断）", "interrupted": true, "sessionId": sess_id });
        }
        let turn = match turn {
            Ok(t) => llm::ensure_nonempty(t, false).map_err(|e| friendly_llm(e.message)),
            Err(e) => Err(e),
        };
        let turn = match turn {
            Ok(t) => t,
            Err(e) => {
                let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
                sess.messages = snapshot.clone();
                sess.trace.pop();
                store.save(h);
                return json!({ "type": "chat", "ok": false, "error": e, "sessionId": sess_id });
            }
        };

        if let Some(u) = &turn.usage {
            let au = Usage { prompt: u.prompt_tokens, completion: u.completion_tokens, calls: 1 };
            run_usage.add(&au);
            run_cost += usage::cost_usd(&au, cfg.price_in, cfg.price_out);
        }

        // 无工具调用 = 最终回答
        if turn.tool_calls.is_empty() {
            answer = if turn.content.trim().is_empty() && !turn.reasoning.is_empty() {
                "（模型只输出了思考过程，没有给出最终回答，可重试或换模型。）".to_string()
            } else {
                turn.content.clone()
            };
            let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
            sess.messages.push(Msg::assistant(&answer));
            sess.updated_at = session::now_ms();
            (hooks.on_log)("💾 会话写盘…");
            store.save(h);
            (hooks.on_log)("💾 写盘完成");
            break;
        }

        // 登记 assistant(tool_calls)
        let tc_json: Vec<Value> = turn
            .tool_calls
            .iter()
            .map(|tc| json!({
                "id": tc.id, "type": "function",
                "function": { "name": tc.name, "arguments": tc.arguments }
            }))
            .collect();
        {
            let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
            sess.messages.push(Msg {
                role: "assistant".into(),
                content: if turn.content.is_empty() { None } else { Some(turn.content.clone()) },
                reasoning: if turn.reasoning.is_empty() { None } else { Some(turn.reasoning.clone()) },
                tool_calls: Some(Value::Array(tc_json)),
                tool_call_id: None,
                name: None,
            });
        }

        // 逐个执行工具（确认型工具命中即停，等用户确认）
        let mut confirm: Option<(String, String, Value)> = None;
        let mut answered: Vec<String> = Vec::new();
        for tc in &turn.tool_calls {
            if h.interrupted() {
                interrupted = true;
                break;
            }
            let args: Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({ "_raw": tc.arguments }));
            progress(emit, "tool", &format!("🔧 {}", tc.name), Some(json!({ "step": step, "total": cfg.max_steps })));
            let t_tool = std::time::Instant::now();
            let args_brief: String = serde_json::to_string(&args).unwrap_or_default().chars().take(160).collect();
            (hooks.on_log)(&format!("→ 工具 {} · 步骤 {} · 参数 {}", tc.name, step, args_brief));
            let outcome = {
                let mut ctx = Ctx { h };
                tools::execute(&mut ctx, &tc.name, &args, false)
            };
            (hooks.on_log)(&format!(
                "← 工具 {} 完成 · {}ms",
                tc.name,
                t_tool.elapsed().as_millis()
            ));
            match outcome {
                Ok(ToolOut::Text(text)) => {
                    let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
                    sess.messages.push(Msg::tool_result(&tc.id, &text));
                    sess.trace.push(format!("🔧 {} → {}B", tc.name, text.len()));
                    answered.push(tc.id.clone());
                    let brief: String = text.chars().take(140).collect();
                    progress(emit, "tool", &brief, None);
                }
                Ok(ToolOut::ConfirmNeeded { tool, args, summary }) => {
                    let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
                    sess.messages.push(Msg::tool_result(
                        &tc.id,
                        &format!("{{\"status\":\"awaiting_confirmation\",\"summary\":\"{}\"}}", summary.replace('"', "'")),
                    ));
                    sess.trace.push(format!("⏸ {}（等确认）", summary));
                    answered.push(tc.id.clone());
                    confirm = Some((tool, summary, args));
                }
                Err(e) => {
                    let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
                    sess.messages.push(Msg::tool_result(&tc.id, &format!("错误：{e}")));
                    sess.trace.push(format!("✗ {}：{}", tc.name, e));
                    answered.push(tc.id.clone());
                    progress(emit, "tool", &format!("✗ {}：{}", tc.name, e), None);
                }
            }
            if confirm.is_some() {
                break;
            }
        }
        // 打断时给未执行的工具调用补占位结果，保持消息配对合法
        if interrupted {
            let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
            for tc in &turn.tool_calls {
                if !answered.contains(&tc.id) {
                    sess.messages.push(Msg::tool_result(&tc.id, "（用户打断，未执行）"));
                }
            }
        }
        {
            let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
            sess.updated_at = session::now_ms();
        }
        store.save(h);

        if let Some((tool, summary, args)) = confirm {
            let ans = format!("请确认以下操作（回复「确认」执行，回复「取消」放弃）：\n{}", summary);
            let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
            sess.pending = Some(PendingAction { tool, args, summary: summary.clone() });
            sess.messages.push(Msg::assistant(&ans)); // 模板回答，不烧 token
            sess.updated_at = session::now_ms();
            store.save(h);
            return json!({
                "type": "chat", "ok": true, "answer": ans, "sessionId": sess_id,
                "confirm": { "summary": summary },
                "usage": run_usage_payload(&run_usage, run_cost),
                "sessionUsage": session_usage_payload(&store, &sess_id),
                "totalUsage": total_usage_payload(&store, &cfg)
            });
        }
        if interrupted {
            answer = "（已打断，本轮工具调用已中止）".to_string();
            let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
            sess.messages.push(Msg::assistant(&answer));
            break;
        }
    }

    // 用量结算（R6）
    {
        let sess = store.sessions.iter_mut().find(|s| s.id == sess_id).unwrap();
        sess.usage.add(&run_usage);
        sess.cost_usd += run_cost;
        store.totals.add(&run_usage);
        store.totals_cost_usd += run_cost;
    }
    store.save(h);
    {
        let sess = store.sessions.iter().find(|s| s.id == sess_id).unwrap();
        usage_progress(emit, sess, &store, &cfg);
    }
    json!({
        "type": "chat", "ok": true, "answer": answer, "interrupted": interrupted,
        "sessionId": sess_id,
        "usage": run_usage_payload(&run_usage, run_cost),
        "sessionUsage": session_usage_payload(&store, &sess_id),
        "totalUsage": total_usage_payload(&store, &cfg)
    })
}

/* ── 其余 dock 命令 ── */

pub fn new_session(h: &mut dyn Host) -> Value {
    let mut store = Store::load(h);
    let s = store.new_session("新会话");
    let id = s.id.clone();
    store.save(h);
    json!({ "type": "session", "ok": true, "sessionId": id, "sessions": session_list(&store) })
}

pub fn list_sessions(h: &mut dyn Host) -> Value {
    let store = Store::load(h);
    json!({ "type": "sessions", "ok": true, "active": store.active, "sessions": session_list(&store) })
}

fn session_list(store: &Store) -> Vec<Value> {
    store
        .sessions
        .iter()
        .map(|s| json!({
            "id": s.id, "title": s.title, "updatedAt": s.updated_at,
            "messages": s.messages.iter().filter(|m| m.role == "user").count(),
            "costUsd": s.cost_usd
        }))
        .collect()
}

pub fn switch_session(h: &mut dyn Host, id: &str) -> Value {
    let mut store = Store::load(h);
    let ok = store.switch(id);
    if ok {
        store.save(h);
    }
    json!({ "type": "session", "ok": ok, "sessionId": if ok { json!(id) } else { Value::Null }, "sessions": session_list(&store) })
}

pub fn delete_session(h: &mut dyn Host, id: &str) -> Value {
    let mut store = Store::load(h);
    let ok = store.delete(id);
    if ok {
        store.save(h);
    }
    json!({ "type": "sessions", "ok": ok, "active": store.active, "sessions": session_list(&store) })
}

/// 导出完整会话上下文（R5：JSON）
pub fn export_session(h: &mut dyn Host, id: &str) -> Value {
    let store = Store::load(h);
    let target = if id.is_empty() { store.active.clone() } else { id.to_string() };
    match store.sessions.iter().find(|s| s.id == target) {
        Some(s) => json!({ "type": "export", "ok": true, "sessionId": s.id, "title": s.title, "json": serde_json::to_string(&s).unwrap_or_default() }),
        None => json!({ "type": "export", "ok": false, "error": "会话不存在" }),
    }
}

/// 导入完整会话上下文（R5：JSON）
pub fn import_session(h: &mut dyn Host, text: &str) -> Value {
    let mut store = Store::load(h);
    match serde_json::from_str::<session::Session>(text.trim()) {
        Ok(mut s) => {
            s.id = format!("i{}", session::now_ms());
            s.pending = None;
            let title = s.title.clone();
            store.sessions.push(s);
            store.active = store.sessions.last().unwrap().id.clone();
            store.save(h);
            json!({ "type": "session", "ok": true, "sessionId": store.active.clone(), "title": title, "sessions": session_list(&store) })
        }
        Err(e) => json!({ "type": "session", "ok": false, "error": format!("导入失败：{e}") }),
    }
}

pub fn usage_report(h: &mut dyn Host) -> Value {
    let store = Store::load(h);
    json!({
        "type": "usage", "ok": true,
        "totals": { "prompt": store.totals.prompt, "completion": store.totals.completion,
                    "calls": store.totals.calls, "costUsd": store.totals_cost_usd },
        "sessions": session_list(&store)
    })
}

/// 内置自检（不依赖网络）：日期解析 / 上下文裁剪 / 用量换算
pub fn selftest() -> Value {
    let t = config::today();
    let tomorrow = t + chrono::Duration::days(1);
    let mut results: Vec<Value> = Vec::new();
    let mut push = |ok: bool, name: &str| results.push(json!({ "name": name, "ok": ok }));
    push(config::resolve_date("明天") == Some(tomorrow), "resolve 明天");
    push(config::resolve_date("今天") == Some(t), "resolve 今天");
    push(config::resolve_date("2025-09-08").is_some(), "resolve YYYY-MM-DD");
    push(config::resolve_date("9月8日").is_some(), "resolve M月D日");
    push(config::resolve_hhmm("14:30") == Some((14, 30)), "hhmm 14:30");
    push(config::resolve_hhmm("下午2点半") == Some((14, 30)), "hhmm 下午2点半");
    push(config::date_choice("明天") == Ok(1), "dateChoice 明天=1");
    let mut msgs = vec![
        Msg { role: "system".into(), content: Some("sys".repeat(10)), reasoning: None, tool_calls: None, tool_call_id: None, name: None },
        Msg::user(&"长".repeat(500)),
        Msg::assistant(&"长".repeat(500)),
        Msg::user("最新问题"),
    ];
    session::trim_history(&mut msgs, 200);
    push(msgs.last().and_then(|m| m.content.clone()) == Some("最新问题".into()), "trim 保留最新");
    let u = Usage { prompt: 1_000_000, completion: 100_000, calls: 2 };
    let c = usage::cost_usd(&u, 0.27, 1.10);
    push((c - 0.38).abs() < 1e-9, "价格换算 0.27/1.10");
    let pass = results.iter().filter(|r| r.get("ok").and_then(|o| o.as_bool()).unwrap_or(false)).count();
    json!({
        "type": "selftest", "ok": pass == results.len(),
        "pass": pass, "total": results.len(),
        "results": results, "today": t.to_string(),
        "estTokens": est_tokens("hello 你好")
    })
}

/// R10：确认执行的回复面向用户——工具结果 JSON 只提炼可读字段，不再整坨灌屏。
/// 非结构化文本截断到 220 字。
fn humanize_tool_text(t: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(t) {
        Ok(v @ serde_json::Value::Object(_)) => {
            let mut parts: Vec<String> = Vec::new();
            for key in ["seat", "section", "time", "status", "cancelled", "kind", "date"] {
                if let Some(x) = v.get(key).and_then(|x| x.as_str()) {
                    if !x.is_empty() {
                        parts.push(x.to_string());
                    }
                }
            }
            if parts.is_empty() { None } else { Some(parts.join(" · ")) }
        }
        _ => None,
    };
    match compact {
        Some(c) => c,
        None => {
            let n = t.chars().count();
            if n > 220 {
                format!("{}…（全文 {} 字略）", t.chars().take(220).collect::<String>(), n)
            } else {
                t.to_string()
            }
        }
    }
}
