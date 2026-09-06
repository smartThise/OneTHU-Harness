//! 插件配置（R3）：每次 run 从宿主 settings.get() 取最新值——用户在管理页
//! 改完设置即刻生效，无需重装/重启插件。
//!
//! 另含中文相对日期解析（场景定制①）：模型说「明天下午」「下周三」，由本地
//! 时钟 + 日历确定性换算成接口要的 YYYY-MM-DD / dateChoice，不让 LLM 心算日期。

use chrono::{Datelike, Duration, Local, NaiveDate, Timelike, Weekday};

#[derive(Clone, Debug)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    /// 思考模式：off=不开；on=DeepSeek 系自动切 reasoner 变体；其他端尽力而为
    pub thinking: bool,
    /// 上下文预算（token 估算上限，超出裁剪历史）
    pub max_context: usize,
    /// SSE 流式输出
    pub stream: bool,
    /// 价格 $/1M tokens
    pub price_in: f64,
    pub price_out: f64,
    /// Token 预算（USD），到量自动停（R6）
    pub budget_usd: f64,
    /// 单次任务最大 agent 步数
    pub max_steps: u32,
}

impl Config {
    pub fn from_settings(map: &serde_json::Map<String, serde_json::Value>) -> Config {
        let get = |k: &str| -> String {
            map.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string()
        };
        let get_num = |k: &str, d: f64| -> f64 { get(k).parse::<f64>().unwrap_or(d) };
        let base = {
            let b = get("baseUrl");
            if b.is_empty() { "https://api.deepseek.com/v1".into() } else { b.trim_end_matches('/').to_string() }
        };
        let mut model = {
            let m = get("model");
            if m.is_empty() { "deepseek-chat".into() } else { m }
        };
        let thinking = matches!(get("thinking").to_lowercase().as_str(), "on" | "true" | "1" | "开");
        // 思考模式（DeepSeek 语义）：chat ↔ reasoner 变体互换
        if thinking && model == "deepseek-chat" {
            model = "deepseek-reasoner".into();
        } else if !thinking && model == "deepseek-reasoner" {
            model = "deepseek-chat".into();
        }
        Config {
            api_key: get("apiKey"),
            base_url: base,
            model,
            thinking,
            max_context: get_num("maxContext", 24000.0).max(4000.0) as usize,
            stream: !matches!(get("stream").to_lowercase().as_str(), "off" | "false" | "0" | "关"),
            price_in: get_num("priceIn", 0.27).max(0.0),
            price_out: get_num("priceOut", 1.10).max(0.0),
            budget_usd: get_num("budget", 2.0).max(0.0),
            max_steps: (get_num("maxSteps", 16.0).max(2.0).min(64.0)) as u32,
        }
    }
}

/// 粗略 token 估算（仅用于上下文裁剪；计费一律以 API 返回 usage 为准）：
/// CJK 字符 ≈ 1 token/字，其余 ≈ 4 字符/token。
pub fn est_tokens(s: &str) -> usize {
    let total = s.chars().count();
    let cjk = s.chars().filter(|c| (*c as u32) >= 0x2E80).count();
    cjk + (total - cjk) / 4 + 1
}

pub fn today() -> NaiveDate {
    Local::now().date_naive()
}

/// 「今天 8:30」之类的人类时间快照（注入系统提示词）
pub fn now_line() -> String {
    let d = Local::now();
    let wd = cn_weekday(d.weekday());
    format!("{}（{}）{:02}:{:02}", d.format("%Y-%m-%d"), wd, d.hour(), d.minute())
}

pub fn cn_weekday(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "周一",
        Weekday::Tue => "周二",
        Weekday::Wed => "周三",
        Weekday::Thu => "周四",
        Weekday::Fri => "周五",
        Weekday::Sat => "周六",
        Weekday::Sun => "周日",
    }
}

/// 解析中文/标准日期表达 → NaiveDate。支持：
/// 今天/明天/后天/大后天、(下|下下|上)周X/星期X/礼拜X、M月D日、YYYY-MM-DD、YYYY/M/D、MM-DD
pub fn resolve_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    let t = today();
    let norm = s.replace(['，', '。', ' '], "");
    match norm.as_str() {
        "今天" | "今日" | "当天" => return Some(t),
        "明天" | "明日" => return Some(t + Duration::days(1)),
        "后天" => return Some(t + Duration::days(2)),
        "大后天" => return Some(t + Duration::days(3)),
        _ => {}
    }
    if let Some(d) = NaiveDate::parse_from_str(&norm, "%Y-%m-%d").ok() {
        return Some(d);
    }
    if let Some(d) = NaiveDate::parse_from_str(&norm, "%Y/%m/%d").ok() {
        return Some(d);
    }
    if let Ok(d) = NaiveDate::parse_from_str(&format!("{}-{}", t.year(), norm), "%Y-%m-%d") {
        return Some(d); // "MM-DD" 按当年
    }
    // M月D日
    if let Some(pos) = norm.find('月') {
        let m: u32 = norm[..pos].parse().ok()?;
        let rest = &norm[pos + 3..]; // '月' 占 3 字节
        let d_end = rest.find('日').unwrap_or(rest.len());
        let d: u32 = rest[..d_end].parse().ok()?;
        for y in [t.year(), t.year() + 1] {
            if let Some(cand) = NaiveDate::from_ymd_opt(y, m, d) {
                if cand >= t {
                    return Some(cand);
                }
            }
        }
        return NaiveDate::from_ymd_opt(t.year(), m, d);
    }
    // 周X / 星期X / 礼拜X（含 下周X / 下下周X / 上周X / 本周X）
    let week_prefix = ["下下周", "下周", "上周", "本周", "这周", "本"];
    let mut rest = norm.as_str();
    let mut offset_weeks: i64 = 0;
    for p in week_prefix {
        if let Some(r) = rest.strip_prefix(p) {
            offset_weeks = match p {
                "下下周" => 2,
                "下周" => 1,
                "上周" => -1,
                _ => 0,
            };
            rest = r;
            break;
        }
    }
    for pre in ["星期", "礼拜", "周"] {
        if let Some(r) = rest.strip_prefix(pre) {
            let wd = match r {
                "一" | "1" => Some(Weekday::Mon),
                "二" | "2" => Some(Weekday::Tue),
                "三" | "3" => Some(Weekday::Wed),
                "四" | "4" => Some(Weekday::Thu),
                "五" | "5" => Some(Weekday::Fri),
                "六" | "6" => Some(Weekday::Sat),
                "日" | "天" | "7" => Some(Weekday::Sun),
                _ => None,
            };
            if let Some(wd) = wd {
                let monday = t - Duration::days(t.weekday().num_days_from_monday() as i64) + Duration::weeks(offset_weeks);
                let target = monday + Duration::days(wd.num_days_from_monday() as i64);
                // 无周前缀的裸「周X」：默认指未来最近的一个 X（含今天）
                if offset_weeks == 0 && target < t {
                    return Some(target + Duration::days(7));
                }
                return Some(target);
            }
        }
    }
    None
}

/// 图书馆座位接口的 dateChoice 只认 0=今天 / 1=明天
pub fn date_choice(s: &str) -> Result<u8, String> {
    match resolve_date(s) {
        Some(d) if d == today() => Ok(0),
        Some(d) if d == today() + Duration::days(1) => Ok(1),
        Some(_) => Err("图书馆座位只支持今天(0)或明天(1)，更远的日期请改天再来查。".into()),
        None => Err(format!("无法理解日期「{s}」，请用 今天/明天 或 YYYY-MM-DD。")),
    }
}

/// 解析「HH:MM」→ (时, 分)，容忍「8点半」「15:00」「下午3点」等常见表达
pub fn resolve_hhmm(s: &str) -> Option<(u32, u32)> {
    let s = s.trim().replace(['，', '。', ' '], "");
    if let Some(pos) = s.find(':') {
        let h: u32 = s[..pos].parse().ok()?;
        let m: u32 = s[pos + 1..].chars().take(2).collect::<String>().parse().unwrap_or(0);
        if h < 24 && m < 60 {
            return Some((h, m));
        }
        return None;
    }
    // 「8点」「8点半」「8点15」「下午2点半」——先剥上下午前缀再解析
    let is_pm = s.contains("下午") || s.contains("晚上");
    let is_am = s.contains("上午");
    let core: String = s.replace(['上', '午', '下', '晚', '中'], "");
    if let Some(pos) = core.find('点') {
        let mut h: u32 = core[..pos].parse().ok()?;
        let rest = &core[pos + 3..];
        let m = if rest.starts_with('半') {
            30
        } else {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() { 0 } else { digits.parse().ok()? }
        };
        if is_pm && h < 12 {
            h += 12;
        }
        if is_am && h == 12 {
            h = 0;
        }
        if h < 24 && m < 60 {
            return Some((h, m));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn est_tokens_basic() {
        assert!(est_tokens("你好世界") >= 4);
        assert!(est_tokens("abcdef") >= 2);
    }
}
