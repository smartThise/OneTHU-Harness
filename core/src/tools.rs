//! 校园工具集：每个工具 = 一次或一串 onethu.call。
//!
//! 场景定制：
//! ② **对象索引定位**——图书馆/研讨间的「对象传递」链（list→floors→sections→seats→book）
//!    对模型暴露为纯索引（libIdx/floorIdx/sectionIdx/seatIdx/roomIdx），工具在 Rust 侧
//!    按 index 取回真实对象再调宿主 API，模型永远不接触也伪造不了复杂对象；
//! ③ **写操作两段式确认**——book/cancel 第一次调用只生成确认摘要（含具体对象与时间），
//!    用户确认后才真正执行；
//! ④ **结果聚合压缩**——座位按区域汇总余位、流水汇总收支，控制回灌给模型的体积。

use crate::config::{date_choice, resolve_date, resolve_hhmm, today};
use crate::host::Host;
use chrono::Duration;
use serde_json::{json, Value};

pub struct ToolDef {
    pub name: &'static str,
    pub desc: &'static str,
    pub params: Value,
    pub confirm: bool,
}

pub struct Ctx<'a> {
    pub h: &'a mut dyn Host,
}

pub enum ToolOut {
    /// 直接回给模型的工具结果文本
    Text(String),
    /// 需要用户确认（场景定制③）：登记 pending 后回给模型摘要
    ConfirmNeeded { tool: String, args: Value, summary: String },
}

fn host(ctx: &mut Ctx, ns: &str, method: &str, args: Value) -> Result<Value, String> {
    ctx.h.call(ns, method, args).map_err(friendly)
}

fn friendly(e: String) -> String {
    if e.contains("会话未能建立") {
        "登录会话已失效：请打开 OneTHU 重新登录后再试。".to_string()
    } else if e.contains("未获授权") {
        format!("{e}（请在插件管理页重装/检查权限）")
    } else {
        e
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
fn n(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(|x| x.as_i64()).unwrap_or(0)
}

pub fn all_tools() -> Vec<ToolDef> {
    let p = |props: Value, req: &[&str]| -> Value {
        let mut v = json!({ "type": "object", "properties": props });
        if !req.is_empty() {
            v["required"] = json!(req);
        }
        v
    };
    vec![
        ToolDef {
            name: "query_profile",
            desc: "查当前登录用户的基本信息（姓名/学号/院系/邮箱）与会话状态",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_schedule",
            desc: "查课表。start/end 接受 今天/明天/下周三/2025-09-08 等表达；缺省为今天起一周",
            params: p(json!({
                "start": {"type": "string", "description": "起始日期，如 明天 / 2025-09-08"},
                "end": {"type": "string", "description": "结束日期（含）"}
            }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_exams",
            desc: "查考试安排（课程/日期/时间/地点）",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_deadlines",
            desc: "查学校重要事项与倒计时（注册、缴费等）",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_grades",
            desc: "查成绩单（全部学期：课程/学分/成绩/绩点/学期）",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_calendar",
            desc: "查校历（学期起止、下学期安排）",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_card_balance",
            desc: "查校园卡余额",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_card_transactions",
            desc: "查校园卡消费/充值流水（自动汇总收支）。start/end 缺省为近 7 天",
            params: p(json!({
                "start": {"type": "string"}, "end": {"type": "string"}
            }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_dorm_ele_records",
            desc: "查宿舍电费缴费记录（时间/金额/渠道/状态）。注意：剩余电量查询暂未开放（电子身份系统需单独登录），用户问余额时说明这一点",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_network",
            desc: "查校园网：套餐用量、账户余额、在线设备数",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_news",
            desc: "查校园新闻列表；给 keyword 则搜索，否则取最新一页",
            params: p(json!({
                "keyword": {"type": "string"}, "page": {"type": "integer", "minimum": 1}
            }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_subscribed_news",
            desc: "查用户的订阅新闻流（信息页订阅源聚合；给 page 翻页）。返回含 url，回答时每条附链接",
            params: p(json!({
                "page": {"type": "integer", "minimum": 1}
            }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "read_news_article",
            desc: "读一篇新闻的正文（需要 query_news 返回的 xxid）",
            params: p(json!({ "news_id": {"type": "string"} }), &["news_id"]),
            confirm: false,
        },
        ToolDef {
            name: "query_empty_classrooms",
            desc: "查教学楼空教室（按节次汇总空闲情况）。building 用楼栋名关键词（如 六教）",
            params: p(json!({
                "building": {"type": "string"},
                "week": {"type": "integer", "description": "周次，缺省=本周"}
            }), &["building"]),
            confirm: false,
        },
        ToolDef {
            name: "query_library",
            desc: "查各图书馆今日/明天的楼层余位总览（libIdx/floorIdx 用于后续工具）",
            params: p(json!({
                "date": {"type": "string", "description": "今天/明天，缺省今天"}
            }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_library_seats",
            desc: "查某馆某楼层的区域与空闲座位（sectionIdx/seatIdx 用于订座）",
            params: p(json!({
                "libIdx": {"type": "integer", "minimum": 0},
                "floorIdx": {"type": "integer", "minimum": 0},
                "date": {"type": "string"}
            }), &["libIdx", "floorIdx"]),
            confirm: false,
        },
        ToolDef {
            name: "book_library_seat",
            desc: "预约图书馆座位（需要先 query_library_seats 拿到索引；执行前会请用户确认）",
            params: p(json!({
                "libIdx": {"type": "integer"}, "floorIdx": {"type": "integer"},
                "sectionIdx": {"type": "integer"}, "seatIdx": {"type": "integer"},
                "date": {"type": "string"}
            }), &["libIdx", "floorIdx", "sectionIdx", "seatIdx"]),
            confirm: true,
        },
        ToolDef {
            name: "my_library_bookings",
            desc: "查我的图书馆座位预约记录（bookingIdx 用于取消）",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "cancel_library_booking",
            desc: "取消一条图书馆座位预约（执行前会请用户确认）",
            params: p(json!({ "bookingIdx": {"type": "integer", "minimum": 0} }), &["bookingIdx"]),
            confirm: true,
        },
        ToolDef {
            name: "query_librooms",
            desc: "查研讨间/音乐室等房间资源（date 必填；kind 可选关键词，如 研讨间/音乐室）",
            params: p(json!({
                "date": {"type": "string"},
                "kind": {"type": "string"}
            }), &["date"]),
            confirm: false,
        },
        ToolDef {
            name: "book_libroom",
            desc: "预约研讨间（roomIdx 来自 query_librooms；成员传姓名/学号关键词数组；执行前会请用户确认）",
            params: p(json!({
                "date": {"type": "string"},
                "roomIdx": {"type": "integer", "minimum": 0},
                "start": {"type": "string", "description": "如 14:00 / 下午2点"},
                "end": {"type": "string"},
                "members": {"type": "array", "items": {"type": "string"}, "description": "同伴姓名/学号关键词"}
            }), &["date", "roomIdx", "start", "end"]),
            confirm: true,
        },
        ToolDef {
            name: "my_libroom_bookings",
            desc: "查我的研讨间预约记录（recordIdx 用于取消）",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "cancel_libroom_booking",
            desc: "取消一条研讨间预约（执行前会请用户确认）",
            params: p(json!({ "recordIdx": {"type": "integer", "minimum": 0} }), &["recordIdx"]),
            confirm: true,
        },
        ToolDef {
            name: "navigate",
            desc: "在 OneTHU 应用内跳转页面（today/learn/schedule/info/life/reserve/settings 等，可带参数）。资金类只读红线不拦导航：如校园卡充值界面 = navigate life + params {\"lifeTab\":\"card\"}（充值操作由用户在官方界面完成，助手不代充）",
            params: p(json!({
                "page": {"type": "string"},
                "params": {"type": "object", "description": "如 {\"reserveTab\":\"lib\"}"}
            }), &["page"]),
            confirm: false,
        },
        ToolDef {
            name: "notify",
            desc: "在应用底部弹一条 toast 提示",
            params: p(json!({ "text": {"type": "string"} }), &["text"]),
            confirm: false,
        },
    ]
}

/// OpenAI tools 参数
pub fn schema() -> Vec<Value> {
    all_tools()
        .into_iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": { "name": t.name, "description": t.desc, "parameters": t.params }
            })
        })
        .collect()
}

pub fn is_confirm(name: &str) -> bool {
    all_tools().iter().find(|t| t.name == name).map(|t| t.confirm).unwrap_or(false)
}

/// 两段式确认的确认摘要（把对象解析好再给用户看）
fn build_summary(name: &str, args: &Value, ctx: &mut Ctx) -> Result<String, String> {
    match name {
        "book_library_seat" => {
            let (seat, sec, floor, lib) = resolve_seat(ctx, args)?;
            let date = {
                let raw = s(args, "date");
                match resolve_date(&or_default(&raw, "今天")) {
                    Some(d) if d == today() => "今天".to_string(),
                    Some(d) if d == today() + Duration::days(1) => "明天".to_string(),
                    Some(d) => d.format("%Y-%m-%d").to_string(),
                    None => match args.get("dateChoice").and_then(|x| x.as_i64()) {
                        Some(1) => "明天".to_string(),
                        _ => "今天".to_string(),
                    },
                }
            };
            let _ = (&seat, &sec, &floor, &lib);
            let seat_type = {
                let t = s(&seat, "type");
                if t.is_empty() { "普通".into() } else { t }
            };
            Ok(format!(
                "预约图书馆座位：{}{} {} {}（{}，座位类型 {}）",
                s(&lib, "zhName"),
                s(&floor, "zhName"),
                s(&sec, "zhName"),
                s(&seat, "zhName"),
                date,
                seat_type
            ))
        }
        "cancel_library_booking" => {
            let recs = host(ctx, "library", "records", json!([]))?;
            let arr = arr_of(&recs);
            let idx = n(args, "bookingIdx") as usize;
            let rec = arr.get(idx).ok_or("预约记录序号不存在，请先 my_library_bookings 查询")?;
            Ok(format!("取消图书馆座位预约：{} @ {}（{}）", s(rec, "pos"), s(rec, "time"), s(rec, "status")))
        }
        "book_libroom" => {
            let (room, date) = resolve_room(ctx, args)?;
            let members = args.get("members").and_then(|m| m.as_array()).cloned().unwrap_or_default();
            let room_name = {
                let r1 = s(&room, "roomName");
                if r1.is_empty() { s(&room, "devName") } else { r1 }
            };
            Ok(format!(
                "预约研讨间：{}（{} {}-{}，成员 {} 人，不含本人）",
                room_name,
                date,
                s(args, "start"),
                s(args, "end"),
                members.len()
            ))
        }
        "cancel_libroom_booking" => {
            let recs = host(ctx, "libroom", "records", json!([]))?;
            let arr = arr_of(&recs);
            let idx = n(args, "recordIdx") as usize;
            let rec = arr.get(idx).ok_or("记录序号不存在，请先 my_libroom_bookings 查询")?;
            Ok(format!(
                "取消研讨间预约：{} {} {}-{}",
                s(rec, "devName"),
                s(rec, "date"),
                s(rec, "begin"),
                s(rec, "end")
            ))
        }
        _ => Err(format!("未知确认工具：{name}")),
    }
}

/// 取字符串字段；为空时回退为整个对象的紧凑 JSON（预约失败原文展示用）
fn s_or_raw(v: &Value, k: &str) -> String {
    let x = s(v, k);
    if x.is_empty() { pretty(v) } else { x }
}

fn arr_of(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn resolve_seat(ctx: &mut Ctx, args: &Value) -> Result<(Value, Value, Value, Value), String> {
    let libs = arr_of(&host(ctx, "library", "list", json!([]))?);
    let lib = libs.get(n(args, "libIdx") as usize).ok_or("libIdx 越界")?;
    let date_raw = s(args, "date");
    let dc = date_choice(&or_default(&date_raw, "今天"))? as i64;
    let floors = arr_of(&host(ctx, "library", "floors", json!([n(lib, "id"), dc]))?);
    let floor = floors.get(n(args, "floorIdx") as usize).ok_or("floorIdx 越界")?;
    let sections = arr_of(&host(ctx, "library", "sections", json!([floor, dc]))?);
    let sec = sections.get(n(args, "sectionIdx") as usize).ok_or("sectionIdx 越界")?;
    let seats = arr_of(&host(ctx, "library", "seats", json!([sec, dc]))?);
    let seat = seats.get(n(args, "seatIdx") as usize).ok_or("seatIdx 越界")?;
    Ok((seat.clone(), sec.clone(), floor.clone(), lib.clone()))
}

fn resolve_room(ctx: &mut Ctx, args: &Value) -> Result<(Value, String), String> {
    let date_raw = s(args, "date");
    let date = resolve_date(&date_raw).ok_or(format!("无法理解日期「{date_raw}」"))?;
    let date_str = date.format("%Y-%m-%d").to_string();
    let kinds = arr_of(&host(ctx, "libroom", "list", json!([]))?);
    // roomIdx 对应 query_librooms 输出里该 kind 下的 rooms 顺序；这里要求 args 带 kindIdx
    let kind_idx = n(args, "kindIdx") as usize;
    let kind = kinds.get(kind_idx).ok_or("kindIdx 越界（请重新 query_librooms）")?;
    let rooms = arr_of(&host(ctx, "libroom", "resources", json!([date_str, n(kind, "kindId")]))?);
    let room = rooms.get(n(args, "roomIdx") as usize).ok_or("roomIdx 越界（请重新 query_librooms）")?;
    Ok((room.clone(), date_str))
}

/// 执行工具。confirmed=true 表示用户已确认（仅确认型工具使用）。
pub fn execute(ctx: &mut Ctx, name: &str, args: &Value, confirmed: bool) -> Result<ToolOut, String> {
    if is_confirm(name) && !confirmed {
        let summary = build_summary(name, args, ctx)?;
        return Ok(ToolOut::ConfirmNeeded { tool: name.into(), args: args.clone(), summary });
    }
    let out: Value = match name {
        "query_profile" => {
            let status: Value = host(ctx, "session", "status", json!([]))?;
            let username: Value = host(ctx, "session", "username", json!([]))?;
            let info = host(ctx, "user", "info", json!([]))?;
            json!({
                "sessionStatus": status, "username": username,
                "name": s(&info, "name"), "studentId": s(&info, "studentId"),
                "department": s(&info, "department"), "major": s(&info, "major"), "email": s(&info, "email")
            })
        }
        "query_schedule" => {
            let start = resolve_date(&or_default(&s(args, "start"), "今天")).ok_or("无法理解 start 日期")?;
            let end = if s(args, "end").is_empty() {
                start + Duration::days(6)
            } else {
                resolve_date(&s(args, "end")).ok_or("无法理解 end 日期")?
            };
            host(ctx, "info", "schedule", json!([start.format("%Y-%m-%d").to_string(), end.format("%Y-%m-%d").to_string()]))?
        }
        "query_exams" => host(ctx, "info", "exams", json!([]))?,
        "query_deadlines" => host(ctx, "info", "deadlines", json!([]))?,
        "query_grades" => host(ctx, "info", "report", json!([]))?,
        "query_calendar" => host(ctx, "info", "schoolCalendar", json!([]))?,
        "query_card_balance" => {
            let c = host(ctx, "card", "info", json!([]))?;
            json!({ "userName": s(&c, "userName"), "balance": c.get("balance").cloned().unwrap_or(json!(null)), "cardStatus": s(&c, "cardStatus") })
        }
        "query_card_transactions" => {
            let start = resolve_date(&or_default(&s(args, "start"), &format!("{}", (today() - Duration::days(6)).format("%Y-%m-%d"))))
                .ok_or("无法理解 start 日期")?;
            let end = if s(args, "end").is_empty() { today() } else { resolve_date(&s(args, "end")).ok_or("无法理解 end 日期")? };
            let txs = arr_of(&host(
                ctx,
                "card",
                "transactions",
                json!([start.format("%Y-%m-%d").to_string(), end.format("%Y-%m-%d").to_string()]),
            )?);
            // 场景定制④：聚合压缩——只回灌关键字段 + 汇总
            let total: f64 = txs.iter().filter_map(|t| t.get("amount").and_then(|a| a.as_f64())).sum();
            let rows: Vec<Value> = txs
                .iter()
                .take(90)
                .map(|t| {
                    json!({
                        "summary": s(t, "summary"), "time": s(t, "timestamp"),
                        "amount": t.get("amount").cloned().unwrap_or(json!(null)), "balance": t.get("balance").cloned().unwrap_or(json!(null))
                    })
                })
                .collect();
            json!({ "count": txs.len(), "netAmount": total, "note": "amount 正负为收支方向", "rows": rows, "truncated": txs.len() > 90 })
        }
        "query_dorm_ele_records" => {
            // R10 用户决策：剩余电量暂关（电子身份单独登录卡死小OH），只开缴费记录
            let recs = arr_of(&host(ctx, "dorm", "elePayRecord", json!([]))?);
            let rows: Vec<Value> = recs
                .iter()
                .take(20)
                .map(|r| {
                    json!({
                        "time": s(r, "time"), "amount": s(r, "value"),
                        "channel": s(r, "channel"), "status": s(r, "status"),
                    })
                })
                .collect();
            json!({ "count": recs.len(), "rows": rows, "truncated": recs.len() > 20 })
        }
        "query_network" => {
            let b = host(ctx, "network", "balance", json!([]))?;
            let cnt: Value = host(ctx, "network", "deviceCount", json!([]))?;
            json!({ "productName": s(&b, "productName"), "usedBytes": b.get("usedBytes").cloned().unwrap_or(json!(null)), "accountBalance": b.get("accountBalance").cloned().unwrap_or(json!(null)), "onlineDevices": cnt })
        }
        "query_news" => {
            let kw = s(args, "keyword");
            let page = args.get("page").and_then(|p| p.as_i64()).unwrap_or(1);
            let list = if kw.is_empty() {
                host(ctx, "info", "news", json!([page]))?
            } else {
                host(ctx, "info", "searchNews", json!([kw, page]))?
            };
            let rows: Vec<Value> = arr_of(&list)
                .into_iter()
                .take(20)
                .map(|it| json!({ "xxid": s(&it, "xxid"), "name": s(&it, "name"), "date": s(&it, "date"), "source": s(&it, "source"), "url": s(&it, "url") }))
                .collect();
            json!({ "rows": rows, "note": "每条新闻务必把 url 一并给用户（可点开原文），不要只报标题" })
        }
        "read_news_article" => {
            let d = host(ctx, "info", "newsDetail", json!([s(args, "news_id")]))?;
            let text: String = d.as_str().map(|x| x.to_string()).unwrap_or_else(|| d.to_string());
            let mut cut: String = text.chars().take(3000).collect();
            if text.chars().count() > 3000 {
                cut.push_str("…（正文过长已截断）");
            }
            json!({ "text": cut })
        }
        "query_empty_classrooms" => {
            let building = s(args, "building");
            let list = arr_of(&host(ctx, "info", "classroomList", json!([]))?);
            let b = list
                .iter()
                .find(|c| s(c, "name").contains(&building) || s(c, "searchName").contains(&building))
                .ok_or(format!("找不到楼栋「{building}」，可用：{}", list.iter().map(|c| s(c, "name")).collect::<Vec<_>>().join("、")))?;
            // 教学楼自带当前周次（weekNumber），无需探测
            let week = args.get("week").and_then(|w| w.as_i64()).unwrap_or_else(|| n(b, "weekNumber"));
            let st = host(ctx, "info", "classroomState", json!([s(b, "name"), week]))?;
            let states = arr_of(st.get("classroomStates").unwrap_or(&Value::Null));
            let mut rooms: Vec<Value> = Vec::new();
            for (i, r) in states.iter().enumerate() {
                if i >= 60 {
                    break;
                }
                // ClassroomStatus：0教学占用 1考试 2借用 3维护 4保留 5空闲
                let st_arr = r.get("status").and_then(|x| x.as_array()).cloned().unwrap_or_default();
                let busy: Vec<usize> = st_arr
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| v.as_i64().map(|x| x != 5).unwrap_or(true))
                    .map(|(p, _)| p)
                    .collect();
                // R10：42 格 = 本周一~周日 × 每天 6 大节——按天归位成结构化输出。
                // （此前把原始下标直接吐给模型，模型误读成"一天 42 节"）
                let day_names = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
                let mut by_day: Vec<Value> = Vec::new();
                for (di, dname) in day_names.iter().enumerate() {
                    let periods: Vec<u32> = busy
                        .iter()
                        .filter(|&&p| p / 6 == di)
                        .map(|&p| (p % 6 + 1) as u32)
                        .collect();
                    if !periods.is_empty() {
                        by_day.push(json!({ "day": dname, "busyPeriods": periods }));
                    }
                }
                let rname = {
                    let nm = s(r, "name");
                    if nm.is_empty() { format!("教室{i}") } else { nm }
                };
                rooms.push(json!({
                    "room": rname, "capacity": n(r, "capacity"),
                    "busyByDay": by_day,
                    "freeNote": "busyByDay=被占用的大节（每格≈两大节）；未列出的天/节次均空闲"
                }));
            }
            json!({ "building": s(b, "name"), "week": week, "rooms": rooms })
        }
        "query_library" => {
            let dc = date_choice(&or_default(&s(args, "date"), "今天"))?;
            let libs = arr_of(&host(ctx, "library", "list", json!([]))?);
            let mut out: Vec<Value> = Vec::new();
            for (li, lib) in libs.iter().enumerate() {
                // R10：单馆失败只记 error 项，不拖垮整个总览（此前一馆解析失败全工具报错）
                match host(ctx, "library", "floors", json!([n(lib, "id"), dc])) {
                    Ok(floors) => {
                        let frows: Vec<Value> = arr_of(&floors)
                            .iter()
                            .enumerate()
                            .map(|(fi, f)| json!({ "floorIdx": fi, "name": s(f, "zhName"), "available": n(f, "available"), "total": n(f, "total") }))
                            .collect();
                        out.push(json!({ "libIdx": li, "id": n(lib, "id"), "name": s(lib, "zhName"), "floors": frows }));
                    }
                    Err(e) => out.push(json!({ "libIdx": li, "name": s(lib, "zhName"), "error": e })),
                }
            }
            json!({ "dateChoice": dc, "libraries": out })
        }
        "query_library_seats" => {
            let dc = date_choice(&or_default(&s(args, "date"), "今天"))? as i64;
            let libs = arr_of(&host(ctx, "library", "list", json!([]))?);
            let lib = libs.get(n(args, "libIdx") as usize).ok_or("libIdx 越界，请先 query_library")?;
            let floors = arr_of(&host(ctx, "library", "floors", json!([n(lib, "id"), dc]))?);
            let floor = floors.get(n(args, "floorIdx") as usize).ok_or("floorIdx 越界，请先 query_library")?;
            let sections = arr_of(&host(ctx, "library", "sections", json!([floor, dc]))?);
            let mut srows: Vec<Value> = Vec::new();
            for (si, sec) in sections.iter().enumerate() {
                let seats = arr_of(&host(ctx, "library", "seats", json!([sec, dc]))?);
                // 可用 = availability=="usable"（缺省看 valid）；status 字段是插座状态，不是占用
                let free_idx: Vec<usize> = seats
                    .iter()
                    .enumerate()
                    .filter(|(_, x)| {
                        let avail = x.get("availability").and_then(|a| a.as_str());
                        match avail {
                            Some(a) => a == "usable",
                            None => x.get("valid").and_then(|v| v.as_bool()).unwrap_or(true),
                        }
                    })
                    .map(|(i, _)| i)
                    .collect();
                let preview: Vec<Value> = free_idx
                    .iter()
                    .take(8)
                    .map(|&i| json!({ "seatIdx": i, "zhName": s(&seats[i], "zhName"), "type": s(&seats[i], "type"), "hasPower": seats[i].get("hasPower").cloned().unwrap_or(json!(null)) }))
                    .collect();
                srows.push(json!({
                    "sectionIdx": si, "name": s(sec, "zhName"),
                    "available": free_idx.len(), "total": seats.len(),
                    "freePreview": preview, "previewNote": "freePreview 为空位示意（seatIdx 是全区域座位数组的真实下标，可直接用于订座）"
                }));
            }
            json!({ "library": s(lib, "zhName"), "floor": s(floor, "zhName"), "dateChoice": dc, "sections": srows })
        }
        "book_library_seat" => {
            let (seat, sec, _floor, _lib) = resolve_seat(ctx, args)?;
            let dc = date_choice(&or_default(&s(args, "date"), "今天"))? as i64;
            let r = host(ctx, "library", "book", json!([seat, n(&sec, "id"), dc]))?;
            let ok = r.get("status").and_then(|x| x.as_i64()).unwrap_or(0) == 1
                || s(&r, "msg").contains("成功");
            if !ok {
                return Err(format!("预约未成功：{}", s_or_raw(&r, "msg")));
            }
            // 精简回执：raw 里有数 KB 的 spaceInfo/hash 结构，用户只需要关键事实
            let d = r.get("data").cloned().unwrap_or(json!({}));
            let seat_name = s(&seat, "zhName");
            let status_name = d.get("statusName").and_then(|x| x.as_str()).unwrap_or("预约成功");
            let time = d.get("starttime").and_then(|x| x.as_str())
                .map(|t| format!("今天 {}", t))
                .unwrap_or_else(|| "今天".into());
            json!({
                "success": true,
                "seat": seat_name,
                "section": s(&sec, "zhName"),
                "time": time,
                "status": status_name,
                "hint": "已同步到预约记录；可用 my_library_bookings 复核"
            })
        }
        "my_library_bookings" => host(ctx, "library", "records", json!([]))?,
        "cancel_library_booking" => {
            let recs = arr_of(&host(ctx, "library", "records", json!([]))?);
            let rec = recs.get(n(args, "bookingIdx") as usize).ok_or("bookingIdx 越界")?;
            let seat_name = s(rec, "pos").clone();
            // R10：取消凭据是 menuDel 的 delId（主程序同款）；id 只是展示编号，传错必被拒
            let cid = rec.get("delId")
                .and_then(|v| v.as_str().map(|x| x.to_string()).or_else(|| v.as_i64().map(|x| x.to_string())))
                .filter(|x| !x.is_empty())
                .unwrap_or_else(|| s(rec, "id"));
            if let Err(e) = host(ctx, "library", "cancel", json!([cid])) {
                let msg = e.to_string();
                // ISeating 限制每日取消 1 次：撞上限时把话说透，别让模型猜
                if msg.contains("上限") || msg.contains("次数") || msg.contains("频繁") {
                    return Err(format!(
                        "今日取消次数已达上限（座位系统限制每个图书馆每自然日取消 1 次）。{seat_name} 的预约仍在生效，明天 0 点后可再取消，或到图书馆座位系统「我的中心」手动处理。"
                    ));
                }
                return Err(msg);
            }
            json!({ "success": true, "cancelled": seat_name, "hint": "可用 my_library_bookings 复核" })
        }
        "query_librooms" => {
            let date_raw = s(args, "date");
            let date = resolve_date(&date_raw).ok_or(format!("无法理解日期「{date_raw}」"))?;
            let date_str = date.format("%Y-%m-%d").to_string();
            let kinds = arr_of(&host(ctx, "libroom", "list", json!([]))?);
            let kw = s(args, "kind");
            // R10 实录：ic-web 对连发 resources 限流（「系统繁忙，请稍后重试」）——
            // 不带 kind 时原实现对每个类型连发查询必撞限流。改为只回类型目录
            // （快、稳），让模型带 kind 指定类型后再查占用；带 kind 时逐个查、
            // 间隔 600ms，失败退避 1.5s 重试一次（只读安全）。
            if kw.is_empty() {
                let catalog: Vec<Value> = kinds
                    .iter()
                    .enumerate()
                    .map(|(ki, k)| json!({ "kindIdx": ki, "kindId": n(k, "kindId"), "kindName": s(k, "kindName") }))
                    .collect();
                json!({ "kinds": catalog, "note": "以上是全部类型目录；查某类型的房间与占用请带 kind 参数重查（如 kind=研讨间）" })
            } else {
                let matched: Vec<(usize, &Value)> = kinds
                    .iter()
                    .enumerate()
                    .filter(|(_, k)| s(k, "kindName").contains(&kw))
                    .collect();
                if matched.is_empty() {
                    return Err(format!(
                        "找不到类型「{kw}」，可用：{}",
                        kinds.iter().map(|k| s(k, "kindName")).collect::<Vec<_>>().join("、")
                    ));
                }
                let mut out: Vec<Value> = Vec::new();
                for (i, (ki, k)) in matched.iter().enumerate() {
                    if i > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(600));
                    }
                    let mut res = host(ctx, "libroom", "resources", json!([date_str, n(k, "kindId")]));
                    if res.is_err() {
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        res = host(ctx, "libroom", "resources", json!([date_str, n(k, "kindId")]));
                    }
                    let rooms = arr_of(&res?);
                    out.push(json!({ "kindIdx": ki, "kindId": n(k, "kindId"), "kindName": s(k, "kindName"), "date": date_str, "rooms": rooms }));
                }
                json!({ "kinds": out })
            }
        }
        "book_libroom" => {
            // 需要.kindIdx：约定模型须先用 query_librooms；若缺省则取第一个 kind
            let mut args2 = args.clone();
            if args2.get("kindIdx").is_none() {
                let kinds = arr_of(&host(ctx, "libroom", "list", json!([]))?);
                let kw = s(args, "kind");
                let ki = kinds
                    .iter()
                    .position(|k| kw.is_empty() || s(k, "kindName").contains(&kw))
                    .ok_or("找不到房间类型")?;
                args2["kindIdx"] = json!(ki);
            }
            let (room, date_str) = resolve_room(ctx, &args2)?;
            let room_name = {
                let r1 = s(&room, "roomName");
                if r1.is_empty() { s(&room, "devName") } else { r1 }
            };
            let start = resolve_hhmm(&s(args, "start")).ok_or("无法理解开始时间")?;
            let end = resolve_hhmm(&s(args, "end")).ok_or("无法理解结束时间")?;
            let max_min = n(&room, "maxMinute");
            let minutes = (end.0 * 60 + end.1) as i64 - (start.0 * 60 + start.1) as i64;
            if minutes <= 0 {
                return Err("结束时间需晚于开始时间".into());
            }
            if max_min > 0 && minutes > max_min {
                return Err(format!("时长 {minutes} 分钟超过该资源上限 {max_min} 分钟"));
            }
            // 成员解析：关键词 → fuzzyMember → accNo
            let mut accs: Vec<i64> = Vec::new();
            let members = args.get("members").and_then(|m| m.as_array()).cloned().unwrap_or_default();
            let limit = n(&room, "limit");
            if !members.is_empty() && limit > 0 && members.len() as i64 > limit {
                return Err(format!("成员 {0} 人超过上限 {limit}（发起人另计，按 UI 语义）", members.len()));
            }
            for m in &members {
                let kw = m.as_str().unwrap_or_default().trim().to_string();
                if kw.is_empty() {
                    continue;
                }
                let hits = arr_of(&host(ctx, "libroom", "fuzzyMember", json!([kw]))?);
                let hit = hits
                    .iter()
                    .find(|h| h.get("label").and_then(|x| x.as_str()).map(|l| l.contains(&kw)).unwrap_or(false))
                    .or_else(|| hits.first())
                    .ok_or(format!("搜不到成员「{kw}」"))?;
                // LibFuzzySearchResult.id 是数字 accNo
                let acc = hit
                    .get("id")
                    .and_then(|x| x.as_i64())
                    .or_else(|| hit.get("id").and_then(|x| x.as_str()).and_then(|x| x.trim().parse().ok()))
                    .ok_or(format!("成员 accNo 解析失败：{}", hit))?;
                accs.push(acc);
            }
            let start_str = format!("{} {:02}:{:02}", date_str, start.0, start.1);
            let end_str = format!("{} {:02}:{:02}", date_str, end.0, end.1);
            host(ctx, "libroom", "book", json!([room, start_str, end_str, accs]))?;
            json!({ "success": true, "room": room_name, "time": format!("{start_str} ~ {end_str}") })
        }
        "my_libroom_bookings" => host(ctx, "libroom", "records", json!([]))?,
        "cancel_libroom_booking" => {
            let recs = arr_of(&host(ctx, "libroom", "records", json!([]))?);
            let rec = recs.get(n(args, "recordIdx") as usize).ok_or("recordIdx 越界")?;
            host(ctx, "libroom", "cancel", json!([s(rec, "uuid")]))?;
            json!({ "success": true })
        }
        "query_subscribed_news" => {
            let page = args.get("page").and_then(|p| p.as_i64()).unwrap_or(1);
            let list = host(ctx, "info", "newsSub", json!([page]))?;
            let rows: Vec<Value> = arr_of(&list)
                .into_iter()
                .take(20)
                .map(|it| json!({ "xxid": s(&it, "xxid"), "name": s(&it, "name"), "date": s(&it, "date"), "source": s(&it, "source"), "url": s(&it, "url") }))
                .collect();
            json!({ "rows": rows, "note": "每条新闻务必把 url 一并给用户（可点开原文）" })
        }
        "navigate" => {
            let params = args.get("params").cloned().unwrap_or(json!({}));
            let ok = host(ctx, "nav", "go", json!([s(args, "page"), params]))?;
            json!({ "navigated": ok })
        }
        "notify" => {
            host(ctx, "ui", "toast", json!([s(args, "text")]))?;
            json!({ "sent": true })
        }
        other => return Err(format!("未知工具：{other}")),
    };
    Ok(ToolOut::Text(pretty(&out)))
}

fn or_default(v: &str, d: &str) -> String {
    if v.trim().is_empty() {
        d.to_string()
    } else {
        v.to_string()
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
}
