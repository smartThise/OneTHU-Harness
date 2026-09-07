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
            name: "query_learn_courses",
            desc: "查网络学堂课程列表（semesterId 缺省=当前学期；跨学期传学期 id，可先 query_learn_semesters）",
            params: p(json!({ "semesterId": {"type": "string"} }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_learn_semesters",
            desc: "查网络学堂学期 id 列表（跨学期查询用）",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_learn_homework",
            desc: "查网络学堂作业（含课程名/状态/截止时间；semesterId 缺省=当前学期）",
            params: p(json!({ "semesterId": {"type": "string"} }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_learn_notifications",
            desc: "查网络学堂课程公告（semesterId 缺省=当前学期）",
            params: p(json!({ "semesterId": {"type": "string"} }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_learn_files",
            desc: "查网络学堂某课程文件列表（course_keyword=课程名关键词，如 计算机组成）",
            params: p(json!({ "course_keyword": {"type": "string"} }), &["course_keyword"]),
            confirm: false,
        },
        ToolDef {
            name: "query_learn_bbs",
            desc: "查网络学堂讨论区：course_keyword 定课程 → 返回版面；threadId 可选直达帖子及回帖",
            params: p(json!({
                "course_keyword": {"type": "string"},
                "bqId": {"type": "string"},
                "threadId": {"type": "string"}
            }), &["course_keyword"]),
            confirm: false,
        },
        ToolDef {
            name: "query_venue_scenes",
            desc: "查体育场馆场景列表（uuid 供场地查询/跳转）",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_venue_slots",
            desc: "查体育场馆某天可约场地（sceneUuid 必填；date=YYYY-MM-DD；classTypeUuid 可选楼栋/类型过滤）",
            params: p(json!({
                "sceneUuid": {"type": "string"},
                "date": {"type": "string"},
                "classTypeUuid": {"type": "string"}
            }), &["sceneUuid", "date"]),
            confirm: false,
        },
        ToolDef {
            name: "query_venue_records",
            desc: "查我的体育场馆预约记录（含 resvUuid，取消用）",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "cancel_venue_reservation",
            desc: "取消一条体育场馆预约（执行前会请用户确认）",
            params: p(json!({ "resvUuid": {"type": "string"} }), &["resvUuid"]),
            confirm: true,
        },
        ToolDef {
            name: "jump_venue_booking",
            desc: "打开体育系统官方预约页（直达所选场馆；预约在官方页面由用户手动完成）",
            params: p(json!({ "sceneUuid": {"type": "string"} }), &["sceneUuid"]),
            confirm: false,
        },
        ToolDef {
            name: "query_kongjian",
            desc: "查宿舍公共空间（共享空间预约页：可约空间/时段；date=YYYY-MM-DD，spaceId/roomId 可选）",
            params: p(json!({
                "date": {"type": "string"},
                "spaceId": {"type": "string"},
                "roomId": {"type": "string"}
            }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_kongjian_my",
            desc: "查我的宿舍公共空间预约记录",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "book_kongjian",
            desc: "预约宿舍公共空间（执行前会请用户确认；需 bookUrl/sid/tel，name/other 可选）",
            params: p(json!({
                "bookUrl": {"type": "string"},
                "name": {"type": "string"},
                "sid": {"type": "string"},
                "tel": {"type": "string"},
                "other": {"type": "string"}
            }), &["bookUrl", "sid", "tel"]),
            confirm: true,
        },
        ToolDef {
            name: "cancel_kongjian",
            desc: "取消一条宿舍公共空间预约（执行前会请用户确认）",
            params: p(json!({ "target": {"type": "string"} }), &["target"]),
            confirm: true,
        },
        ToolDef {
            name: "query_xk_catalog",
            desc: "查本科选课开课目录（服务端搜索，秒回）——上课【时间】权威来源（time=星期节次(周次)）。教室请用 query_coursex detail 或 query_learn_courses 的 timeLocation。q=课名或课号；teacher=教师名（可选精确过滤）；semester 如 2026-2027-1 缺省=当前；page 翻页",
            params: p(json!({
                "semester": {"type": "string"},
                "q": {"type": "string"},
                "teacher": {"type": "string"},
                "page": {"type": "integer", "minimum": 1}
            }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_xk_selected",
            desc: "查我的已选课程（semester 缺省=当前）",
            params: p(json!({ "semester": {"type": "string"} }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_xk_reviews",
            desc: "查选课社区课程评价（独立可用，无需进入选课）：course=课名关键词，teacher 可选教师名过滤",
            params: p(json!({
                "course": {"type": "string"},
                "teacher": {"type": "string"}
            }), &["course"]),
            confirm: false,
        },
        ToolDef {
            name: "query_coursex",
            desc: "查 CourseX 课程共享计划【没选的课的时间地点首选这个】：q=课名或教师名（二选一即可）；rows[].timeLocation=星期节次+教室。semester 缺省=当前学期，跨学期传 semester（如 2026-2027-1）。自己选了的课查课表 query_schedule 即有",
            params: p(json!({
                "q": {"type": "string"},
                "semester": {"type": "string"},
                "detail": {"type": "boolean", "description": "对首个结果取详情（具体时间地点）"}
            }), &["q"]),
            confirm: false,
        },
        ToolDef {
            name: "query_invoices",
            desc: "查电子发票列表（页码可选，默认第 1 页）",
            params: p(json!({ "page": {"type": "integer", "minimum": 1} }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_bank_payments",
            desc: "查银行代发工资记录（按月汇总）",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_graduate_income",
            desc: "查研究生收入明细（begin/end=YYYY-MM-DD，缺省近 90 天；本科生无权限会如实说明）",
            params: p(json!({ "begin": {"type": "string"}, "end": {"type": "string"} }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_dorm_score",
            desc: "查宿舍卫生检查成绩",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_physical_exam",
            desc: "查体测成绩",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "query_assessments",
            desc: "查教学评估任务列表（是否已填等）",
            params: p(json!({}), &[]),
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
        "cancel_venue_reservation" => {
            let recs = arr_of(&host(ctx, "venue", "myRecords", json!([1]))?);
            let u = s(args, "resvUuid");
            let rec = recs.iter().find(|r| s(r, "resvUuid") == u).ok_or("resvUuid 不在最近记录中，请先 query_venue_records")?;
            Ok(format!("取消体育场馆预约：{} {}（{}）", s(rec, "scene"), s(rec, "time"), s(rec, "status")))
        }
        "book_kongjian" => Ok(format!(
            "预约宿舍公共空间：{}（联系人 {}，电话 {}）",
            s(args, "bookUrl"), s(args, "name"), s(args, "tel")
        )),
        "cancel_kongjian" => {
            let recs = arr_of(&host(ctx, "kongjian", "my", json!([]))?);
            let t = s(args, "target");
            let rec = recs.iter().find(|r| s(r, "cancelTarget") == t || s(r, "spaceName").contains(&t))
                .ok_or("未找到匹配的预约记录，请先 query_kongjian_my")?;
            Ok(format!("取消公共空间预约：{}（{}）", s(rec, "spaceName"), s(rec, "time")))
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
        "query_learn_semesters" => json!({ "semesters": host(ctx, "learn", "semesters", json!([]))? }),
        "query_learn_courses" => {
            let sem = s(args, "semesterId");
            let out = if sem.is_empty() {
                host(ctx, "learn", "courses", json!([null]))?
            } else {
                host(ctx, "learn", "courses", json!([sem]))?
            };
            let rows: Vec<Value> = arr_of(&out.get("courses").cloned().unwrap_or_else(|| json!([])))
                .iter()
                .map(|c| json!({ "id": s(c, "id"), "name": s(c, "name"), "teacher": s(c, "teacher"), "timeLocation": c.get("timeAndLocation").cloned().unwrap_or_else(|| json!([])) }))
                .collect();
            json!({ "semester": out.get("semester").cloned().unwrap_or(json!("")), "count": rows.len(), "rows": rows, "note": "timeLocation=上课时间地点（教室以这里和 CourseX 为权威来源）；semester=网络学堂自报当前学期（新学期课程可能未发布在此学期），查不到时用 query_learn_semesters 换 semesterId 再试" })
        }
        "query_learn_homework" => {
            let sem = s(args, "semesterId");
            let out = if sem.is_empty() { host(ctx, "learn", "homework", json!([null]))? } else { host(ctx, "learn", "homework", json!([sem]))? };
            let rows: Vec<Value> = arr_of(&out)
                .iter()
                .take(30)
                .map(|h| json!({ "course": s(h, "courseName"), "title": s(h, "title"), "due": s(h, "endTime"), "status": s(h, "status"), "submitted": h.get("submitted").cloned().unwrap_or(json!(false)) }))
                .collect();
            json!({ "count": rows.len(), "rows": rows, "truncated": rows.len() >= 30 })
        }
        "query_learn_notifications" => {
            let sem = s(args, "semesterId");
            let out = if sem.is_empty() { host(ctx, "learn", "notifications", json!([null]))? } else { host(ctx, "learn", "notifications", json!([sem]))? };
            let rows: Vec<Value> = arr_of(&out)
                .iter()
                .take(20)
                .map(|n| json!({ "course": s(n, "courseName"), "title": s(n, "title"), "time": s(n, "time"), "hasDetail": s(n, "content") != "" }))
                .collect();
            json!({ "count": rows.len(), "rows": rows, "truncated": rows.len() >= 20 })
        }
        "query_learn_files" => {
            let kw = s(args, "course_keyword");
            let courses = arr_of(&host(ctx, "learn", "courses", json!([null]))?.get("courses").cloned().unwrap_or_else(|| json!([])));
            let hit = courses.iter().find(|c| s(c, "name").contains(&kw))
                .ok_or_else(|| format!("找不到课程「{kw}」；可用：{}", courses.iter().map(|c| s(c, "name")).collect::<Vec<_>>().join("、")))?;
            let files = host(ctx, "learn", "files", json!([s(hit, "id")]))?;
            let rows: Vec<Value> = arr_of(&files)
                .iter()
                .take(25)
                .map(|f| json!({ "title": s(f, "title"), "size": s(f, "size"), "time": s(f, "time") }))
                .collect();
            json!({ "course": s(hit, "name"), "count": rows.len(), "rows": rows })
        }
        "query_learn_bbs" => {
            let kw = s(args, "course_keyword");
            let courses = arr_of(&host(ctx, "learn", "courses", json!([null]))?.get("courses").cloned().unwrap_or_else(|| json!([])));
            let hit = courses.iter().find(|c| s(c, "name").contains(&kw))
                .ok_or_else(|| format!("找不到课程「{kw}」"))?;
            let wlkcid = s(hit, "id");
            let bq = s(args, "bqId");
            let tid = s(args, "threadId");
            if tid.is_empty() && bq.is_empty() {
                let boards = host(ctx, "learn", "bbsBoards", json!([wlkcid]))?;
                json!({ "course": s(hit, "name"), "boards": boards, "note": "带 bqId 查帖子列表；再带 threadId 看帖子与回帖" })
            } else if tid.is_empty() {
                let t = host(ctx, "learn", "bbsThreads", json!([wlkcid, { "bqid": bq }]))?;
                json!({ "course": s(hit, "name"), "total": t.get("total").cloned().unwrap_or(json!(0)), "threads": t.get("threads").cloned().unwrap_or_else(|| json!([])), "note": "带 threadId 看帖子内容与回帖" })
            } else {
                let detail = host(ctx, "learn", "bbsThread", json!([wlkcid, tid, if bq.is_empty() { json!(null) } else { json!(bq) }]))?;
                let posts = host(ctx, "learn", "bbsPosts", json!([wlkcid, tid, 1]))?;
                json!({ "course": s(hit, "name"), "thread": detail, "postsPage1": posts })
            }
        }
        "query_venue_scenes" => host(ctx, "venue", "scenes", json!([]))?,
        "query_venue_slots" => {
            let out = host(ctx, "venue", "currentPage", json!([{
                "sceneUuid": s(args, "sceneUuid"),
                "reserveDate": s(args, "date"),
                "classTypeUuid": s(args, "classTypeUuid"),
            }]))?;
            let rows: Vec<Value> = arr_of(&out)
                .iter()
                .take(30)
                .map(|it| json!({ "siteUuid": s(it, "siteUuid"), "name": s(it, "siteName"), "status": s(it, "statusName"), "price": it.get("price").cloned().unwrap_or(json!(null)) }))
                .collect();
            json!({ "date": s(args, "date"), "count": rows.len(), "rows": rows, "note": "预约在官方页面完成（jump_venue_booking 跳转）；站内可取消已约记录" })
        }
        "query_venue_records" => {
            let rows: Vec<Value> = arr_of(&host(ctx, "venue", "myRecords", json!([1]))?)
                .iter()
                .take(10)
                .map(|r| json!({ "resvUuid": s(r, "resvUuid"), "scene": s(r, "sceneName"), "time": s(r, "beginTime"), "status": s(r, "statusName") }))
                .collect();
            json!({ "rows": rows })
        }
        "jump_venue_booking" => {
            let url = host(ctx, "venue", "jump", json!([s(args, "sceneUuid")]))?;
            json!({ "opened": true, "url": url, "note": "已打开官方预约页（系统浏览器）" })
        }
        "query_kongjian" => {
            let out = host(ctx, "kongjian", "page", json!([{
                "date": s(args, "date"), "spaceId": s(args, "spaceId"), "roomId": s(args, "roomId"),
            }]))?;
            json!(out)
        }
        "query_kongjian_my" => host(ctx, "kongjian", "my", json!([]))?,
        "cancel_venue_reservation" => {
            host(ctx, "venue", "cancel", json!([s(args, "resvUuid")]))?;
            json!({ "cancelled": true, "resvUuid": s(args, "resvUuid") })
        }
        "book_kongjian" => {
            let out = host(ctx, "kongjian", "book", json!([s(args, "bookUrl"), {
                "name": s(args, "name"), "sid": s(args, "sid"), "tel": s(args, "tel"), "other": s(args, "other"),
            }]))?;
            json!({ "booked": true, "result": out })
        }
        "cancel_kongjian" => {
            host(ctx, "kongjian", "cancel", json!([s(args, "target")]))?;
            json!({ "cancelled": true, "target": s(args, "target") })
        }
        "query_xk_catalog" => {
            // 服务端搜索（每页 20 行秒回）。教师名优先单独作过滤（更准；且绕开
            // 课名 GBK 参数的未知嫌疑——该路径应用里没在线用过），返回后本地按 q 筛课名。
            let q = s(args, "q");
            let teacher = s(args, "teacher");
            let is_code = !q.is_empty() && q.chars().all(|ch| ch.is_ascii_alphanumeric());
            let page = args.get("page").and_then(|p| p.as_i64()).unwrap_or(1);
            let mut filter: Value = json!({
                "kcm": if is_code || !teacher.is_empty() { json!(null) } else { json!(q) },
                "kch": if is_code { json!(q) } else { json!(null) },
                "teacher": if teacher.is_empty() { json!(null) } else { json!(teacher) },
                "semester": s(args, "semester"),
                "page": page,
            });
            if is_code {
                filter["kcm"] = json!(null);
            }
            let mut out = host(ctx, "xk", "search", json!([filter]))?;
            let raw = out.get("rows").cloned().unwrap_or_else(|| json!([]));
            let mut rows: Vec<Value> = arr_of(&raw)
                .iter()
                .filter(|c| {
                    q.is_empty()
                        || is_code
                        || !teacher.is_empty() && s(c, "name").contains(&q)
                        || s(c, "name").contains(&q)
                        || s(c, "code").contains(&q)
                })
                .map(|c| {
                    let time = s(c, "time");
                    let mut room = s(c, "room");
                    if room.is_empty() {
                        if let Some(i) = time.rfind(')') {
                            room = time[i + 1..].trim().to_string();
                        }
                    }
                    json!({ "code": s(c, "code"), "seq": s(c, "seq"), "name": s(c, "name"), "teacher": s(c, "teacher"), "credits": c.get("credits").cloned().unwrap_or(json!(0)), "time": time, "room": room, "remaining": c.get("remaining").cloned().unwrap_or(json!(null)), "capacity": c.get("capacity").cloned().unwrap_or(json!(null)), "teacherId": s(c, "teacherId") })
                })
                .collect();
            // 教师过滤 + 本地课名筛选后若为空，但服务端确有行 → 该教师本学期没这门课（如实说）
            let server_rows = arr_of(&raw).len();
            let page_kind = s(&out, "pageKind");
            out = json!({
                "count": rows.len(),
                "serverRowCount": server_rows,
                "page": out.get("page").cloned().unwrap_or(json!(1)),
                "hasMore": out.get("hasMore").cloned().unwrap_or(json!(false)),
                "pageKind": page_kind,
                "diag": if page_kind == "unknown" {
                    let h: String = out.get("htmlHead").and_then(|v| v.as_str()).unwrap_or("").chars().take(120).collect();
                    h
                } else { String::new() },
                "rows": rows,
                "note": "time=星期节次(周次)——权威时间来源；room 仅个别行有；教室去 query_coursex detail=true 或 query_learn_courses 的 timeLocation 查；serverRowCount>0 而 count=0=该过滤组合无匹配（如该教师本学期无此课），如实告知勿重试同参数",
            });
            rows.clear();
            out
        }
        "query_xk_selected" => {
            let sem = s(args, "semester");
            let out = if sem.is_empty() { host(ctx, "xk", "selected", json!([null]))? } else { host(ctx, "xk", "selected", json!([sem]))? };
            let rows: Vec<Value> = arr_of(&out)
                .iter()
                .map(|c| json!({ "code": s(c, "code"), "name": s(c, "name"), "teacher": s(c, "teacher"), "time": s(c, "time"), "credits": c.get("credits").cloned().unwrap_or(json!(0)) }))
                .collect();
            json!({ "count": rows.len(), "rows": rows })
        }
        "query_xk_reviews" => {
            let out = host(ctx, "xk", "reviews", json!([s(args, "course"), s(args, "teacher")]))?;
            if out.is_null() {
                json!({ "found": false, "note": "选课社区没有匹配该课名/教师的评价" })
            } else {
                out
            }
        }
        "query_coursex" => {
            let q = s(args, "q");
            let sem = s(args, "semester");
            let list = host(ctx, "coursex", "search", json!([q, sem]))?;
            let rows: Vec<Value> = arr_of(&list)
                .iter()
                .take(12)
                .map(|it| json!({ "id": s(it, "id"), "name": s(it, "name"), "teacher": s(it, "teacherName"), "timeLocation": s(it, "timeLocation"), "semesterId": s(it, "semesterId") }))
                .collect();
            let details = if s(args, "detail") == "true" && !rows.is_empty() {
                let ids: Vec<String> = rows
                    .iter()
                    .take(3)
                    .filter_map(|r| r.get("id").and_then(|v| v.as_str()).map(|x| x.to_string()))
                    .collect();
                ids.into_iter()
                    .map(|id| match host(ctx, "coursex", "detail", json!([id])) {
                        Ok(v) => v,
                        Err(e) => json!({ "id": id, "error": e }),
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            json!({
                "rows": rows,
                "details": details,
                "note": "rows[].timeLocation=上课时间地点（教室就在搜索结果行里，人类流程即如此：q=课名或教师名，最多换学期）。details 为空不影响 rows 已含答案；跨学期传 semester",
            })
        }
        "query_invoices" => {
            let page = args.get("page").and_then(|p| p.as_i64()).unwrap_or(1);
            let out = host(ctx, "info", "invoices", json!([page]))?;
            let rows: Vec<Value> = arr_of(&out.get("data").cloned().unwrap_or_else(|| json!([])))
                .iter()
                .take(15)
                .map(|it| {
                    json!({ "title": s(it, "title"), "amount": s(it, "amount"), "date": s(it, "date"), "buyer": s(it, "buyer"), "uuid": s(it, "uuid") })
                })
                .collect();
            json!({ "count": out.get("count").cloned().unwrap_or_else(|| json!(0)), "rows": rows, "note": "uuid 可用于后续取 PDF（如开放）" })
        }
        "query_bank_payments" => {
            let rows: Vec<Value> = arr_of(&host(ctx, "info", "bankPayments", json!([]))?)
                .into_iter()
                .take(12)
                .map(|it| json!({ "month": s(&it, "month"), "payment": it.get("payment").cloned().unwrap_or_else(|| json!([])) }))
                .collect();
            json!({ "rows": rows, "note": "payment 为该月各项金额数组（列序=上游原样）" })
        }
        "query_graduate_income" => {
            let b = s(args, "begin");
            let e = s(args, "end");
            let out = host(ctx, "info", "graduateIncome", json!([b, e]))?;
            if out.is_null() {
                json!({ "noPermission": true, "note": "无权限或无数据（本科生专项目不开放，属正常）" })
            } else {
                let rows: Vec<Value> = arr_of(&out)
                    .iter()
                    .take(20)
                    .map(|it| json!({ "time": s(it, "time"), "name": s(it, "name"), "amount": s(it, "amount"), "card": s(it, "card") }))
                    .collect();
                json!({ "count": rows.len(), "rows": rows, "truncated": rows.len() >= 20 })
            }
        }
        "query_dorm_score" => host(ctx, "info", "dormScore", json!([]))?,
        "query_physical_exam" => host(ctx, "info", "physicalExam", json!([]))?,
        "query_assessments" => host(ctx, "info", "assessmentList", json!([]))?,
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
                .map(|it| json!({ "xxid": s(&it, "xxid"), "name": s(&it, "name"), "date": s(&it, "date"), "source": s(&it, "source"), "link": format!("onethu-news://{}", s(&it, "xxid")) }))
                .collect();
            json!({ "rows": rows, "note": "每条新闻用 markdown 链接把 link 给用户（[标题](link)，应用内直达新闻页）；不要给外部原文 URL" })
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
                .map(|it| json!({ "xxid": s(&it, "xxid"), "name": s(&it, "name"), "date": s(&it, "date"), "source": s(&it, "source"), "link": format!("onethu-news://{}", s(&it, "xxid")) }))
                .collect();
            json!({ "rows": rows, "note": "每条新闻用 markdown 链接把 link 给用户（[标题](link)，应用内直达新闻页）；不要给外部原文 URL" })
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
