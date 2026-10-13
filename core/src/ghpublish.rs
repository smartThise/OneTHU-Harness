//! 插件发布出口（2026-10-13 用户点名）：本地插件目录 → GitHub 仓库 → 插件市场名单 PR。
//!
//! 原则（用户原话落实）：
//! - 「要检查，如果他自己已经创建了当然不要代劳了」——pub_check 只读探测远端仓库与
//!   本地插件状态；远端已存在就**只推不建**；市场里 id 已收录就**不再提 PR**，只回报链接。
//! - 账户依赖：`gh` CLI 登录态（`gh auth status` / `gh auth token`）——不想让用户在
//!   本插件里手填 token（凭据管理是 App 的地盘）；没 gh 就给出清晰的一步指引。
//! - 全程两段式确认（tools.rs 里 confirm: true）：推送/建仓/开 PR 都是一次对外 write。

use serde_json::{json, Value};

pub const MARKET_OWNER: &str = "smartThise";
pub const MARKET_REPO: &str = "OneTHU-Market";
pub const MARKET_ENTRY: &str = "registry.json";

type R<T> = Result<T, String>;

/* ── 进程小执行器（mcp.rs 的 Command 用法同款） ── */

fn run(dir: Option<&std::path::Path>, program: &str, args: &[&str]) -> R<String> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null());
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    match cmd.output() {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if o.status.success() {
                Ok(out)
            } else {
                let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                if err.is_empty() {
                    Err(format!("{program} 退出码 {:?}", o.status.code()))
                } else {
                    Err(err.lines().map(|l| l.trim()).collect::<Vec<_>>().join("；"))
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!("本机没有 {program} 命令")),
        Err(e) => Err(e.to_string()),
    }
}

fn gh_token() -> R<String> {
    let t = run(None, "gh", &["auth", "token"])?;
    if t.is_empty() {
        return Err("gh 未登录或拿不到 token（gh auth status 看状态）".into());
    }
    Ok(t)
}

fn gh_login() -> R<String> {
    let l = run(None, "gh", &["api", "/user", "-q", ".login"])?;
    if l.is_empty() {
        return Err("gh 已装但拿不到用户名（gh auth status 检查）".into());
    }
    Ok(l)
}

/* ── GitHub REST（市側专用小客户端；插件/仓库操作走 git CLI，不烧 API quota） ── */

fn http_gh(method: &str, url_path: &str, token: &str, body: Option<&Value>) -> R<Value> {
    let url = if url_path.starts_with("http") {
        url_path.to_string()
    } else {
        format!("https://api.github.com{url_path}")
    };
    let req = match method {
        "GET" => ureq::get(&url),
        "POST" => ureq::post(&url),
        "PUT" => ureq::put(&url),
        _ => return Err(format!("不支持的方法 {method}")),
    };
    let resp = req
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28");
    let raw_resp = match body {
        Some(b) => resp.send_string(&b.to_string()),
        None => resp.call(),
    };
    let resp = raw_resp.map_err(|e| format!("GitHub 请求失败：{e}"))?;
    let status = resp.status();
    let text = resp.into_string().unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::String(text.clone()));
    if (200..300).contains(&status) || status == 202 {
        Ok(v)
    } else {
        Err(format!("GitHub {status}：{text}"))
    }
}

fn b64(s: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((s.len() + 2) / 3 * 4);
    for c in s.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/* ── 探测（pub_check 只读） ── */

pub struct Probe {
    pub gh: bool,
    pub account: String,
    pub git_ok: bool,
    pub has_entry: bool,
    pub entry_name: String,
    pub repo_exists: Option<bool>,
    pub version: String,
    pub plugin_id: String,
}

pub fn probe(dir: &str, repo: &str) -> Probe {
    let gh = run(None, "gh", &["auth", "status"]).is_ok();
    let account = if let Ok(l) = gh_login() { l } else { String::new() };
    let git_ok = run(None, "git", &["--version"]).is_ok();
    let d = std::path::Path::new(dir);
    let entry_name = ["plugin.js", "index.js", "main.js"]
        .iter()
        .find(|f| d.join(f).exists())
        .map(|f| f.to_string())
        .unwrap_or_default();
    let manifest_id = std::fs::read_to_string(d.join(&entry_name))
        .map(|src| {
            src.split("\n")
                .find(|l| l.contains("id:") && l.contains("onethu."))
                .map(|l| {
                    l.split(":")
                        .nth(1)
                        .unwrap_or("")
                        .trim()
                        .trim_matches(',')
                        .trim_matches('"')
                        .to_string()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let version = std::fs::read_to_string(d.join(&entry_name))
        .map(|src| {
            src.split("\n")
                .find(|l| l.contains("version:"))
                .map(|l| {
                    l.split(':')
                        .nth(1)
                        .unwrap_or("")
                        .trim()
                        .trim_matches(',')
                        .trim_matches('"')
                        .to_string()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let repo_exists = if repo.split('/').count() == 2 && !repo.is_empty() {
        match run(None, "git", &["ls-remote", &format!("https://github.com/{repo}.git"), "HEAD"]) {
            Ok(_) => Some(true),
            Err(e) if e.contains("Repository not found") => Some(false),
            Err(_) => None, // 网络/私有化不确定：不妄断
        }
    } else {
        None
    };
    Probe { gh, account, git_ok, has_entry: !entry_name.is_empty(), entry_name, repo_exists, version, plugin_id: manifest_id }
}

/* ── 本地插件 → GitHub（远端已存在只推不建；不存在需 gh 代建） ── */

const GH_NAME: &str = "OneTHU Harness";
const GH_EMAIL: &str = "harness@onethu.local";

pub fn publish_to_github(dir: &str, repo: &str, summary_pub: bool) -> R<Value> {
    let d = std::path::Path::new(dir);
    if !d.is_dir() {
        return Err(format!("本地目录不存在：{dir}"));
    }
    let p = probe(dir, repo);
    if !p.has_entry {
        return Err("目录里没找到 plugin.js / index.js / main.js（不是插件仓库根目录）".into());
    }
    if !summary_pub {
        return Err("summary=false 时不执行写操作（摘要确认流程用）".into());
    }
    run(Some(d), "git", &["rev-parse", "--is-inside-work-tree"])
        .err()
        .map(|_| run(Some(d), "git", &["init", "-b", "main"]))
        .transpose()?;
    run(Some(d), "git", &["add", "-A"])?;
    run(Some(d), "git", &["-c", &format!("user.name={GH_NAME}"), "-c", &format!("user.email={GH_EMAIL}"), "commit", "-m", "发布：OneTHU 插件源码与文档"])?;
    let url = format!("https://github.com/{repo}");
    match p.repo_exists {
        Some(false) => {
            if !p.gh {
                return Err(format!(
                    "远端还没有 {repo}，本机也没装 gh（GitHub CLI）。两条路：① github.com/new 手建仓库后重跑本工具；② 终端里 `gh auth login` 后重跑，我来代建。"
                ));
            }
            run(Some(d), "gh", &["repo", "create", repo, "--source", ".", "--remote", "origin", "--push", "--public"])?;
        }
        Some(true) => {
            run(Some(d), "git", &["remote", "remove", "origin"]).ok();
            run(Some(d), "git", &["remote", "add", "origin", &url])?;
            run(Some(d), "git", &["push", "-u", "origin", "HEAD:main"])?;
        }
        None => return Err("远端仓库探测失败（网络）；确认能访问 github.com 后重试。".into()),
    }
    Ok(json!({ "url": url, "branch": "main", "owner": gh_login().unwrap_or_default() }))
}

/* ── 市场名单 PR：fork 官方仓库 → upsert 条目 → contents API 单文件 commit → 开 PR ── */

fn base64_of(s: &str) -> String {
    b64(s.as_bytes())
}

pub fn market_pr(entry: &Value) -> R<Value> {
    let token = gh_token()?;
    let login = gh_login()?;
    let id = entry
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        return Err("条目缺 id（形如 onethu.your-plugin）".into());
    }
    // ① 已存在即不代劳：拉官方名单先看一眼
    let raw = ureq::get(&format!(
        "https://raw.githubusercontent.com/{MARKET_OWNER}/{MARKET_REPO}/main/{MARKET_ENTRY}"
    ))
    .call()
    .map_err(|e| format!("拉取市场名单失败：{e}"))?
    .into_string()
    .map_err(|e| e.to_string())?;
    let mut list: Vec<Value> = serde_json::from_str(&raw).unwrap_or_default();
    if list.iter().any(|e| e.get("id").and_then(|v| v.as_str()) == Some(id.as_str())) {
        return Err(format!("市场名单里已有 id={id}：收录时不重复提 PR；数据要改的话请直接更新你插件仓库的 manifest（市场条目以仓库为准）。"));
    }
    list.push(entry.clone());
    let new_registry = serde_json::to_string_pretty(&list).unwrap_or_else(|_| raw);

    // ② fork（幂等：已 fork 返回现有）
    let fork = http_gh("POST", &format!("/repos/{MARKET_OWNER}/{MARKET_REPO}/forks"), &token, Some(&json!({})))?;
    let fork_full = fork
        .get("full_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // ③ 新分支 + 单文件提交（contents API）
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 60).unwrap_or(0);
    let branch = format!("market-{id}-{}", ts);
    let base = http_gh("GET", &format!("/repos/{fork_full}/git/ref/heads/main"), &token, None)?;
    let base_sha = base.get("object").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let old = http_gh("GET", &format!("/repos/{fork_full}/contents/{MARKET_ENTRY}?ref=main"), &token, None)?;
    let old_sha = old.get("sha").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let _unused = base_sha;
    let put_body = json!({
        "message": format!("[插件收录] {} {}", entry.get("name").and_then(|v| v.as_str()).unwrap_or(id.as_str()), entry.get("version").and_then(|v| v.as_str()).unwrap_or("")),
        "content": base64_of(&new_registry),
        "branch": branch,
        "sha": old_sha,
    });
    http_gh("PUT", &format!("/repos/{fork_full}/contents/{MARKET_ENTRY}"), &token, Some(&put_body))?;

    // ④ 已有同名 open PR 就不重复开，返回现链接
    let existing = http_gh("GET", &format!("/repos/{MARKET_OWNER}/{MARKET_REPO}/pulls?head={login}:{branch}&state=open"), &token, None)?;
    if let Some(arr) = existing.as_array() {
        if let Some(pr) = arr.first() {
            let url = pr.get("html_url").and_then(|v| v.as_str()).unwrap_or("");
            return Ok(json!({ "pr": url, "existing": true }));
        }
    }
    let pr_body = json!({
        "title": format!("[插件收录] {}", entry.get("name").and_then(|v| v.as_str()).unwrap_or(id.as_str())),
        "head": format!("{login}:{branch}"),
        "base": "main",
        "body": format!(
            "## 插件收录申请\n\n- **id**：{id}\n- **名字**：{}\n- **版本**：{}\n- **仓库**：{}\n- **来源**：OneTHU Harness 一键收录（人工审查请继续清单标准：入口可拉取可解析、权限与功能匹配、无混淆/无动态拼装/无凭据收集）",
            entry.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            entry.get("version").and_then(|v| v.as_str()).unwrap_or(""),
            entry.get("repo").and_then(|v| v.as_str()).unwrap_or("")
        )
    });
    let pr = http_gh("POST", &format!("/repos/{MARKET_OWNER}/{MARKET_REPO}/pulls"), &token, Some(&pr_body))?;
    Ok(json!({ "pr": pr.get("html_url").and_then(|v| v.as_str()).unwrap_or(""), "existing": false, "fork": fork_full }))
}
