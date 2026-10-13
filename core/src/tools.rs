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

/// 检索行路由表（**编译期资产**：plugins/OneTHU-Harness/assets/routes.json）。
/// 由 tools/gen-oh-routes.mjs 从 apps/desktop/src/state/searchRows.ts（唯一真源）离线生成；
/// 改动检索行后**必须重跑**该脚本，护栏 tools/oh-routes-sync-test.mjs 不一致会 exit 1（防脱同）。
const ROUTES_JSON: &str = include_str!("../../assets/routes.json");

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
            name: "thubook_toc",
            desc: "列 THUbook（清华手册，thubook.help）的全部页面路径。先看目录再决定读哪页",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
            name: "thubook_read",
            desc: "读 THUbook 某页正文（纯文本）。path 来自 thubook_toc，如 /thubook/专题/chuguo.html；缺省为手册首页",
            params: p(json!({
                "path": {"type": "string", "description": "页面路径，以 / 开头；缺省 /thubook/"}
            }), &[]),
            confirm: false,
        },
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
            name: "query_agenda",
            desc: "查我的日程（云同步日历 + 本地手动日程：班会/活动/自习等个人日程都在这，含 OneTHU 或系统日历里添加的）。start/end 接受 今天/明天/下周三/2025-09-08；缺省今天起 14 天。上课时间地点另查 query_schedule",
            params: p(json!({
                "start": {"type": "string", "description": "起始日期，如 明天 / 2025-09-08"},
                "end": {"type": "string", "description": "结束日期（含）"}
            }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "add_schedule",
            desc: "新建日程并同步到云日历（系统日历/其他设备可见；未配置云同步则存本地）。例如：明天 14:00-15:30 班会",
            params: p(json!({
                "title": {"type": "string", "description": "日程标题"},
                "date": {"type": "string", "description": "日期：明天 / 2025-09-08"},
                "start": {"type": "string", "description": "开始 HH:MM（全天日程可省）"},
                "end": {"type": "string", "description": "结束 HH:MM（全天日程可省）"},
                "location": {"type": "string", "description": "地点（可选）"},
                "note": {"type": "string", "description": "备注（可选）"},
                "allDay": {"type": "boolean", "description": "全天日程（可选）"}
            }), &["title", "date"]),
            confirm: true,
        },
        ToolDef {
            name: "remove_schedule",
            desc: "删除日程（uid 来自 query_agenda 返回；云端/本地自动路由）",
            params: p(json!({
                "uid": {"type": "string", "description": "query_agenda 返回的 uid"}
            }), &["uid"]),
            confirm: true,
        },
        ToolDef {
            name: "edit_schedule",
            desc: "修改已有日程（uid 来自 query_agenda；只传要改的字段，未传的保留原值）。例如：把明天 14:00 的班会改到 16:00-17:30",
            params: p(json!({
                "uid": {"type": "string", "description": "query_agenda 返回的 uid"},
                "title": {"type": "string", "description": "新标题（不改不传）"},
                "date": {"type": "string", "description": "新日期：明天 / 2025-09-08（不改不传）"},
                "start": {"type": "string", "description": "新开始 HH:MM（不改不传；改全天则不传）"},
                "end": {"type": "string", "description": "新结束 HH:MM（不改不传）"},
                "location": {"type": "string", "description": "新地点（不改不传；传空串=清除地点）"},
                "note": {"type": "string", "description": "新备注（不改不传；传空串=清除备注）"},
                "allDay": {"type": "boolean", "description": "改为全天日程（可选）"},
                "toCloud": {"type": "boolean", "description": "迁移存储（可选）：true=转入云日历（多设备可见），false=转为仅本机"}
            }), &["uid"]),
            confirm: true,
        },
        ToolDef {
            name: "query_mails",
            desc: "查我的清华邮箱最新邮件（收件箱或已发送）。返回 uid/主题/发件人/时间/未读；要看正文用 read_mail。用户问「我的邮件」「最新邮件」「有没有新邮件」时用这个",
            params: p(json!({
                "folder": {"type": "string", "description": "收件箱(inbox，默认) / 已发送(sent)"},
                "limit": {"type": "integer", "description": "最多返回几封（默认 5，最大 20）"}
            }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "read_mail",
            desc: "读一封邮件的正文（uid 来自 query_mails / search_mails；读后自动标为已读）",
            params: p(json!({
                "uid": {"type": "integer", "description": "邮件 uid"},
                "folder": {"type": "string", "description": "默认收件箱"}
            }), &["uid"]),
            confirm: false,
        },
        ToolDef {
            name: "search_mails",
            desc: "全箱搜索邮件（服务器端按主题/发件人匹配，覆盖全部历史邮件，不只是最新一页）",
            params: p(json!({
                "query": {"type": "string", "description": "关键词"},
                "folder": {"type": "string", "description": "默认收件箱"}
            }), &["query"]),
            confirm: false,
        },
        ToolDef {
            name: "send_mail",
            desc: "从我的清华邮箱发邮件（多个收件人用逗号分隔）",
            params: p(json!({
                "to": {"type": "string", "description": "收件人邮箱"},
                "subject": {"type": "string", "description": "主题"},
                "body": {"type": "string", "description": "正文（纯文本）"},
                "cc": {"type": "string", "description": "抄送（可选）"}
            }), &["to", "subject", "body"]),
            confirm: true,
        },
        ToolDef {
            name: "query_cloud_files",
            desc: "查我的清华云盘（Seafile）。不带参数=列出资料库；带 repo（库名）=列该库根目录；带 repo+path=列子目录。文件操作（上传/分享）的 repo/path 参数从这里拿",
            params: p(json!({
                "repo": {"type": "string", "description": "资料库名（不带参数时返回的 name）"},
                "path": {"type": "string", "description": "库内目录（如 /课件，缺省 /）"}
            }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "search_cloud_files",
            desc: "清华云盘库内搜索文件名（服务器端，覆盖整库历史文件）",
            params: p(json!({
                "repo": {"type": "string", "description": "资料库名"},
                "query": {"type": "string", "description": "文件名关键词"}
            }), &["repo", "query"]),
            confirm: false,
        },
        ToolDef {
            name: "cloud_share_file",
            desc: "生成云盘文件的分享链接（复制给他人可下载；默认永久有效）",
            params: p(json!({
                "repo": {"type": "string", "description": "资料库名"},
                "path": {"type": "string", "description": "文件完整路径（如 /课件/第一周.pdf）"},
                "days": {"type": "integer", "description": "有效天数（0=永久，缺省 0）"}
            }), &["repo", "path"]),
            confirm: true,
        },
        ToolDef {
            name: "cloud_upload_file",
            desc: "上传本地文件到清华云盘（本地路径支持 ~ 前缀，如 ~/Downloads/报告.pdf）",
            params: p(json!({
                "localPath": {"type": "string", "description": "本地文件路径（支持 ~）"},
                "repo": {"type": "string", "description": "目标资料库名"},
                "path": {"type": "string", "description": "目标目录（缺省 /，不存在会报错；先 query_cloud_files 确认）"},
                "replace": {"type": "boolean", "description": "同名覆盖（缺省 false）"}
            }), &["localPath", "repo"]),
            confirm: true,
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
            name: "reply_learn_thread",
            desc: "在网络学堂讨论区某帖子下回帖（写操作，执行前会请用户确认；仅在用户明确要求回帖时使用）",
            params: p(json!({
                "wlkcid": {"type": "string"},
                "threadId": {"type": "string"},
                "content": {"type": "string"}
            }), &["wlkcid", "threadId", "content"]),
            confirm: true,
        },
        ToolDef {
            name: "post_learn_thread",
            desc: "在网络学堂某课程某版面发新帖（写操作，执行前会请用户确认；仅在用户明确要求发帖时使用）",
            params: p(json!({
                "wlkcid": {"type": "string"},
                "bqid": {"type": "string"},
                "title": {"type": "string"},
                "content": {"type": "string"}
            }), &["wlkcid", "bqid", "title", "content"]),
            confirm: true,
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
            desc: "查网络学堂某课程文件列表（course_keyword=课程名关键词；往期学期课程必须带 semesterId，如春季课 = 2025-2026-2）",
            params: p(json!({
                "course_keyword": {"type": "string"},
                "semesterId": {"type": "string", "description": "课程所属学期（往期课程必传，如 2025-2026-2）"}
            }), &["course_keyword"]),
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
            desc: "查 CourseX 课程共享计划【没选的课的时间地点首选，一次调用直接给教室】：q=单个关键词（课名 或 教师名，不拼接）。自动取详情合并 rows[].timeLocation（星期节次+教室）。semester 缺省=当前；跨学期传 semester。自己选了的课直接查 query_schedule",
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
            desc: "在 OneTHU 应用内跳转页面（today/learn/schedule/info/life/reserve/settings 等，可带参数）。网络学堂一键直达：learn-course {courseId, semesterId, courseTab?: files|notifications|bbs}、learn-forum-thread {courseId, itemId, semesterId, bqid?}（具体帖子）、learn-assignment-detail {courseId, itemId, semesterId}（具体作业）、learn-notice-detail {courseId, itemId, semesterId}（具体通知）、learn-files {courseId, semesterId}（文件列表）、learn-assignments/learn-notices。semesterId=课程所属学期（query_learn_courses 返回的 semester；**必传**——courseId 是学期作用域的，不传会落空白页）。资金类只读红线不拦导航：校园卡充值界面 = navigate life + {\"lifeTab\":\"card\"}（充值由用户在官方界面完成，助手不代充）",
            params: p(json!({
                "page": {"type": "string"},
                "params": {"type": "object", "description": "如 {\"reserveTab\":\"lib\"}"}
            }), &["page"]),
            confirm: false,
        },
        ToolDef {
            name: "open_page",
            desc: "按名字在应用内直接打开一个页面或事物——用户说「打开亲友来访」「帮我开研讨间预约」「打开数据结构这门课」时用它，比 navigate 更省事：只需给名字，本工具自己检索并跳转（在线服务、课程、作业、通知、文件、场馆、教学楼、洗衣机楼、图书馆、新闻、今日组件、功能页面等）。先查本机缓存（快、离线）；本机没有时**自动兜底查一次在线服务目录**（容忍口语简称：「亲友预约」能命中「亲友来访预约」），所以在线服务不需要用户事先打开过。命中多个时返回 candidates，向用户确认后再带 kind 调用一次。目录里只有「有点像」的名字时（返回 similar/score 且 opened=false）**不会**替你打开，要先把候选念给用户确认，再用确认的正式名调一次。回话时用目录里的正式名（官网口径），不要复述用户的口语叫法当作官方名称；查询失败与「确实没有」要分开说，别把失败说成学校没这个服务",
            params: p(json!({
                "query": {"type": "string", "description": "页面或事物的名字，如「亲友来访」「缓考」「数据结构」「研讨间预约」"},
                "kind": {"type": "string", "description": "可选：限定原子种类（如 thos-service / course / sports-v / library），不确定就不要传"}
            }), &["query"]),
            confirm: false,
        },
        ToolDef {
            name: "query_usage",
            desc: "查本机使用统计（用户自己点过什么）：返回总次数与最常用的页面/事物（带 kind+key）。用途：用户问「我最近都在用什么」「帮我把常用的打开」时先看这里，再用 open_page（或 navigate）打开；也可据此判断某个功能他到底用不用得上。数据只在本机，不含任何校园内容；不要把它当成学习成绩之类的隐私数据来解读",
            params: p(json!({ "limit": {"type": "integer", "minimum": 1, "maximum": 30} }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "notify",
            desc: "在应用底部弹一条 toast 提示",
            params: p(json!({ "text": {"type": "string"} }), &["text"]),
            confirm: false,
        },
        // ── OH 记忆（云端记忆共享，docs/memory-cloud；引擎在宿主 TS 侧，经门禁 memory:read/write）──
        ToolDef {
            name: "memory_search",
            desc: "检索长期记忆（个人偏好/课程上下文/任务状态等）。用户提到「我之前说过」「记住过」或需要个性化上下文时先调用本工具。query 空格分词；tags/type/folder 可选过滤；返回 permalink 列表+摘要。",
            params: p(json!({
                "query": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "type": {"type": "string"},
                "folder": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 20}
            }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "memory_read",
            desc: "按 permalink 读一条记忆的全文（超 8KB 截断）。permalink 从 memory_search/memory_list 获取。",
            params: p(json!({ "permalink": {"type": "string"} }), &["permalink"]),
            confirm: false,
        },
        ToolDef {
            name: "memory_list",
            desc: "列记忆清单（不读正文）：folder 如 preferences/courses/tasks/knowledge/journal，tag 可选。",
            params: p(json!({ "folder": {"type": "string"}, "tag": {"type": "string" } }), &[]),
            confirm: false,
        },
        ToolDef {
            name: "memory_write",
            desc: "新建一条长期记忆（执行前会请用户确认）。title 必须把同义词折叠进去（如「线性代数」而非「那门数学课」）；observations 每条 = {category, text}（category 如 preference/context/habit/fact）；写前先 memory_search 查重，已有同主题实体就改 memory_append。",
            params: p(json!({
                "title": {"type": "string"},
                "observations": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "category": {"type": "string"},
                            "text": {"type": "string"},
                            "context": {"type": "string"}
                        },
                        "required": ["category", "text"]
                    }
                },
                "relations": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "rel": {"type": "string"}, "target": {"type": "string"}, "context": {"type": "string" } },
                        "required": ["rel", "target"]
                    }
                },
                "tags": {"type": "array", "items": {"type": "string"}},
                "type": {"type": "string"}
            }), &["title", "observations"]),
            confirm: true,
        },
        ToolDef {
            name: "memory_append",
            desc: "给已有记忆追加一条观察（执行前会请用户确认）。permalink 从 search/list 获取；category 如 context/habit/fact。",
            params: p(json!({
                "permalink": {"type": "string"},
                "category": {"type": "string"},
                "text": {"type": "string"},
                "context": {"type": "string"}
            }), &["permalink", "category", "text"]),
            confirm: true,
        },
        ToolDef {
            name: "memory_edit",
            desc: "对一条记忆做字面替换改写（执行前会请用户确认）。找不到 find 文本会报错；改前先 memory_read 核对原文。",
            params: p(json!({
                "permalink": {"type": "string"},
                "find": {"type": "string"},
                "replace": {"type": "string"}
            }), &["permalink", "find", "replace"]),
            confirm: true,
        },
        ToolDef {
            name: "memory_delete",
            desc: "归档一条记忆（软删进 .trash，执行前会请用户确认并显示标题）。",
            params: p(json!({
                "permalink": {"type": "string"},
                "reason": {"type": "string"}
            }), &["permalink"]),
            confirm: true,
        },
        ToolDef {
            name: "memory_refresh",
            desc: "重扫记忆镜像并重建账本（排障用：云盘拉取或手工改动后对齐）。",
            params: p(json!({}), &[]),
            confirm: false,
        },
        ToolDef {
        name: "list_plugin_cmds",
        desc: "列出当前已启用插件的命令清单（联动插件）。需要执行插件能力时，先调用本工具查看可用命令，再用 run_plugin_cmd 执行。",
        params: json!({ "type": "object", "properties": {} }),
        confirm: false,
    },
    ToolDef {
        name: "run_plugin_cmd",
        desc: "执行某个已启用插件的命令（联动插件）。pluginId/cmdId 从 list_plugin_cmds 结果获取；input 为该命令的文本参数（命令标注「无输入」时传空串）。注意：命令可能包含写操作，执行前应向用户说明将要做什么。",
        params: json!({
            "type": "object",
            "properties": {
                "pluginId": { "type": "string", "description": "插件 id，如 onethu.dept-notices" },
                "cmdId": { "type": "string", "description": "命令 id" },
                "input": { "type": "string", "description": "命令文本参数；命令无输入时传空串" }
            },
            "required": ["pluginId", "cmdId"]
        }),
        confirm: false,
    },
    ToolDef {
        name: "list_favorite_kinds",
        desc: "列出可收藏的原子种类（收藏夹体系）。需要收藏信息时先调用本工具查看可用的 kind。",
        params: json!({ "type": "object", "properties": {} }),
        confirm: false,
    },
    ToolDef {
        name: "ui_navigate",
        desc: "检索行级直达（与检索条的搜索结果同一条链）：给出 routeId（路由表 ui_route_table 的 id）或 query（自然语言关键词，如 多模态 / 管理主题 / 音色 / 桌面小组件）。唯一命中 → 真实跳转到那一行（正确的页面 / OH 档位 / 插件视图，滚动并高亮那一行）；结果返回 {ok, target, breadcrumb}。查无或多个可能 → 返回 {ok:false} 与候选，向用户复述并等待选择，绝不硬猜。",
        params: p(json!({
            "routeId": {"type": "string", "description": "路由表行的稳定 id（来自 ui_route_table 或 history）；给 routeId 时精确直达"},
            "query": {"type": "string", "description": "关键词；与检索条同源匹配（标题/别名），例如「多模态」「管理主题」「音色」"}
        }), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ui_route_table",
        desc: "列出可直达的检索行（searchRows 注册表投影：id/标题/面包屑/档位视图）。用户想「打开/设置 XX」但不清楚具体行 id 时，先调本工具拿 id，再 ui_navigate 精确直达。",
        params: p(json!({}), &[]),
        confirm: false,
    },
    /* —— 用户扩展层元工具（2026-10-13 OH 自我进化第一批）：用户工具 / OneFlow / 生成组件 ——
       全部转发 TS 侧 ext 命名空间（持久化与执行器在 TS：state/userext.ts）。 —— */
    ToolDef {
        name: "ext_flow_create",
        desc: "创建 OneFlow（工作区+自动化+技能的结合体）：给流程起名、选触发方式（手动 / 定时每 N 分钟 / 事件[作业截止·成绩更新·应用启动]）、再给一串步骤（每步 {tool: \"ns.method\", args: {…}, from: {参数名: \"$N.路径\"}}，$N 引用第 N 步结果）。创建后立即生效：OneGO 的 OneFlow 区可见可跑，定时/事件自动触发。例：每天 8 点查作业并发提醒。",
        params: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "流程名（如「每日作业播报」）" },
                "trigger": {
                    "type": "object",
                    "description": "触发器：{kind:\"manual\"} / {kind:\"cron\",everyMinutes:30} / {kind:\"event\",event:\"hw-deadline\"|\"grades-update\"|\"app-start\"}",
                    "properties": {
                        "kind": { "type": "string", "enum": ["manual", "cron", "event"] },
                        "everyMinutes": { "type": "number" },
                        "event": { "type": "string", "enum": ["hw-deadline", "grades-update", "notice", "app-start"] }
                    },
                    "required": ["kind"]
                },
                "steps": {
                    "type": "array",
                    "description": "步骤序列：[{tool:\"ui_navigate\",args:{query:\"作业\"}}, …]；from 可把上一步结果注入参数",
                    "items": {
                        "type": "object",
                        "properties": {
                            "tool": { "type": "string" },
                            "args": { "type": "object" },
                            "from": { "type": "object" }
                        },
                        "required": ["tool"]
                    }
                }
            },
            "required": ["name", "trigger", "steps"]
        }),
        confirm: false,
    },
    ToolDef {
        name: "ext_flow_list",
        desc: "列出全部 OneFlow（名字 / 触发器 / 上次运行与结果）。",
        params: p(json!({}), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ext_flow_run",
        desc: "立即运行一个 OneFlow（给 id，来自 ext_flow_list）。",
        params: p(json!({ "id": { "type": "string", "description": "id（来自对应 list 工具）" } }), &["id"]),
        confirm: false,
    },
    ToolDef {
        name: "ext_flow_delete",
        desc: "删除一个 OneFlow（需先向用户确认）。",
        params: p(json!({ "id": { "type": "string", "description": "id（来自对应 list 工具）" } }), &["id"]),
        confirm: true,
    },
    ToolDef {
        name: "ext_tool_create",
        desc: "把你刚才用得顺手的**一串工具调用**固化成一个命名工具（用户自定义技能）：之后 ext_tool_run 一步执行。steps 语法与 ext_flow_create 相同（无触发器）。",
        params: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "工具名（如「查今天作业并高亮」）" },
                "description": { "type": "string", "description": "一句话说明它做什么（会出现在工具列表里）" },
                "steps": {
                    "type": "array",
                    "description": "步骤序列（同 OneFlow steps）",
                    "items": { "type": "object", "properties": { "tool": { "type": "string" }, "args": { "type": "object" }, "from": { "type": "object" } }, "required": ["tool"] }
                }
            },
            "required": ["name", "description", "steps"]
        }),
        confirm: false,
    },
    ToolDef {
        name: "ext_tool_list",
        desc: "列出全部用户自定义工具（名字 / 说明）。",
        params: p(json!({}), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ext_tool_run",
        desc: "运行一个用户自定义工具（给 id，来自 ext_tool_list）。",
        params: p(json!({ "id": { "type": "string", "description": "id（来自对应 list 工具）" } }), &["id"]),
        confirm: false,
    },
    ToolDef {
        name: "ext_tool_delete",
        desc: "删除一个用户自定义工具（需先向用户确认）。",
        params: p(json!({ "id": { "type": "string", "description": "id（来自对应 list 工具）" } }), &["id"]),
        confirm: true,
    },
    ToolDef {
        name: "ext_theme_apply",
        desc: "创建并应用一个**用户生成主题**（液态玻璃时代的主题就是一组 CSS 变量）：给名字与变量表（--md-sys-color-primary / --md-sys-color-surface-container… 等 System 名，值任意 CSS 颜色）。立即生效且持久。用户说「换个主题，主色用 X」→ 你算好一套和谐的变量再调本工具。恢复默认 = ext_theme_activate(null)。",
        params: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "主题名（如「夜航蓝」）" },
                "vars": {
                    "type": "object",
                    "description": "CSS 变量表：{\"--md-sys-color-primary\": \"#5b9bff\", …}（System 名；至少给 primary/surface 系才有完整观感）"
                }
            },
            "required": ["name", "vars"]
        }),
        confirm: false,
    },
    ToolDef {
        name: "ext_theme_list",
        desc: "列出全部用户生成主题（名字/是否生效中）。",
        params: p(json!({}), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ext_theme_activate",
        desc: "切换/停用用户主题：id 来自 ext_theme_list，传 null 恢复跟随系统主题。",
        params: p(json!({ "id": { "type": ["string", "null"], "description": "主题 id；null=恢复默认" } }), &["id"]),
        confirm: false,
    },
    ToolDef {
        name: "ext_theme_delete",
        desc: "删除一个用户生成主题（需先向用户确认）。",
        params: p(json!({ "id": { "type": "string", "description": "id（来自对应 list 工具）" } }), &["id"]),
        confirm: true,
    },
    ToolDef {
        name: "ext_widget_create",
        desc: "在首页白板上生成一块**用户自定义组件**（你写的 HTML 片段，自动净化脚本后渲染在玻璃卡面里）。适合做：个人签名卡、自定义快捷说明、小抄、倒计时文字等纯展示内容。",
        params: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "组件名" },
                "html": { "type": "string", "description": "HTML 片段（可用 h1-h3/p/b/i/ul/li/span/div/a；script 与事件属性会被剥除）" },
                "x": { "type": "number", "description": "落点（可选，默认左上区域）" },
                "y": { "type": "number", "description": "落点（可选）" }
            },
            "required": ["name", "html"]
        }),
        confirm: false,
    },
    ToolDef {
        name: "ext_widget_list",
        desc: "列出全部用户生成组件（id/名字/创建时间）。管理或引用组件 id 前先查这里。",
        params: p(json!({}), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ext_widget_delete",
        desc: "删除一个用户生成组件（需先向用户确认）：定义与其在首页白板/各 OnePage 上的实例一并清掉。",
        params: p(json!({ "id": { "type": "string", "description": "组件 id（来自 ext_widget_list）" } }), &["id"]),
        confirm: true,
    },
    ToolDef {
        name: "ext_onepage_create",
        desc: "新建一块 OnePage（用户白板页）。用户说「帮我建个白板/页面」时用。",
        params: p(json!({
            "name": { "type": "string", "description": "OnePage 名字" },
            "icon": { "type": "string", "description": "图标名（可选，默认 star）" }
        }), &["name"]),
        confirm: false,
    },
    ToolDef {
        name: "ext_onepage_list",
        desc: "列出全部 OnePage（id/名字/类型）。往某块 OnePage 放组件前先查 id。",
        params: p(json!({}), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ext_onepage_add",
        desc: "往一块 OnePage（用户白板）里放一个组件。kind 三选一：text（一段文字）、atom-link（引用一个已有原子，ref 格式 kind|key，可从收藏夹/原子目录取）、custom（用户生成组件 id，如 usergen:uw.xxx）。格子坐标 x/y 以手机 4 列为准，w/h 单位格。",
        params: p(json!({
            "boardId": { "type": "string", "description": "OnePage id（来自 ext_onepage_list）" },
            "kind": { "type": "string", "description": "text | atom-link | custom" },
            "payload": {
                "type": "object",
                "description": "随 kind 变：text 给 {text, variant?}；atom-link 给 {ref}；custom 给 {id}"
            },
            "x": { "type": "number" },
            "y": { "type": "number" },
            "w": { "type": "number", "description": "格宽（默认 2）" },
            "h": { "type": "number", "description": "格高（默认 1）" }
        }), &["boardId", "kind"]),
        confirm: false,
    },
    ToolDef {
        name: "ext_phone_push",
        desc: "把几行文字推到**手机桌面小组件**（Android 首页卡片）：每次推送整体替换该槽位内容（≤8 行，每行 ≤40 字，副行 ≤60 字）。用户说「把今天待办放上手机桌面 / 手机小组件显示 XX」时用。rows 传空数组即清除、还原插件默认内容。App 在前台时会随下一次快照推送到原生。",
        params: p(json!({
            "title": { "type": "string", "description": "卡片标题（≤20 字）" },
            "rows": {
                "type": "array",
                "description": "行数组，每行 {text: 主行, sub?: 副行}",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "主行文字" },
                        "sub": { "type": "string", "description": "副行补充（可选）" }
                    },
                    "required": ["text"]
                }
            }
        }), &["rows"]),
        confirm: false,
    },
    ToolDef {
        name: "onePage_write",
        desc: "编辑已存在的 OnePage（用户白板）：op 四选一——\"rename\" 改名（needs name）；\"delete\" 删除（板与内容永久消失，高危）；\"open\" 打开为当前标签页；\"items\" 列出板上的组件 id 与格位（配合 onePage_place 调位 / 删除）。id 来自 ext_onepage_list。",
        params: p(json!({
            "op": { "type": "string", "description": "rename | delete | open | items" },
            "id": { "type": "string", "description": "OnePage id" },
            "name": { "type": "string", "description": "op=rename 时的新名字" }
        }), &["op", "id"]),
        confirm: false,
    },
    ToolDef {
        name: "onePage_place",
        desc: "对 OnePage 上的一个已存在组件做调整：op=move 把它挪到目标格（x/y 以手机 4 列计，不需要视觉判断，直接按用户说的数字来）；op=remove 把它摘下来。boardId/itemId 来自 onePage_write(op=items)。",
        params: p(json!({
            "op": { "type": "string", "description": "move | remove" },
            "boardId": { "type": "string" },
            "itemId": { "type": "string" },
            "x": { "type": "number", "description": "op=move 时的目标列（0 起）" },
            "y": { "type": "number", "description": "op=move 时的目标行（0 起）" }
        }), &["op", "boardId", "itemId"]),
        confirm: false,
    },
    ToolDef {
        name: "workspace_write",
        desc: "工作区管理：op=list 列全部工作区（id/名字/是否活板/成员数）；op=create 新建（给 name；可选 tabIds 成员标签数组，不给就把当前页签存档，再不行建一块空活底板）；op=bring 打开为当前工作组；op=collapse 收起活板回到 OneGO 抽屉；op=spread 解除工作区（成组标签复位为散标签，板的定义消失但标签还在，多数场景用这个）。id 来自 op=list。",
        params: p(json!({
            "op": { "type": "string", "description": "list | create | bring | collapse | spread" },
            "name": { "type": "string", "description": "op=create 时的名字" },
            "tabIds": { "type": "array", "description": "op=create 时的成员标签 id 列表（可选）", "items": { "type": "string" } },
            "id": { "type": "string", "description": "op=bring/collapse/spread 时的工作区 id" }
        }), &["op"]),
        confirm: false,
    },
    ToolDef {
        name: "ext_tab",
        desc: "标签页管理：op=list 列当前全部标签（id/标题/页）；op=open 开新一级标签页（page 可用 today/learn/schedule/info/life/reserve/mail/cloud/gitlab/thubook/tasks/services/favs/boards/board 等）；op=focus 跳到某个已有标签；op=duplicate 再开一个同页同参的标签。",
        params: p(json!({
            "op": { "type": "string", "description": "list | open | focus | duplicate" },
            "page": { "type": "string", "description": "op=open 时的页面名" },
            "id": { "type": "string", "description": "op=focus/duplicate 时的标签 id" }
        }), &["op"]),
        confirm: false,
    },
    ToolDef {
        name: "ext_tab_close",
        desc: "关闭一个标签页（首页关不掉）。id 来自 ext_tab(op=list)。",
        params: p(json!({ "id": { "type": "string" } }), &["id"]),
        confirm: true,
    },
    ToolDef {
        name: "ext_wallpaper",
        desc: "读取或设置壁纸：不带 ref 就是读当前（全屏档与当前标签档）；带 ref 则设置。scope=global 改全屏档（整个软件底色）；scope=tab 只改当前标签页。ref 可选内置：builtin:aurora（极光）/ builtin:dusk（暮色）/ builtin:matcha（抹茶）/ builtin:ink（墨）；传 null 或 \"\" 清除档位回落默认。本地图片/视频不动 OH——请用户在 设置→壁纸 的库里上传与点选。",
        params: p(json!({
            "scope": { "type": "string", "description": "global | tab" },
            "ref": { "type": ["string", "null"], "description": "壁纸引用；缺省则只读当前" }
        }), &["scope"]),
        confirm: false,
    },
    ToolDef {
        name: "ext_voice",
        desc: "OH 语音设置：调音色与朗读。字段全部可选——autoSpeak（true 自动朗读 OH 回复 / false 静音）、backend（\"system\" 系统音 / \"neural\" 神经音色）、voiceName（音色名，空串=引擎默认）、pitch（0.5~2 语调）、rate（0.5~2 语速）。返回调整后的完整当前值。",
        params: p(json!({
            "autoSpeak": { "type": "boolean" },
            "backend": { "type": "string", "description": "system | neural" },
            "voiceName": { "type": "string" },
            "pitch": { "type": "number" },
            "rate": { "type": "number" }
        }), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ext_prefs",
        desc: "外观与一般偏好小开关：capsuleTap（点底部胶囊默认开「search」搜索还是「oh」对话）、voiceTarget（长按语音识别默认发到哪）。leaf 字段可选；返回当前值。",
        params: p(json!({
            "capsuleTap": { "type": "string", "description": "search | oh" },
            "voiceTarget": { "type": "string", "description": "search | oh" }
        }), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ext_theme_schedule",
        desc: "昼夜主题自动跟随：followSystem=false 手动模式（ext_theme_apply 即可切）；followSystem=true 时用明暗两档主题——dayThemeId / nightThemeId（可空=跟随系统默认）。返回跟随状态。",
        params: p(json!({
            "followSystem": { "type": "boolean" },
            "dayThemeId": { "type": ["string", "null"] },
            "nightThemeId": { "type": ["string", "null"] }
        }), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ext_notify",
        desc: "通知提醒设置（改完立即生效）：enabled 总开关、ddl 作业截止提醒、classLead 上课前几分钟提醒、briefingAt 每日摘要时刻（HH:mm）、quietFrom/quietTo 免打扰时段（HH:mm）、horizonDays 提前几天内的日程 / maxItems 摘要条数上限。字段全可选，返回当前值。",
        params: p(json!({
            "enabled": { "type": "boolean" },
            "ddl": { "type": "boolean" },
            "classLead": { "type": "number" },
            "briefingAt": { "type": "string" },
            "quietFrom": { "type": "string" },
            "quietTo": { "type": "string" },
            "horizonDays": { "type": "number" },
            "maxItems": { "type": "number" }
        }), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ext_hw_timeline",
        desc: "DDL 时间线默认提前量：提醒时间默认提前 minutes 分钟（0~20160）。用户说「提前两晚上提醒我」→ 给分钟数。",
        params: p(json!({ "minutes": { "type": "number" } }), &["minutes"]),
        confirm: false,
    },
    ToolDef {
        name: "ext_hw_reminder",
        desc: "单个作业的提醒提前量（覆盖默认）：minutes 传数字设置，传 null 恢复跟随默认。id 是作业 id（网络学堂工具可查）。",
        params: p(json!({ "id": { "type": "string" }, "minutes": { "type": ["number", "null"] } }), &["id"]),
        confirm: false,
    },
    ToolDef {
        name: "ext_event_add",
        desc: "新增一条本地日程（会随用户云日历同步生效）：summary 标题、start/end 格式「2026-10-14 09:00」（或毫秒数）、location 可选、allDay 全天档 true、alarmMinutes 提前提醒分钟、onethuWidget=true 时上手机桌面小组件。",
        params: p(json!({
            "summary": { "type": "string" },
            "start": { "type": "string", "description": "「2026-10-14 09:00」或毫秒数" },
            "end": { "type": "string", "description": "同 start；缺省=同刻" },
            "location": { "type": "string", "description": "地点（可选）" },
            "allDay": { "type": "boolean", "description": "全天日程（可选）" },
            "alarmMinutes": { "type": "number", "description": "提前提醒分钟（可选）" },
            "onethuWidget": { "type": "boolean", "description": "进手机小组件（可选）" }
        }), &["summary", "start"]),
        confirm: false,
    },
    ToolDef {
        name: "ext_event_delete",
        desc: "删除一条本地日程（需先向用户确认）。uid 来自 ext_event_list。",
        params: p(json!({ "uid": { "type": "string" } }), &["uid"]),
        confirm: true,
    },
    ToolDef {
        name: "ext_event_list",
        desc: "列出未来 7 天内 + 昨天起的本地与手动日程（uid/time/标题），用于增删前对号。",
        params: p(json!({}), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ext_plugin_installed",
        desc: "列出本机已安装插件（id/名字/是否启用）。",
        params: p(json!({}), &[]),
        confirm: false,
    },
    ToolDef {
        name: "ext_plugin_install",
        desc: "从 GitHub 仓库一键安装插件（需要先向用户确认）：repo 支持 user/repo、user/repo@dev、完整 URL 或子目录。安装走市场同一校验管线（清单校验 + 权限确认）。",
        params: p(json!({
            "repo": { "type": "string", "description": "仓库地址或简写" },
            "entry": { "type": "string", "description": "入口文件名（可选，缺省按 plugin.js/index.js/main.js 找）" }
        }), &["repo"]),
        confirm: true,
    },
    ToolDef {
        name: "ext_plugin_toggle",
        desc: "启用/停用插件（需要先向用户确认）。id 来自 ext_plugin_installed。",
        params: p(json!({ "id": { "type": "string" }, "on": { "type": "boolean" } }), &["id", "on"]),
        confirm: true,
    },
    ToolDef {
        name: "ext_plugin_uninstall",
        desc: "卸载并删除插件（需要先向用户确认，删除后重新安装才能恢复）。id 来自 ext_plugin_installed。",
        params: p(json!({ "id": { "type": "string" } }), &["id"]),
        confirm: true,
    },
    ToolDef {
        name: "add_favorite",
        desc: "把一条信息收进用户收藏夹（万物原子化）。key 为稳定引用标识（建议 fav:<毫秒时间戳>），title 为收藏卡片显示的标题（必填），note 为补充说明（可选，显示在第二行）。收藏后用户在收藏夹页可见，点击会提示来源。",
        params: json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "收藏卡片标题（如「图书馆 3F-12 座位」）" },
                "key": { "type": "string", "description": "稳定引用标识（如 fav:1730000000000）" },
                "note": { "type": "string", "description": "补充说明（可选）" }
            },
            "required": ["title", "key"]
        }),
        confirm: false,
    },
    ]
}

/// OpenAI tools 参数（静态校园工具集）
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

/// OpenAI tools 参数：校园工具集 + MCP 动态工具（设置 mcpServers 配置的 server 逐个列出）
pub fn schema_with(mcp_servers: &[crate::mcp::McpServerDef]) -> Vec<Value> {
    let mut out = schema();
    if mcp_servers.is_empty() {
        return out;
    }
    for def in mcp_servers {
        if let Ok(tools) = crate::mcp::list_tools(def) {
            for t in tools {
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": crate::mcp::tool_full_name(&t.server, &t.name),
                        "description": format!("（MCP·{}）{}", t.server, t.desc),
                        "parameters": t.params
                    }
                }));
            }
        }
    }
    out
}

pub fn is_confirm(name: &str) -> bool {
    all_tools().iter().find(|t| t.name == name).map(|t| t.confirm).unwrap_or(false)
}

/// 两段式确认的确认摘要（把对象解析好再给用户看）
fn build_summary(name: &str, args: &Value, ctx: &mut Ctx) -> Result<String, String> {
    match name {
        "cloud_share_file" => {
            Ok(format!(
                "生成云盘分享链接：{}{}（{}）",
                s(args, "repo"),
                s(args, "path"),
                if n(args, "days") > 0 { format!("{} 天有效", n(args, "days")) } else { "永久有效".into() }
            ))
        }
        "cloud_upload_file" => {
            Ok(format!(
                "上传 {} 到云盘 {}{}",
                s(args, "localPath"),
                s(args, "repo"),
                if s(args, "path").is_empty() || s(args, "path") == "/" { "（根目录）".into() } else { s(args, "path") }
            ))
        }
        "send_mail" => {
            let to = s(args, "to");
            let subject = s(args, "subject");
            let body = s(args, "body");
            let cc = s(args, "cc");
            Ok(format!(
                "发邮件给 {}{}：{}（正文 {} 字）",
                to,
                if cc.is_empty() { String::new() } else { format!("（抄送 {}）", cc) },
                if subject.is_empty() { "(无主题)" } else { &subject },
                body.chars().count()
            ))
        }
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
        "reply_learn_thread" => Ok(format!(
            "在网络学堂帖子 {} 下回帖：{}",
            s(args, "threadId"),
            s(args, "content").chars().take(80).collect::<String>()
        )),
        "post_learn_thread" => Ok(format!(
            "在网络学堂课程 {} 版面 {} 发新帖《{}》",
            s(args, "wlkcid"),
            s(args, "bqid"),
            s(args, "title")
        )),
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
        "add_schedule" => {
            let date = {
                let raw = s(args, "date");
                match resolve_date(&raw) {
                    Some(d) if d == today() => "今天".to_string(),
                    Some(d) if d == today() + Duration::days(1) => "明天".to_string(),
                    Some(d) => d.format("%Y-%m-%d").to_string(),
                    None => raw,
                }
            };
            let all_day = args.get("allDay").and_then(|v| v.as_bool()).unwrap_or(false);
            let when = if all_day { format!("{}（全天）", date) } else { format!("{} {}-{}", date, or_default(&s(args, "start"), "08:00"), or_default(&s(args, "end"), "09:35")) };
            let loc = s(args, "location");
            Ok(format!("新建日程「{}」：{}{}", s(args, "title"), when, if loc.is_empty() { String::new() } else { format!(" @{}", loc) }))
        }
        "remove_schedule" => Ok(format!("删除日程（uid={}）", s(args, "uid"))),
        "edit_schedule" => {
            let uid = s(args, "uid");
            // 拉近期日程找旧值（±400 天窗口），找不到就只展示新值（执行时会给出权威报错）
            let from = (today() - Duration::days(400)).format("%Y-%m-%d").to_string();
            let to = (today() + Duration::days(400)).format("%Y-%m-%d").to_string();
            let rec = host(ctx, "cal", "agenda", json!([from, to]))
                .ok()
                .map(|v| arr_of(&v))
                .and_then(|arr| arr.into_iter().find(|r| s(r, "uid") == uid));
            let (od, ost, oen, old_all_day) = rec
                .as_ref()
                .map(|r| (s(r, "date"), s(r, "start"), s(r, "end"), r.get("allDay").and_then(|v| v.as_bool()).unwrap_or(false)))
                .unwrap_or_default();
            // 新 when：新字段在旧值基础上合并（与 facade cal.edit 同语义）
            let date = {
                let raw = s(args, "date");
                if raw.is_empty() {
                    od.clone()
                } else {
                    resolve_date(&raw).ok_or("无法理解 date 日期")?.format("%Y-%m-%d").to_string()
                }
            };
            let all_day = args.get("allDay").and_then(|v| v.as_bool()).unwrap_or(old_all_day);
            let when = if all_day {
                format!("{}（全天）", date)
            } else {
                let st = or_default(&s(args, "start"), &ost);
                let en = or_default(&s(args, "end"), &oen);
                format!("{} {}-{}", date, st, en)
            };
            let old_when = if rec.is_none() || (od.is_empty() && ost.is_empty()) {
                String::new()
            } else if old_all_day {
                format!("{}（全天）", od)
            } else {
                format!("{} {}-{}", od, ost, oen)
            };
            let old_title = rec.as_ref().map(|r| s(r, "title")).unwrap_or_default();
            let mut extra = String::new();
            if let Some(t) = args.get("title").and_then(|v| v.as_str()) {
                if !t.trim().is_empty() {
                    extra.push_str(&format!("；标题「{}」→「{}」", old_title, t));
                }
            }
            for (k, label) in [("location", "地点"), ("note", "备注")] {
                if let Some(v) = args.get(k).and_then(|x| x.as_str()) {
                    let old = rec.as_ref().map(|r| s(r, k)).unwrap_or_default();
                    let shown = |x: &str| if x.is_empty() { "（无）".to_string() } else { x.to_string() };
                    extra.push_str(&format!("；{} {}→{}", label, shown(&old), shown(v)));
                }
            }
            if let Some(b) = args.get("toCloud").and_then(|v| v.as_bool()) {
                let from_side = rec.as_ref().map(|r| s(r, "source")).unwrap_or_default();
                let arrow = if b { "本机→云日历（多设备可见）" } else { "云日历→本机（仅本机）" }.to_string();
                if from_side.is_empty() {
                    extra.push_str(&format!("；{}", arrow));
                } else {
                    extra.push_str(&format!("；存储 {}", arrow));
                }
            }
            let head = if old_title.is_empty() { s(args, "title") } else { old_title.clone() };
            let arrow = if old_when.is_empty() || old_when == when {
                when.clone()
            } else {
                format!("{} → {}", old_when, when)
            };
            Ok(format!("修改日程「{}」（uid {}）：{}{}", head, uid.chars().take(12).collect::<String>(), arrow, extra))
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
pub fn execute(ctx: &mut Ctx, name: &str, args: &Value, confirmed: bool, mcp_servers: &[crate::mcp::McpServerDef]) -> Result<ToolOut, String> {
    // MCP 动态工具（B2）：name 形如 mcp_<server>_<tool>
    if let Some(full) = name.strip_prefix("mcp_") {
        for def in mcp_servers {
            let prefix = crate::mcp::tool_full_name(&def.name, "");
            if let Some(tool) = full.strip_prefix(&prefix) {
                let text = crate::mcp::call_tool(def, tool, args)?;
                return Ok(ToolOut::Text(if text.is_empty() { "（MCP 工具执行完成，无输出）".into() } else { text }));
            }
        }
        return Ok(ToolOut::Text(format!("未找到 MCP server：{name}（检查设置 mcpServers）")));
    }
    // 联动插件（B1）：插件命令工具化——模型经宿主门禁调用已启用插件
    if name == "list_plugin_cmds" {
        let v = host(ctx, "plugins", "list", json!([]))?;
        let mut lines: Vec<String> = Vec::new();
        if let Some(arr) = v.as_array() {
            for p in arr {
                let pid = p.get("pluginId").and_then(|x| x.as_str()).unwrap_or("");
                if let Some(cmds) = p.get("commands").and_then(|x| x.as_array()) {
                    for c in cmds {
                        let cid = c.get("id").and_then(|x| x.as_str()).unwrap_or("");
                        let title = c.get("title").and_then(|x| x.as_str()).unwrap_or("");
                        let il = c.get("inputLabel").and_then(|x| x.as_str()).unwrap_or("");
                        let io = if il.is_empty() { "（无输入）".to_string() } else { format!("（输入：{il}）") };
                        lines.push(format!("- pluginId={pid} cmdId={cid} 「{title}」{io}"));
                    }
                }
            }
        }
        let text = if lines.is_empty() {
            "当前没有已启用插件提供命令。".to_string()
        } else {
            format!("可用插件命令：\n{}", lines.join("\n"))
        };
        return Ok(ToolOut::Text(text));
    }
    // ── OH 记忆（引擎在宿主 TS 侧；写操作走 confirm 两段式，权限门禁 memory:read/write）──
    if name == "memory_search" {
        let q = json!({
            "query": s(args, "query"),
            "tags": args.get("tags").cloned().unwrap_or(json!([])),
            "type": s(args, "type"),
            "folder": s(args, "folder"),
            "limit": args.get("limit").cloned().unwrap_or(json!(8)),
        });
        let v = host(ctx, "memory", "search", json!([q]))?;
        let hits = v.as_array().cloned().unwrap_or_default();
        if hits.is_empty() {
            return Ok(ToolOut::Text("没有找到相关记忆（可换关键词，或用 memory_list 浏览）。".into()));
        }
        let mut out = String::new();
        for h in hits {
            let p = h.get("permalink").and_then(|x| x.as_str()).unwrap_or("");
            let t = h.get("title").and_then(|x| x.as_str()).unwrap_or("");
            let tags = h
                .get("tags")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","))
                .unwrap_or_default();
            let sn = h.get("snippet").and_then(|x| x.as_str()).unwrap_or("");
            let via = h.get("via").and_then(|x| x.as_str()).unwrap_or("");
            if via.is_empty() {
                out.push_str(&format!("- {p} 「{t}」 tags=[{tags}] {sn}\n"));
            } else {
                out.push_str(&format!("- {p} 「{t}」 tags=[{tags}] 关联自「{via}」\n"));
            }
        }
        return Ok(ToolOut::Text(out));
    }
    if name == "memory_read" {
        let permalink = s(args, "permalink");
        if permalink.is_empty() {
            return Ok(ToolOut::Text("缺少 permalink（用 memory_search/memory_list 获取）".into()));
        }
        let v = host(ctx, "memory", "read", json!([permalink]))?;
        return Ok(ToolOut::Text(match v {
            Value::String(t) => t,
            other => serde_json::to_string_pretty(&other).unwrap_or_default(),
        }));
    }
    if name == "memory_list" {
        let opts = json!({ "folder": s(args, "folder"), "tag": s(args, "tag") });
        let v = host(ctx, "memory", "list", json!([opts]))?;
        let items = v.as_array().cloned().unwrap_or_default();
        if items.is_empty() {
            return Ok(ToolOut::Text("记忆库为空（或该目录/标签下没有条目）。".into()));
        }
        let mut out = format!("共 {} 条：\n", items.len());
        for it in items {
            let p = it.get("permalink").and_then(|x| x.as_str()).unwrap_or("");
            let t = it.get("title").and_then(|x| x.as_str()).unwrap_or("");
            let ty = it.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let f = it.get("folder").and_then(|x| x.as_str()).unwrap_or("");
            let m = it.get("modified").and_then(|x| x.as_str()).unwrap_or("");
            out.push_str(&format!("- {p} 「{t}」 {ty} {f} {m}\n"));
        }
        return Ok(ToolOut::Text(out));
    }
    if name == "memory_write" {
        let input = json!({
            "title": s(args, "title"),
            "observations": args.get("observations").cloned().unwrap_or(json!([])),
            "relations": args.get("relations").cloned().unwrap_or(json!([])),
            "tags": args.get("tags").cloned().unwrap_or(json!([])),
            "type": s(args, "type"),
        });
        let v = host(ctx, "memory", "write", json!([input]))?;
        let p = v.get("permalink").and_then(|x| x.as_str()).unwrap_or("");
        return Ok(ToolOut::Text(format!("已创建记忆：{p}")));
    }
    if name == "memory_append" {
        let _ = host(ctx, "memory", "append", json!([
            s(args, "permalink"), s(args, "category"), s(args, "text"), s(args, "context")
        ]))?;
        return Ok(ToolOut::Text("已追加观察。".into()));
    }
    if name == "memory_edit" {
        let _ = host(ctx, "memory", "edit", json!([
            s(args, "permalink"), s(args, "find"), s(args, "replace")
        ]))?;
        return Ok(ToolOut::Text("已改写。".into()));
    }
    if name == "memory_delete" {
        let v = host(ctx, "memory", "delete", json!([
            s(args, "permalink"), s(args, "reason")
        ]))?;
        let t = v.get("trashedTo").and_then(|x| x.as_str()).unwrap_or("");
        return Ok(ToolOut::Text(format!("已归档（软删除）到 {t}")));
    }
    if name == "memory_refresh" {
        let v = host(ctx, "memory", "refresh", json!([]))?;
        let n = v.get("count").and_then(|x| x.as_i64()).unwrap_or(0);
        return Ok(ToolOut::Text(format!("已重扫记忆镜像并重建账本（{n} 条）。")));
    }
    if name == "run_plugin_cmd" {
        let pid = s(args, "pluginId");
        let cid = s(args, "cmdId");
        let input = s(args, "input");
        if pid.is_empty() || cid.is_empty() {
            return Ok(ToolOut::Text("缺少 pluginId 或 cmdId：先用 list_plugin_cmds 查可用命令。".into()));
        }
        let v = host(ctx, "plugins", "call", json!([pid, cid, input]))?;
        let text = match v {
            Value::String(t) => t,
            other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| "（无输出）".into()),
        };
        return Ok(ToolOut::Text(if text.is_empty() { "（命令执行完成，无输出）".into() } else { text }));
    }
    if name == "list_favorite_kinds" {
        let v = host(ctx, "favorites", "kinds", json!([]))?;
        let mut lines: Vec<String> = Vec::new();
        if let Some(arr) = v.as_array() {
            for k in arr {
                let kind = k.get("kind").and_then(|x| x.as_str()).unwrap_or("");
                let group = k.get("group").and_then(|x| x.as_str()).unwrap_or("");
                if !kind.is_empty() {
                    lines.push(format!("- kind={kind} （{group}）"));
                }
            }
        }
        let text = if lines.is_empty() {
            "当前没有可收藏的原子种类。".to_string()
        } else {
            format!("可收藏的原子种类：\n{}", lines.join("\n"))
        };
        return Ok(ToolOut::Text(text));
    }
    /* —— 用户扩展层元工具：全部转发 TS 侧 ext 命名空间（持久化与执行器在 TS）—— */
    if name == "ext_flow_create" {
        let out = host(ctx, "ext", "flowCreate", json!([s(args, "name"), args.get("trigger").cloned().unwrap_or(json!({"kind":"manual"})), args.get("steps").cloned().unwrap_or(json!([]))]))?;
        return Ok(ToolOut::Text(format!("OneFlow 已创建并生效：{}", out)));
    }
    if name == "ext_flow_list" {
        let out = host(ctx, "ext", "flowList", json!([]))?;
        return Ok(ToolOut::Text(out.to_string()));
    }
    if name == "ext_flow_run" {
        let out = host(ctx, "ext", "flowRun", json!([s(args, "id")]))?;
        return Ok(ToolOut::Text(out.to_string()));
    }
    if name == "ext_flow_delete" {
        let out = host(ctx, "ext", "flowDelete", json!([s(args, "id")]))?;
        return Ok(ToolOut::Text("OneFlow 已删除。".into()));
    }
    if name == "ext_tool_create" {
        let out = host(ctx, "ext", "toolCreate", json!([s(args, "name"), s(args, "description"), args.get("steps").cloned().unwrap_or(json!([]))]))?;
        return Ok(ToolOut::Text(format!("用户工具已创建：{}", out)));
    }
    if name == "ext_tool_list" {
        let out = host(ctx, "ext", "toolList", json!([]))?;
        return Ok(ToolOut::Text(out.to_string()));
    }
    if name == "ext_tool_run" {
        let out = host(ctx, "ext", "toolRun", json!([s(args, "id")]))?;
        return Ok(ToolOut::Text(out.to_string()));
    }
    if name == "ext_tool_delete" {
        let out = host(ctx, "ext", "toolDelete", json!([s(args, "id")]))?;
        return Ok(ToolOut::Text("用户工具已删除。".into()));
    }
    if name == "ext_theme_apply" {
        let out = host(ctx, "ext", "themeApply", json!([s(args, "name"), args.get("vars").cloned().unwrap_or(json!({}))]))?;
        return Ok(ToolOut::Text(format!("主题已创建并应用：{}", out)));
    }
    if name == "ext_theme_list" {
        let out = host(ctx, "ext", "themeList", json!([]))?;
        return Ok(ToolOut::Text(out.to_string()));
    }
    if name == "ext_theme_activate" {
        let out = host(ctx, "ext", "themeActivate", json!([args.get("id").cloned()]))?;
        return Ok(ToolOut::Text("主题已切换。".into()));
    }
    if name == "ext_theme_delete" {
        let out = host(ctx, "ext", "themeDelete", json!([s(args, "id")]))?;
        return Ok(ToolOut::Text("主题已删除。".into()));
    }
    if name == "ext_widget_create" {
        let out = host(ctx, "ext", "widgetCreate", json!([s(args, "name"), s(args, "html"), args.get("x").cloned(), args.get("y").cloned()]))?;
        return Ok(ToolOut::Text(format!("组件已放到首页白板：{}", out)));
    }
    if name == "ext_widget_list" {
        let out = host(ctx, "ext", "widgetList", json!([]))?;
        let rows: Vec<String> = out
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|w| {
                        format!(
                            "id={} {}",
                            w.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                            w.get("name").and_then(|v| v.as_str()).unwrap_or("")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if rows.is_empty() {
            return Ok(ToolOut::Text("还没有用户生成组件。ext_widget_create 可以现场做一个。".into()));
        }
        return Ok(ToolOut::Text(rows.join("\n")));
    }
    if name == "ext_widget_delete" {
        host(ctx, "ext", "widgetDelete", json!([s(args, "id")]))?;
        return Ok(ToolOut::Text("组件已删除（白板上的实例一并清掉）。".into()));
    }
    if name == "ext_onepage_create" {
        let out = host(ctx, "ext", "onePageCreate", json!([s(args, "name"), args.get("icon").cloned()]))?;
        let id = out.get("id").and_then(|v| v.as_str()).unwrap_or("");
        return Ok(ToolOut::Text(format!("OnePage 已创建（id={id}）。往里放组件用 ext_onepage_add。")));
    }
    if name == "ext_onepage_list" {
        let out = host(ctx, "ext", "onePageList", json!([]))?;
        let rows: Vec<String> = out
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|b| {
                        format!(
                            "id={} [{}] {}",
                            b.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                            b.get("kind").and_then(|v| v.as_str()).unwrap_or(""),
                            b.get("name").and_then(|v| v.as_str()).unwrap_or("")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        return Ok(ToolOut::Text(rows.join("\n")));
    }
    if name == "ext_onepage_add" {
        let out = host(ctx, "ext", "onePageAddItem", json!([
            s(args, "boardId"),
            s(args, "kind"),
            args.get("payload").cloned().unwrap_or(json!({})),
            args.get("x").cloned(),
            args.get("y").cloned(),
            args.get("w").cloned(),
            args.get("h").cloned(),
        ]))?;
        let id = out.get("id").and_then(|v| v.as_str()).unwrap_or("");
        return Ok(ToolOut::Text(format!("组件已放到 OnePage（it={id}）。")));
    }
    if name == "ext_phone_push" {
        let title = s(args, "title");
        let rows = args.get("rows").cloned().unwrap_or(json!([]));
        if !rows.is_array() {
            return Ok(ToolOut::Text("rows 必须是数组（每行 {text, sub?}）；清空也传空数组。".into()));
        }
        let n = rows.as_array().map(|a| a.len()).unwrap_or(0);
        host(ctx, "ext", "phoneWidgetSet", json!([title, rows]))?;
        return Ok(ToolOut::Text(if n == 0 {
            "已清空手机小组件自定义内容（还原插件默认）。".into()
        } else {
            format!("已推 {n} 行到手机桌面小组件，App 下次进前台即生效。")
        }));
    }
    if name == "onePage_write" {
        let op = s(args, "op");
        let id = s(args, "id");
        match op.as_str() {
            "rename" => {
                let nm = s(args, "name");
                if nm.is_empty() {
                    return Ok(ToolOut::Text("改名需要 name。".into()));
                }
                host(ctx, "ext", "onePageRename", json!([id, nm]))?;
                return Ok(ToolOut::Text("已改名。".into()));
            }
            "delete" => {
                host(ctx, "ext", "onePageDelete", json!([id]))?;
                return Ok(ToolOut::Text("OnePage 已删除（板和内容都清了）。".into()));
            }
            "open" => {
                host(ctx, "ext", "onePageOpen", json!([id]))?;
                return Ok(ToolOut::Text("已打开这块 OnePage。".into()));
            }
            "items" => {
                let out = host(ctx, "ext", "onePageItems", json!([id]))?;
                let rows: Vec<String> = out
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|it| {
                                format!(
                                    "id={} kind={} cell=({},{}) {}",
                                    it.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                                    it.get("kind").and_then(|v| v.as_str()).unwrap_or(""),
                                    it.get("x").and_then(|v| v.as_i64()).unwrap_or(0),
                                    it.get("y").and_then(|v| v.as_i64()).unwrap_or(0),
                                    it.get("text").and_then(|v| v.as_str()).unwrap_or("")
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok(ToolOut::Text(if rows.is_empty() { "这块板还没有组件（onePage_place 只动已有件；放新件用 ext_onepage_add）。".into() } else { rows.join("\n") }));
            }
            _ => return Ok(ToolOut::Text("op 必须是 rename/delete/open/items。".into())),
        }
    }
    if name == "onePage_place" {
        let op = s(args, "op");
        let board = s(args, "boardId");
        let item = s(args, "itemId");
        if op == "move" {
            host(ctx, "ext", "onePageMoveItem", json!([board, item, n(args, "x"), n(args, "y")]))?;
            return Ok(ToolOut::Text("组件已挪位。".into()));
        }
        if op == "remove" {
            host(ctx, "ext", "onePageRemoveItem", json!([board, item]))?;
            return Ok(ToolOut::Text("组件已摘除。".into()));
        }
        return Ok(ToolOut::Text("op 必须是 move / remove。".into()));
    }
    if name == "workspace_write" {
        let op = s(args, "op");
        match op.as_str() {
            "list" => {
                let out = host(ctx, "ext", "workspaceList", json!([]))?;
                let rows: Vec<String> = out
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|w| {
                                format!(
                                    "id={} [{}] {}（{} 个标签）",
                                    w.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                                    if w.get("live").and_then(|v| v.as_bool()).unwrap_or(false) { "活板" } else { "存档" },
                                    w.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                                    w.get("tabs").and_then(|v| v.as_i64()).unwrap_or(0)
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok(ToolOut::Text(if rows.is_empty() { "还没有工作区。op=create 可新建。".into() } else { rows.join("\n") }));
            }
            "create" => {
                let out = host(ctx, "ext", "workspaceCreate", json!([s(args, "name"), args.get("tabIds").cloned().unwrap_or(json!(null))]))?;
                let id = out.get("id").and_then(|v| v.as_str()).unwrap_or("");
                return Ok(ToolOut::Text(format!("工作区已创建（id={id}）。")));
            }
            "bring" => {
                host(ctx, "ext", "workspaceOpen", json!([s(args, "id")]))?;
                return Ok(ToolOut::Text("已打开为当前工作组。".into()));
            }
            "collapse" => {
                host(ctx, "ext", "workspaceClose", json!([s(args, "id")]))?;
                return Ok(ToolOut::Text("活板已收起。".into()));
            }
            "spread" => {
                host(ctx, "ext", "workspaceDissolve", json!([s(args, "id")]))?;
                return Ok(ToolOut::Text("工作区已解除（成组标签复位为散标签）。".into()));
            }
            _ => return Ok(ToolOut::Text("op 必须是 list/create/bring/collapse/spread。删除已存档的工作区用 ext_workspace_delete。".into())),
        }
    }
    if name == "ext_workspace_delete" {
        host(ctx, "ext", "workspaceDelete", json!([s(args, "id")]))?;
        return Ok(ToolOut::Text("工作区定义已删除。".into()));
    }
    if name == "ext_tab" {
        let op = s(args, "op");
        match op.as_str() {
            "list" => {
                let out = host(ctx, "ext", "tabList", json!([]))?;
                let rows: Vec<String> = out
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|t| {
                                format!(
                                    "id={} 「{}」",
                                    t.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                                    t.get("title").and_then(|v| v.as_str()).unwrap_or("")
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok(ToolOut::Text(rows.join("\n")));
            }
            "open" => {
                let out = host(ctx, "ext", "tabOpen", json!([s(args, "page")]))?;
                let id = out.get("id").and_then(|v| v.as_str()).unwrap_or("");
                return Ok(ToolOut::Text(format!("新标签已打开（id={id}）。")));
            }
            "focus" => {
                host(ctx, "ext", "tabFocus", json!([s(args, "id")]))?;
                return Ok(ToolOut::Text("已跳到该标签。".into()));
            }
            "duplicate" => {
                let out = host(ctx, "ext", "tabDuplicate", json!([s(args, "id")]))?;
                let id = out.get("id").and_then(|v| v.as_str()).unwrap_or("");
                return Ok(ToolOut::Text(format!("已复制标签（新 id={id}）。")));
            }
            _ => return Ok(ToolOut::Text("op 必须是 list/open/focus/duplicate。".into())),
        }
    }
    if name == "ext_tab_close" {
        host(ctx, "ext", "tabClose", json!([s(args, "id")]))?;
        return Ok(ToolOut::Text("标签已关闭。".into()));
    }
    if name == "ext_wallpaper" {
        if let Some(r) = args.get("ref").cloned() {
            let refv = match &r {
                Value::Null => json!(null),
                other => other.clone(),
            };
            host(ctx, "ext", "wallpaperSet", json!([s(args, "scope"), refv]))?;
        }
        let cur = host(ctx, "ext", "wallpaperGet", json!([]))?;
        return Ok(ToolOut::Text(format!(
            "全屏档={}；本页档={}（builtin: 前缀是内置渐变，null 是没设）",
            cur.get("global").map(|v| v.as_str().unwrap_or("null")).unwrap_or("null"),
            cur.get("tab").map(|v| v.as_str().unwrap_or("null")).unwrap_or("null")
        )));
    }
    if name == "ext_voice" {
        let out = host(ctx, "ext", "voiceSet", json!([args.clone()]))?;
        return Ok(ToolOut::Text(serde_json::to_string_pretty(&out).unwrap_or_default()));
    }
    if name == "ext_prefs" {
        host(ctx, "ext", "prefSet", json!([args.get("capsuleTap").cloned().map(|v| json!({ "capsuleTap": v })).unwrap_or(json!({}))]))?;
        host(ctx, "ext", "prefSet", json!([args.get("voiceTarget").cloned().map(|v| json!({ "voiceTarget": v })).unwrap_or(json!({}))]))?;
        let out = host(ctx, "ext", "prefGet", json!([]))?;
        return Ok(ToolOut::Text(serde_json::to_string_pretty(&out).unwrap_or_default()));
    }
    if name == "ext_theme_schedule" {
        let q = json!({
            "followSystem": args.get("followSystem").cloned().unwrap_or(Value::Null),
            "dayThemeId": args.get("dayThemeId").cloned().unwrap_or(Value::Null),
            "nightThemeId": args.get("nightThemeId").cloned().unwrap_or(Value::Null),
        });
        let out = host(ctx, "ext", "themeScheduleSet", json!([q]))?;
        return Ok(ToolOut::Text(serde_json::to_string_pretty(&out).unwrap_or_default()));
    }
    if name == "ext_notify" {
        let out = host(ctx, "ext", "notifySet", json!([args.clone()]))?;
        return Ok(ToolOut::Text(serde_json::to_string_pretty(&out).unwrap_or_default()));
    }
    if name == "ext_hw_timeline" {
        let out = host(ctx, "ext", "hwTimelineSet", json!([n(args, "minutes")]))?;
        let m = out.get("minutes").and_then(|v| v.as_i64()).unwrap_or(0);
        return Ok(ToolOut::Text(format!("作业提醒默认提前 {m} 分钟。")));
    }
    if name == "ext_hw_reminder" {
        let minutes = args.get("minutes").cloned().unwrap_or(Value::Null);
        host(ctx, "ext", "hwReminderSet", json!([s(args, "id"), minutes]))?;
        return Ok(ToolOut::Text("这条作业的提醒已更新。".into()));
    }
    if name == "ext_event_add" {
        let out = host(ctx, "ext", "eventAdd", json!([args.clone()]))?;
        let uid = out.get("uid").and_then(|v| v.as_str()).unwrap_or("");
        return Ok(ToolOut::Text(format!("日程已加（uid={uid}）。")));
    }
    if name == "ext_event_delete" {
        host(ctx, "ext", "eventDelete", json!([s(args, "uid")]))?;
        return Ok(ToolOut::Text("日程已删除。".into()));
    }
    if name == "ext_event_list" {
        let out = host(ctx, "ext", "eventList", json!([]))?;
        let rows: Vec<String> = out
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|e| {
                        format!(
                            "uid={} {} 「{}」",
                            e.get("uid").and_then(|v| v.as_str()).unwrap_or(""),
                            e.get("start").and_then(|v| v.as_i64()).map(fmt_mail_ms).unwrap_or_default(),
                            e.get("summary").and_then(|v| v.as_str()).unwrap_or("")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        return Ok(ToolOut::Text(if rows.is_empty() { "近 8 天没有本地日程。".into() } else { rows.join("\n") }));
    }
    if name == "ext_plugin_installed" {
        let out = host(ctx, "ext", "pluginInstalledList", json!([]))?;
        let rows: Vec<String> = out
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|p| {
                        format!(
                            "id={} [{}] {}",
                            p.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                            if p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false) { "启用" } else { "停用" },
                            p.get("name").and_then(|v| v.as_str()).unwrap_or("")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        return Ok(ToolOut::Text(if rows.is_empty() { "本机没有已安装插件（可以从插件市场或 GitHub 装）。".into() } else { rows.join("\n") }));
    }
    if name == "ext_plugin_install" {
        let out = host(ctx, "ext", "pluginInstallFromRepo", json!([s(args, "repo"), args.get("entry").and_then(|v| if v.is_null() { None } else { v.as_str() })]))?;
        let id = out.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let nm = out.get("name").and_then(|v| v.as_str()).unwrap_or("");
        return Ok(ToolOut::Text(format!("插件「{nm}」已安装（id={id}），装好即在插件页可见。")));
    }
    if name == "ext_plugin_toggle" {
        host(ctx, "ext", "pluginEnableToggle", json!([s(args, "id"), args.get("on").and_then(|v| v.as_bool()).unwrap_or(true)]))?;
        return Ok(ToolOut::Text("插件状态已切换。".into()));
    }
    if name == "ext_plugin_uninstall" {
        host(ctx, "ext", "pluginUninstall", json!([s(args, "id")]))?;
        return Ok(ToolOut::Text("插件已卸载删除。".into()));
    }
    if name == "add_favorite" {
        let title = s(args, "title");
        let key = s(args, "key");
        let note = s(args, "note");
        if title.is_empty() || key.is_empty() {
            return Ok(ToolOut::Text("缺少 title 或 key。".into()));
        }
        let k = if key.starts_with("fav:") { key.clone() } else { format!("fav:{}", key) };
        let refv = json!({ "kind": "plugin:onethu.harness", "key": k });
        let meta = if note.is_empty() {
            json!({ "title": title, "group": "对话收藏" })
        } else {
            json!({ "title": title, "sub": note, "group": "对话收藏" })
        };
        let v = host(ctx, "favorites", "addAtom", json!([refv, meta]))?;
        let ok = v.is_null() || v.as_bool() == Some(true) || !v.is_object();
        let _ = ok;
        return Ok(ToolOut::Text(format!("已收藏「{title}」，可在收藏夹页查看。")));
    }
    if is_confirm(name) && !confirmed {
        let summary = build_summary(name, args, ctx)?;
        return Ok(ToolOut::ConfirmNeeded { tool: name.into(), args: args.clone(), summary });
    }
    let out: Value = match name {
        "query_mails" => {
            let folder = match s(args, "folder").to_lowercase().as_str() {
                "sent" | "已发送" => "Sent Items",
                _ => "INBOX",
            };
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(5).clamp(1, 20) as usize;
            let r = host(ctx, "mail", "list", json!([folder, limit]))?;
            let mails = r.get("mails").and_then(|m| m.as_array()).cloned().unwrap_or_default();
            let total = n(&r, "total");
            let lines: Vec<String> = mails
                .iter()
                .map(|m| {
                    format!(
                        "uid={}{} {} | {} | {}",
                        m.get("uid").and_then(|v| v.as_i64()).unwrap_or(0),
                        if m.get("seen").and_then(|v| v.as_bool()).unwrap_or(true) { "" } else { " [未读]" },
                        fmt_mail_ms(m.get("dateMs").and_then(|v| v.as_i64()).unwrap_or(0)),
                        m.get("from").and_then(|v| v.as_str()).unwrap_or(""),
                        m.get("subject").and_then(|v| v.as_str()).unwrap_or(""),
                    )
                })
                .collect();
            json!({
                "folder": if folder == "INBOX" { "收件箱" } else { "已发送" },
                "total": total,
                "count": lines.len(),
                "mails": lines,
                "hint": "read_mail 可看某封正文；total 大于 count 时说明还有更早的邮件"
            })
        }
        "read_mail" => {
            let folder = match s(args, "folder").to_lowercase().as_str() {
                "sent" | "已发送" => "Sent Items",
                _ => "INBOX",
            };
            let uid = n(args, "uid");
            let r = host(ctx, "mail", "read", json!([folder, uid]))?;
            let text = s(&r, "text");
            let truncated = text.chars().count() > 6000;
            let shown: String = text.chars().take(6000).collect();
            json!({
                "subject": s(&r, "subject"),
                "from": s(&r, "from"),
                "to": s(&r, "to"),
                "date": fmt_mail_ms(r.get("dateMs").and_then(|v| v.as_i64()).unwrap_or(0)),
                "body": shown,
                "truncated": truncated,
                "hint": if truncated { "正文过长已截断（原文更长）" } else { "" }
            })
        }
        "search_mails" => {
            let folder = match s(args, "folder").to_lowercase().as_str() {
                "sent" | "已发送" => "Sent Items",
                _ => "INBOX",
            };
            let query = s(args, "query");
            let r = host(ctx, "mail", "search", json!([folder, query]))?;
            let mails = arr_of(&r);
            let lines: Vec<String> = mails
                .iter()
                .map(|m| {
                    format!(
                        "uid={}{} {} | {} | {}",
                        m.get("uid").and_then(|v| v.as_i64()).unwrap_or(0),
                        if m.get("seen").and_then(|v| v.as_bool()).unwrap_or(true) { "" } else { " [未读]" },
                        fmt_mail_ms(m.get("dateMs").and_then(|v| v.as_i64()).unwrap_or(0)),
                        m.get("from").and_then(|v| v.as_str()).unwrap_or(""),
                        m.get("subject").and_then(|v| v.as_str()).unwrap_or(""),
                    )
                })
                .collect();
            json!({ "query": query, "count": lines.len(), "mails": lines })
        }
        "query_cloud_files" => {
            let repos = arr_of(&host(ctx, "cloud", "repos", json!([]))?);
            let repo_arg = s(args, "repo");
            if repo_arg.is_empty() {
                let names: Vec<String> = repos
                    .iter()
                    .map(|r| format!("{}（{}，{}）", s(r, "name"), fmt_size(r.get("size").and_then(|x| x.as_i64()).unwrap_or(0)), repos.len().to_string()))
                    .collect();
                let _ = &names;
                let lines: Vec<String> = repos
                    .iter()
                    .map(|r| {
                        let sz = r.get("size").and_then(|x| x.as_i64()).unwrap_or(0);
                        format!("{} | {} | {}", s(r, "name"), s(r, "id"), fmt_size(sz))
                    })
                    .collect();
                json!({
                    "count": repos.len(),
                    "repos": lines,
                    "hint": "带 repo=库名 再调一次可列其根目录"
                })
            } else {
                let repo = find_cloud_repo(&repos, &repo_arg)?;
                let path = {
                    let p = s(args, "path");
                    if p.is_empty() { "/".to_string() } else if p.starts_with('/') { p } else { format!("/{p}") }
                };
                let entries = arr_of(&host(ctx, "cloud", "list", json!([s(repo, "id"), path]))?);
                let lines: Vec<String> = entries
                    .iter()
                    .map(|e| {
                        let kind = s(e, "kind");
                        let sz = e.get("size").and_then(|x| x.as_i64()).unwrap_or(0);
                        format!(
                            "[{}] {}{}",
                            if kind == "dir" { "目录" } else { "文件" },
                            s(e, "name"),
                            if kind == "dir" { String::new() } else { format!("（{}）", fmt_size(sz)) }
                        )
                    })
                    .collect();
                json!({ "repo": s(repo, "name"), "path": path, "count": lines.len(), "entries": lines })
            }
        }
        "search_cloud_files" => {
            let repos = arr_of(&host(ctx, "cloud", "repos", json!([]))?);
            let repo = find_cloud_repo(&repos, &s(args, "repo"))?;
            let query = s(args, "query");
            let hits = arr_of(&host(ctx, "cloud", "search", json!([s(repo, "id"), query]))?);
            let lines: Vec<String> = hits
                .iter()
                .map(|e| {
                    let sz = e.get("size").and_then(|x| x.as_i64()).unwrap_or(0);
                    format!("{}（{}）", s(e, "name"), fmt_size(sz))
                })
                .collect();
            json!({ "repo": s(repo, "name"), "query": query, "count": lines.len(), "results": lines })
        }
        "cloud_share_file" => {
            let repos = arr_of(&host(ctx, "cloud", "repos", json!([]))?);
            let repo = find_cloud_repo(&repos, &s(args, "repo"))?;
            let path = {
                let p = s(args, "path");
                if p.starts_with('/') { p } else { format!("/{p}") }
            };
            let days = n(args, "days");
            let r = host(ctx, "cloud", "share", json!([s(repo, "id"), path, days]))?;
            json!({ "success": true, "link": s(&r, "link") })
        }
        "cloud_upload_file" => {
            let repos = arr_of(&host(ctx, "cloud", "repos", json!([]))?);
            let repo = find_cloud_repo(&repos, &s(args, "repo"))?;
            let parent = {
                let p = s(args, "path");
                if p.is_empty() || p == "/" { "/".to_string() } else if p.starts_with('/') { p } else { format!("/{p}") }
            };
            let local = s(args, "localPath");
            let replace = args.get("replace").and_then(|x| x.as_bool()).unwrap_or(false);
            let r = host(ctx, "cloud", "upload", json!([s(repo, "id"), parent, local, replace]))?;
            json!({
                "success": true,
                "repo": s(repo, "name"),
                "path": parent,
                "size": r.get("size").and_then(|x| x.as_i64()).unwrap_or(-1)
            })
        }
        "send_mail" => {
            let to = s(args, "to");
            let cc = s(args, "cc");
            let subject = s(args, "subject");
            let body = s(args, "body");
            host(ctx, "mail", "send", json!([to, cc, subject, body]))?;
            json!({ "success": true, "to": to, "subject": subject })
        }
        "thubook_toc" => {
            let body = thubook_fetch("https://thubook.help/sitemap.xml")?;
            let mut paths: Vec<String> = Vec::new();
            let mut rest = body.as_str();
            while let Some(a) = rest.find("<loc>") {
                let b = match rest[a + 5..].find("</loc>") {
                    Some(n) => a + 5 + n,
                    None => break,
                };
                let loc = rest[a + 5..b].trim().to_string();
                if !loc.is_empty() {
                    paths.push(loc);
                }
                rest = &rest[b + 6..];
            }
            let total = paths.len();
            paths.truncate(400);
            json!({ "count": total, "note": "全部页面 loc；thubook_read 的 path 用域名后部分", "paths": paths })
        }
        "thubook_read" => {
            let raw = s(args, "path");
            let path = if raw.is_empty() { "/thubook/".to_string() } else { raw.to_string() };
            if !path.starts_with('/') || path.contains("..") {
                return Err(format!("非法 path：{path}（须以 / 开头且不含 ..，域内仅 thubook.help）"));
            }
            let body = thubook_fetch(&format!("https://thubook.help{path}"))?;
            let (title, text) = thubook_extract(&body);
            let truncated = text.chars().count() > 9000;
            let text: String = text.chars().take(9000).collect();
            json!({ "path": path, "title": title, "chars": text.chars().count(), "truncated": truncated, "text": text })
        }
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
        "query_agenda" => {
            let start = if s(args, "start").is_empty() {
                today()
            } else {
                resolve_date(&s(args, "start")).ok_or("无法理解 start 日期")?
            };
            let end = if s(args, "end").is_empty() {
                start + Duration::days(13)
            } else {
                resolve_date(&s(args, "end")).ok_or("无法理解 end 日期")?
            };
            host(ctx, "cal", "agenda", json!([start.format("%Y-%m-%d").to_string(), end.format("%Y-%m-%d").to_string()]))?
        }
        "add_schedule" => {
            let date = resolve_date(&s(args, "date")).ok_or("无法理解 date 日期")?;
            let all_day = args.get("allDay").and_then(|v| v.as_bool()).unwrap_or(false);
            let start = if all_day { "00:00".to_string() } else { or_default(&s(args, "start"), "08:00") };
            let end = if all_day { "01:00".to_string() } else { or_default(&s(args, "end"), "09:35") };
            let mut opts = json!({});
            if !s(args, "location").is_empty() {
                opts["location"] = json!(s(args, "location"));
            }
            if !s(args, "note").is_empty() {
                opts["note"] = json!(s(args, "note"));
            }
            if all_day {
                opts["allDay"] = json!(true);
            }
            host(ctx, "cal", "add", json!([s(args, "title"), date.format("%Y-%m-%d").to_string(), start, end, opts]))?
        }
        "remove_schedule" => {
            let uid = s(args, "uid");
            if uid.is_empty() {
                return Err("缺少 uid（先用 query_agenda 拿日程的 uid 字段）".to_string());
            }
            host(ctx, "cal", "remove", json!([uid]))?
        }
        "edit_schedule" => {
            let uid = s(args, "uid");
            if uid.is_empty() {
                return Err("缺少 uid（先用 query_agenda 拿日程的 uid 字段）".to_string());
            }
            let mut ch = json!({});
            if let Some(t) = args.get("title").and_then(|v| v.as_str()) {
                if !t.trim().is_empty() {
                    ch["title"] = json!(t);
                }
            }
            let d = s(args, "date");
            if !d.is_empty() {
                let resolved = resolve_date(&d).ok_or("无法理解 date 日期")?;
                ch["date"] = json!(resolved.format("%Y-%m-%d").to_string());
            }
            // 时间：必须可解析（防「改时间」参数写错被静默保留原时间）
            for k in ["start", "end"] {
                let v = s(args, k);
                if !v.is_empty() {
                    let (h, m) = crate::config::resolve_hhmm(&v).ok_or(format!("无法理解 {k} 时间（应为 HH:MM，如 16:00）"))?;
                    ch[k] = json!(format!("{:02}:{:02}", h, m));
                }
            }
            // location/note：显式传空串=清除，未传=保留（facade 按键是否存在区分）
            for k in ["location", "note"] {
                if let Some(v) = args.get(k).and_then(|x| x.as_str()) {
                    ch[k] = json!(v);
                }
            }
            for k in ["allDay", "toCloud"] {
                if let Some(v) = args.get(k).and_then(|x| x.as_bool()) {
                    ch[k] = json!(v);
                }
            }
            let r = host(ctx, "cal", "edit", json!([uid, ch]))?;
            json!({
                "success": true,
                "uid": s(&r, "uid"),
                "where": if s(&r, "where") == "local" { "本机（不云同步）" } else { "云日历（多设备可见）" },
                "note": "修改已保存；系统日历镜像（若开启）会自动跟进"
            })
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
        "reply_learn_thread" => {
            host(ctx, "learn", "reply", json!([s(args, "wlkcid"), s(args, "threadId"), s(args, "content")]))?;
            json!({ "replied": true, "threadId": s(args, "threadId") })
        }
        "post_learn_thread" => {
            host(ctx, "learn", "post", json!([s(args, "wlkcid"), s(args, "bqid"), s(args, "title"), s(args, "content")]))?;
            json!({ "posted": true, "title": s(args, "title") })
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
                .map(|h| {
                    // facade 回传核心 Homework 形状（deadline/lateDeadline/…，无 endTime/status 字段）
                    let submitted = h.get("submitted").and_then(|v| v.as_bool()).unwrap_or(false);
                    let graded = h.get("graded").and_then(|v| v.as_bool()).unwrap_or(false);
                    let status = if graded { "已批" } else if submitted { "已交" } else { "未交" };
                    let late_due = s(h, "lateDeadline");
                    json!({
                        "course": s(h, "courseName"), "title": s(h, "title"),
                        "due": s(h, "deadline"),
                        "lateDue": if late_due.is_empty() { json!(null) } else { json!(late_due) },
                        "published": s(h, "publishTime"),
                        "status": status, "submitted": submitted, "graded": graded,
                        "submitTime": s(h, "submitTime"),
                        "grade": h.get("grade").cloned().unwrap_or(json!(null)),
                    })
                })
                .collect();
            json!({ "count": rows.len(), "rows": rows, "truncated": rows.len() >= 30 })
        }
        "query_learn_notifications" => {
            let sem = s(args, "semesterId");
            let out = if sem.is_empty() { host(ctx, "learn", "notifications", json!([null]))? } else { host(ctx, "learn", "notifications", json!([sem]))? };
            let rows: Vec<Value> = arr_of(&out)
                .iter()
                .take(20)
                .map(|n| {
                    let exp = s(n, "expireTime");
                    json!({
                        "course": s(n, "courseName"), "title": s(n, "title"),
                        "publisher": s(n, "publisher"), "time": s(n, "publishTime"),
                        "expireTime": if exp.is_empty() { json!(null) } else { json!(exp) },
                        "important": n.get("important").and_then(|v| v.as_bool()).unwrap_or(false),
                        "hasDetail": s(n, "content") != "",
                    })
                })
                .collect();
            json!({ "count": rows.len(), "rows": rows, "truncated": rows.len() >= 20 })
        }
        "query_learn_files" => {
            let kw = s(args, "course_keyword");
            let sem = s(args, "semesterId");
            let sem_arg = if sem.is_empty() { json!([null]) } else { json!([sem]) };
            let courses = arr_of(&host(ctx, "learn", "courses", sem_arg)?.get("courses").cloned().unwrap_or_else(|| json!([])));
            let hit = courses.iter().find(|c| s(c, "name").contains(&kw))
                .ok_or_else(|| format!("找不到课程「{kw}」；可用：{}", courses.iter().map(|c| s(c, "name")).collect::<Vec<_>>().join("、")))?;
            let files = host(ctx, "learn", "files", json!([s(hit, "id")]))?;
            let rows: Vec<Value> = arr_of(&files)
                .iter()
                .take(25)
                .map(|f| json!({ "title": s(f, "title"), "size": s(f, "size"), "time": s(f, "uploadTime") }))
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
            // 人类流程单次完成：搜索（q=课名或教师名单关键词）→ 自动取前几条详情
            // 把 timeLocation（星期节次+教室）合并进行。detail 参数仅保留 false 关闭。
            let q = s(args, "q");
            let sem = s(args, "semester");
            let list = host(ctx, "coursex", "search", json!([q, sem]))?;
            let mut rows: Vec<Value> = arr_of(&list)
                .iter()
                .take(6)
                .map(|it| json!({ "id": s(it, "id"), "name": s(it, "name"), "teacher": s(it, "teacherName"), "timeLocation": "", "semesterId": s(it, "semesterId") }))
                .collect();
            let mut detail_err = String::new();
            if s(args, "detail") != "false" {
                for i in 0..rows.len().min(4) {
                    let id = rows[i].get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if id.is_empty() {
                        continue;
                    }
                    match host(ctx, "coursex", "detail", json!([id])) {
                        Ok(d) => {
                            let tl = d.get("timeLocation").cloned().unwrap_or_else(|| json!([]));
                            if tl.is_array() && !arr_of(&tl).is_empty() {
                                rows[i]["timeLocation"] = tl;
                            }
                        }
                        Err(e) => detail_err = e,
                    }
                }
            }
            json!({
                "count": rows.len(),
                "rows": rows,
                "detailErr": detail_err,
                "note": "timeLocation=星期节次+教室（已自动取详情合并；为空=CourseX 未收录该班时间地点，如实告知）。q 单关键词（课名或教师名，不拼接）；跨学期传 semester",
            })
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
            if rows.is_empty() {
                return Ok(ToolOut::Text(json!({
                    "rows": [],
                    "note": "0 行：q 必须是单个关键词（课名 或 教师名，不拼接）。建议重试：只用「郑莉」，或只用「计算机程序设计基础」，或换 semester",
                }).to_string()));
            }
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
                "note": "q 只填一个关键词：课名 或 教师名，绝不拼接（「郑莉计算机程序设计基础」查不到任何东西）；0 行就换另一个关键词或换 semester。教室在详情：对命中的 id 再 detail=true",
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
        "ui_navigate" => {
            let table = routes_assets();
            if table.is_empty() {
                return Ok(ToolOut::Text(pretty(&json!({
                    "ok": false,
                    "reason": "路由表不可用（routes.json 资产缺失/解析失败）。改动检索行后请重跑 tools/gen-oh-routes.mjs，或先用 open_page 兜底。"
                }))));
            }
            let route_id = s(args, "routeId");
            let q = s(args, "query").trim().to_string();
            if route_id.is_empty() && q.is_empty() {
                return Ok(ToolOut::Text(pretty(&json!({
                    "ok": false,
                    "reason": "缺 routeId 或 query。可先调 ui_route_table 拿行 id，或用关键词（如 多模态 / 管理主题 / 音色）重试。"
                }))));
            }
            let (hit, candidates, reason) = match_route(&table, &route_id, &q);
            let row = match hit {
                Some(r) => r,
                None => {
                    return Ok(ToolOut::Text(pretty(&json!({
                        "ok": false,
                        "reason": reason,
                        "candidates": candidates
                    }))));
                }
            };
            let rid = s(&row, "id");
            // 经宿主门面真跳转：TS nav.searchRow → goSearchRow ——**与检索条点行同一条链**（家族→档位/视图→行。
            // 1.8s 短超时：本体退后台/挂起时立刻显形，绝不挂等 60s。
            let res = ctx.h.call_timeout("nav", "searchRow", json!([{ "routeId": rid }]), 1800);
            let out = match res {
                Ok(v) => {
                    let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true);
                    let note = s(&v, "note");
                    json!({
                        "ok": ok,
                        "target": s(&row, "title"),
                        "breadcrumb": v.get("breadcrumb").cloned().unwrap_or(json!([])),
                        "landedOn": s(&v, "landedOn"),
                        "reason": if ok && !note.is_empty() { format!("跳到了分节根：{note}") } else if ok { String::new() } else { "宿主拒绝执行（权限/未知行）".to_string() },
                    })
                }
                Err(e) => {
                    json!({
                        "ok": false,
                        "target": s(&row, "title"),
                        "reason": format!("已定位到「{}」（{}）检索行，但本体界面未应答（应用可能退到后台或已退出）——请回到 OneTHU 前台再说一次：{}", s(&row, "title"), rid, e)
                    })
                }
            };
            out
        }
        "ui_route_table" => {
            let table = routes_assets();
            if table.is_empty() {
                return Err("路由表不可用（assets/routes.json 缺失；改动 searchRows 后请重跑 tools/gen-oh-routes.mjs）。".into());
            }
            json!({
                "rows": table
                    .iter()
                    .map(|r| json!({
                        "id": s(r, "id"),
                        "title": s(r, "title"),
                        "kind": s(r, "kind"),
                        "tier": s(r, "tier"),
                        "view": s(r, "view"),
                        "tab": s(r, "tab"),
                        "section": s(r, "section")
                    }))
                    .collect::<Vec<_>>(),
                "note": "用 ui_navigate {routeId} 直达；行 id 稳定，但若 UI 改版仍以 ui_route_table 的最新清单为准"
            })
        }
        "open_page" => {
            let q = s(args, "query");
            let q = q.trim().to_string();
            if q.is_empty() {
                return Err("query 不能为空".into());
            }
            let want_kind = s(args, "kind").trim().to_string();
            let hits = arr_of(&host(ctx, "nav", "searchAtoms", json!([q, 12]))?);
            let candidates: Vec<Value> = hits
                .into_iter()
                .filter(|h| want_kind.is_empty() || s(h, "kind") == want_kind)
                .collect();
            // 选最像的那个：同名 > 以名字开头 > 包含名字 > 第一条
            let pick = candidates
                .iter()
                .find(|h| s(h, "title") == q)
                .or_else(|| candidates.iter().find(|h| s(h, "title").starts_with(&q)))
                .or_else(|| candidates.iter().find(|h| s(h, "title").contains(&q)))
                .or_else(|| candidates.first())
                .cloned();
            let picked = match pick {
                Some(p) => p,
                None => {
                    // 本机缓存没有 → 兜底：查一次在线服务目录（仅当没限定别的 kind）。
                    // 目录命中的结果由宿主写回原子缓存，此后本地检索即可命中。
                    if let Some(out) = try_open_service(ctx, &q, &want_kind)? {
                        return Ok(ToolOut::Text(pretty(&out)));
                    }
                    return Ok(ToolOut::Text(pretty(&json!({
                        "opened": false,
                        "reason": format!("本机没有检索到「{q}」，在线服务目录里也没有相近的服务名。若用户给的是简称，可问一句完整叫法；其它入口需要他在对应页面打开过一次才能一句话直达"),
                        "candidates": []
                    }))));
                }
            };
            let refs = json!([{ "kind": s(&picked, "kind"), "key": s(&picked, "key") }]);
            let opened = host(ctx, "nav", "openAtom", refs)?;
            let opened = opened.as_bool().unwrap_or(false);
            let others: Vec<Value> = candidates
                .iter()
                .filter(|h| s(h, "key") != s(&picked, "key"))
                .take(5)
                .map(|h| json!({ "kind": s(h, "kind"), "title": s(h, "title"), "sub": s(h, "sub"), "group": s(h, "group") }))
                .collect();
            json!({
                "opened": opened,
                "title": s(&picked, "title"),
                "kind": s(&picked, "kind"),
                "group": s(&picked, "group"),
                "candidates": others,
                "note": if opened { "已在应用内打开" } else { "该条目已失效（对应的收藏或数据可能已被删除）" }
            })
        }
        "query_usage" => {
            let limit = args.get("limit").and_then(|x| x.as_i64()).unwrap_or(10);
            let limit = limit.clamp(1, 30);
            host(ctx, "nav", "usage", json!([limit]))?
        }
        "notify" => {
            host(ctx, "ui", "toast", json!([s(args, "text")]))?;
            json!({ "sent": true })
        }
        other => return Err(format!("未知工具：{other}")),
    };
    Ok(ToolOut::Text(pretty(&out)))
}

/// 解析编译期路由资产（routes.json：tools/gen-oh-routes.mjs 由 searchRows 唯一真源生成）。
/// 每次现取一次：文件小；解析失败返回空表，调用方能给出明确的用户可读错误。
fn routes_assets() -> Vec<Value> {
    serde_json::from_str::<Value>(ROUTES_JSON)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// 本地路由表匹配（webview 不在场也能先**匹配出结论**）：
///   routeId 非空 → 精确 id；
///   query → 标题包含（去空格；中文无大小写）优先 → 别名等值/包含；
/// 返回 (命中行, 候选, 不可行原因)。命中唯一才算 hit，多义如实回候选不硬猜。
fn match_route(table: &[Value], route_id: &str, q: &str) -> (Option<Value>, Vec<Value>, String) {
    if !route_id.is_empty() {
        if let Some(v) = table.iter().find(|r| s(r, "id") == route_id) {
            return (Some(v.clone()), Vec::new(), String::new());
        }
        return (None, Vec::new(), format!("没有 id 为「{route_id}」的检索行；可先调 ui_route_table 查看全部行 id"));
    }
    let m = q.to_lowercase().replace(' ', "");
    let mut hits: Vec<Value> = table
        .iter()
        .filter(|r| s(r, "title").to_lowercase().replace(' ', "").contains(&m))
        .cloned()
        .collect();
    if hits.is_empty() {
        hits = table
            .iter()
            .filter(|r| {
                let als = r.get("aliases").and_then(|x| x.as_array()).cloned().unwrap_or_default();
                als.iter().any(|a| {
                    let ax = a.as_str().unwrap_or("").to_lowercase().replace(' ', "");
                    ax == m || ax.contains(&m)
                })
            })
            .cloned()
            .collect();
    }
    if hits.is_empty() {
        (None, Vec::new(), format!("路由表里没有「{q}」可以命中的检索行；可调 ui_route_table 看全部，或换个说法再试"))
    } else if hits.len() == 1 {
        (hits.first().cloned(), Vec::new(), String::new())
    } else {
        let candidates = hits
            .iter()
            .take(6)
            .map(|r| json!({ "id": s(r, "id"), "title": s(r, "title") }))
            .collect();
        (None, candidates, format!("「{q}」有 {} 个可能的检索行，请让用户选一个后再用 routeId 精确直达", hits.len()))
    }
}

/// 本地原子缓存未命中时的服务目录兜底：
/// 走宿主 `services.search`（一次校园请求，容忍口语简称）→ 分数够高才 `services.open`
/// （应用内官方页，与用户点在线服务同一条链路）。
///
/// 三档处置（对应返回里的 note，模型据此回话）：
///   · score ≥ 40：有把握 → 直接打开；
///   · 20 ~ 39：只是「像」（如 亲友预约 ↔ 亲友入校报备）→ **不打开**，把候选交回模型，
///     让用户确认叫法后再用正式名调一次（正式名会命中 100 分）；
///   · 查询失败（未登录 / 平台不支持 / 接口不可用）或目录里确实没有 → Ok(None)，
///     只有当查询**成功但为空**时才回落成「没有相近名称」，失败要说失败的原因。
fn try_open_service(ctx: &mut Ctx, query: &str, want_kind: &str) -> Result<Option<Value>, String> {
    if !want_kind.is_empty() && want_kind != "thos-service" {
        return Ok(None);
    }
    let hits = match host(ctx, "services", "search", json!([query, 5])) {
        Ok(v) => arr_of(&v),
        Err(e) => {
            // 会话失效/接口不可用：如实说明，别让模型以为「学校没这个服务」
            // （失败原因随工具结果回给模型，dock 里用户也看得到，无需另找日志通道）
            return Ok(Some(json!({
                "opened": false,
                "reason": format!("本机没有缓存「{query}」，查在线服务目录也没成功（{e}）。可以稍后再试一次，或让用户先在应用里打开一次该服务"),
                "candidates": []
            })));
        }
    };
    let first = match hits.first() {
        Some(f) => f.clone(),
        None => return Ok(None),
    };
    let score = first.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let title = s(&first, "name");
    let others: Vec<Value> = hits
        .iter()
        .skip(1)
        .map(|h| {
            json!({
                "kind": "thos-service",
                "title": s(h, "name"),
                "sub": s(h, "department"),
                "group": "在线服务"
            })
        })
        .collect();
    let also = hits
        .iter()
        .map(|h| {
            json!({
                "name": s(h, "name"),
                "department": s(h, "department"),
                "score": h.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0)
            })
        })
        .collect::<Vec<_>>();
    // 分数不够：不动手打开，交回候选让用户点头
    if score < 40.0 {
        return Ok(Some(json!({
            "opened": false,
            "reason": format!("本机没有缓存「{query}」；服务目录里有名字相近的服务，但把握不足，**没有**替你打开：请先在回话里列出候选让用户确认叫法，再用他确认的正式名调一次 open_page"),
            "title": title,
            "kind": "thos-service",
            "group": "在线服务",
            "candidates": others,
            "similar": also
        })));
    }
    let opened = host(
        ctx,
        "services",
        "open",
        json!([{ "id": s(&first, "id"), "name": title, "url": s(&first, "url") }]),
    )?;
    let opened = opened.as_bool().unwrap_or(false);
    Ok(Some(json!({
        "opened": opened,
        "title": title,
        "kind": "thos-service",
        "group": "在线服务",
        "candidates": others,
        "note": "结果来自在线服务目录（本机此前没缓存过这个服务）；回话用目录里的正式名。若用户说的其实是别的同名事物，可用 candidates 或再调一次本工具带上 kind"
    })))
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

/** epoch ms → 北京时间 "2026-09-09 23:11"（邮件工具输出用） */
fn fmt_mail_ms(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::FixedOffset::east_opt(8 * 3600)
        .and_then(|tz| chrono::DateTime::from_timestamp(ms.div_euclid(1000), 0).map(|d| d.with_timezone(&tz)))
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".into())
}

/** 云盘资料库按名/ID 定位（精确名 → 前缀唯一 → ID） */
fn find_cloud_repo<'a>(repos: &'a [Value], want: &str) -> Result<&'a Value, String> {
    let exact: Vec<&Value> = repos.iter().filter(|r| s(r, "name") == want).collect();
    if let [r] = exact.as_slice() {
        return Ok(r);
    }
    let prefix: Vec<&Value> = repos.iter().filter(|r| s(r, "name").contains(want)).collect();
    if let [r] = prefix.as_slice() {
        return Ok(r);
    }
    if let Some(r) = repos.iter().find(|r| s(r, "id") == want) {
        return Ok(r);
    }
    let names: Vec<String> = repos.iter().map(|r| s(r, "name")).collect();
    Err(format!("云盘里找不到资料库「{want}」，可用：{}", names.join("、")))
}

/** 字节数 → 人类可读（云盘工具输出用） */
fn fmt_size(b: i64) -> String {
    if b < 0 {
        return "-".into();
    }
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}


/* ══════════ THUbook（thubook.help）抓取与正文提取 ══════════ */

fn thubook_fetch(url: &str) -> Result<String, String> {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(25))
        .build()
        .get(url)
        .set("User-Agent", "OneTHU-Harness/0.1 (THUbook reader)")
        .call()
        .map_err(|e| format!("THUbook 请求失败：{e}"))?
        .into_string()
        .map_err(|e| format!("THUbook 读取失败：{e}"))
}

/// 预渲染 VuePress 页面 → (标题, 纯文本正文)：main 优先，去 script/style/tag，
/// 解常见实体，压缩空白。不含首尾导航杂质。
fn thubook_extract(html: &str) -> (String, String) {
    let title = html
        .find("<title>")
        .and_then(|a| html[a + 7..].find("</title>").map(|n| html[a + 7..a + 7 + n].to_string()))
        .unwrap_or_default();
    let seg = match (html.find("<main"), html.find("</main>")) {
        (Some(a), Some(b)) if b > a => &html[a..b],
        _ => match (html.find("<body"), html.find("</body>")) {
            (Some(a), Some(b)) if b > a => &html[a..b],
            _ => html,
        },
    };
    // 迭代剔除整块标签（script/style/nav/header/footer）
    fn strip_blocks(mut src: &str, tag: &str) -> String {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let mut out = String::with_capacity(src.len());
        loop {
            match src.find(&open) {
                Some(a) => match src[a..].find(&close) {
                    Some(n) => {
                        out.push_str(&src[..a]);
                        src = &src[a + n + close.len()..];
                    }
                    None => {
                        out.push_str(src);
                        break;
                    }
                },
                None => {
                    out.push_str(src);
                    break;
                }
            }
        }
        out
    }
    let mut owned = seg.to_string();
    for tag in ["script", "style", "nav", "header", "footer"] {
        owned = strip_blocks(&owned, tag);
    }
    let rest = owned.as_str();
    // 去 tag
    let mut out = String::with_capacity(rest.len());
    let mut in_tag = false;
    for c in rest.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let t = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    let mut text = String::with_capacity(t.len());
    let mut prev_space = false;
    for c in t.chars() {
        if c.is_whitespace() {
            if !prev_space {
                text.push(' ');
            }
            prev_space = true;
        } else {
            text.push(c);
            prev_space = false;
        }
    }
    (title, text.trim().to_string())
}
