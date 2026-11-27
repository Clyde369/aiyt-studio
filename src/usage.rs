//! What each AI provider has used and has left, plus "how many more shorts" estimates.
//! Claude: its own `/usage` report. Codex: rate-limit snapshots it writes to ~/.codex/sessions.
//! Everyone: tokens the app itself recorded per call (usage.jsonl).

use crate::llm::{command, run};
use crate::settings::{config_dir, write_atomic, Settings};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

fn now() -> u64 {
    crate::youtube::now_secs()
}

/// The short being generated right now; every AI call made meanwhile is counted towards it.
pub static JOB: Mutex<String> = Mutex::new(String::new());

pub fn set_job(id: &str) {
    *JOB.lock().unwrap() = id.to_string();
}

/// Append one AI call to the local usage log.
pub fn record(provider: &str, model: &str, task: &str, input: u64, output: u64) {
    let job = JOB.lock().unwrap().clone();
    let line = json!({"t": now(), "provider": provider, "model": model, "task": task, "in": input, "out": output, "job": job});
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(config_dir().join("usage.jsonl")) {
        let _ = writeln!(f, "{line}");
    }
}

fn log_since(secs: u64) -> Vec<Value> {
    let since = now().saturating_sub(secs);
    fs::read_to_string(config_dir().join("usage.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["t"].as_u64().unwrap_or(0) >= since)
        .collect()
}

/// Claude's own usage report, e.g. "Current week (all models): 19% used · resets Nov 27, 5pm".
/// Cached for 2 minutes because it starts the Claude app.
static CLAUDE_CACHE: Mutex<Option<(Instant, Vec<Value>)>> = Mutex::new(None);

pub fn claude_windows(fresh: bool) -> Vec<Value> {
    if let Some((at, v)) = CLAUDE_CACHE.lock().unwrap().as_ref() {
        if !fresh && at.elapsed() < Duration::from_secs(120) {
            return v.clone();
        }
    }
    let exe = if crate::on_path("claude.exe") || !cfg!(windows) { "claude" } else { "claude.cmd" };
    let mut c = command(exe);
    c.args(["-p", "/usage", "--output-format", "text", "--setting-sources", "", "--strict-mcp-config"]);
    let text = run(&mut c, "claude").map(|o| String::from_utf8_lossy(&o).to_string()).unwrap_or_default();
    let windows: Vec<Value> = text.lines().filter_map(parse_usage_line).collect();
    *CLAUDE_CACHE.lock().unwrap() = Some((Instant::now(), windows.clone()));
    windows
}

fn parse_usage_line(l: &str) -> Option<Value> {
    let (label, rest) = l.split_once(':')?;
    let pct: f64 = rest.split('%').next()?.trim().parse().ok()?;
    if !rest.contains("used") {
        return None;
    }
    let resets = rest.split_once("resets").map(|r| r.1.trim().to_string()).unwrap_or_default();
    Some(json!({"label": label.trim(), "used": pct, "resets": resets}))
}

fn codex_dir() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".codex").join("sessions"))
}

/// Latest Codex rate-limit snapshot: 5-hour and weekly windows.
pub fn codex_windows() -> Vec<Value> {
    let Some(root) = codex_dir() else { return vec![] };
    // sessions/YYYY/MM/DD/*.jsonl, newest first
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = vec![];
    let mut stack = vec![(root, 0)];
    while let Some((dir, depth)) = stack.pop() {
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() && depth < 3 {
                stack.push((p, depth + 1));
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                files.push((e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), p));
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, f) in files.iter().take(30) {
        let text = fs::read_to_string(f).unwrap_or_default();
        for line in text.lines().rev() {
            if !line.contains("\"rate_limits\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let rl = find_key(&v, "rate_limits");
            if rl["primary"].is_object() {
                // A window whose reset time has passed is back to 0% (the snapshot is from before the reset).
                let w = |x: &Value, label: &str| {
                    let expired = x["resets_at"].as_u64().is_some_and(|t| t < now());
                    json!({"label": label, "used": if expired { json!(0.0) } else { x["used_percent"].clone() }, "resets_at": if expired { Value::Null } else { x["resets_at"].clone() }})
                };
                let mut out = vec![w(&rl["primary"], "5-hour window")];
                if rl["secondary"].is_object() {
                    out.push(w(&rl["secondary"], "Weekly"));
                }
                return out;
            }
        }
    }
    vec![]
}

fn find_key(v: &Value, key: &str) -> Value {
    match v {
        Value::Object(m) => m.get(key).cloned().unwrap_or_else(|| m.values().map(|x| find_key(x, key)).find(|x| !x.is_null()).unwrap_or(Value::Null)),
        Value::Array(a) => a.iter().map(|x| find_key(x, key)).find(|x| !x.is_null()).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// Highest "% used" across a provider's windows (the one that will stop you first).
pub fn used_pct(provider: &str, fresh: bool) -> Option<f64> {
    let w = match provider {
        "claude" => claude_windows(fresh),
        "codex" => codex_windows(),
        _ => return None,
    };
    w.iter().filter_map(|x| x["used"].as_f64()).reduce(f64::max)
}

fn state_path() -> PathBuf {
    config_dir().join("usage_state.json")
}

/// After a batch: remember roughly how many % of the limit one short costs (smoothed).
pub fn learn(provider: &str, before: Option<f64>, after: Option<f64>, shorts: usize) {
    let (Some(b), Some(a)) = (before, after) else { return };
    if shorts == 0 || a < b {
        return; // window reset mid-batch; nothing to learn
    }
    let per = ((a - b) / shorts as f64).max(0.1);
    let mut st: HashMap<String, f64> = fs::read_to_string(state_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let old = st.get(provider).copied();
    st.insert(provider.into(), old.map(|o| o * 0.6 + per * 0.4).unwrap_or(per));
    let _ = write_atomic(&state_path(), &serde_json::to_string(&st).unwrap_or_default());
}

/// Everything the sidebar and the usage window show.
pub fn summary(s: &Settings, fresh: bool) -> Value {
    let per_short: HashMap<String, f64> = fs::read_to_string(state_path()).ok().and_then(|x| serde_json::from_str(&x).ok()).unwrap_or_default();
    let week = log_since(7 * 86400);
    let tokens = |p: &str, secs: u64| {
        let since = now().saturating_sub(secs);
        week.iter().filter(|v| v["provider"] == p && v["t"].as_u64().unwrap_or(0) >= since).fold((0u64, 0u64), |(c, t), v| (c + 1, t + v["in"].as_u64().unwrap_or(0) + v["out"].as_u64().unwrap_or(0)))
    };
    let in_use: Vec<String> = s.tasks_in_use();
    let mut out = vec![];
    for (p, name, tool) in [("claude", "Claude", "claude"), ("codex", "ChatGPT (Codex)", "codex"), ("gemini", "Gemini", ""), ("opencode", "OpenCode", "opencode"), ("ollama", "Ollama", "ollama")] {
        let installed = tool.is_empty() || crate::on_path(tool);
        let active = in_use.iter().any(|w| w == p);
        let windows = match p {
            "claude" if installed && active => claude_windows(fresh),
            "codex" if installed => codex_windows(),
            _ => vec![],
        };
        let used = windows.iter().filter_map(|w| w["used"].as_f64()).reduce(f64::max);
        let (calls_day, tok_day) = tokens(p, 86400);
        let (calls_week, tok_week) = tokens(p, 7 * 86400);
        let estimate = match (p, used, per_short.get(p)) {
            ("ollama", ..) => json!("Unlimited (runs on this PC)"),
            ("gemini", ..) if s.gemini_key.trim().is_empty() => json!("Add a Gemini key"),
            ("gemini", ..) => json!("Free keys have daily request limits; Google does not report what is left"),
            (_, Some(u), Some(per)) => json!(((100.0 - u) / per).floor().max(0.0) as u64),
            (_, Some(_), None) => json!("Make a few shorts to calibrate"),
            _ => json!("Not reported by this provider"),
        };
        out.push(json!({
            "id": p, "name": name, "installed": installed, "active": active, "windows": windows, "used": used,
            "calls_day": calls_day, "tokens_day": tok_day, "calls_week": calls_week, "tokens_week": tok_week,
            "per_short": per_short.get(p), "estimate": estimate, "avg_tokens": avg_tokens_per_short(p)
        }));
    }
    json!(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_usage_lines() {
        let v = parse_usage_line("Current week (all models): 19% used · resets Nov 27, 5pm (Africa/Johannesburg)").unwrap();
        assert_eq!(v["used"], 19.0);
        assert_eq!(v["label"], "Current week (all models)");
        assert!(v["resets"].as_str().unwrap().starts_with("Nov 27"));
        assert!(parse_usage_line("Last 24h · 649 requests · 19 sessions").is_none());
        assert!(parse_usage_line("  93%  Start fresh when you switch tasks").is_none());
        let nested = json!({"payload": {"info": {"rate_limits": {"primary": {"used_percent": 15.0}}}}});
        assert_eq!(find_key(&nested, "rate_limits")["primary"]["used_percent"], 15.0);
    }
}

/// Tokens and AI calls per short: job id -> (tokens, calls, "provider model" list).
pub fn job_tokens() -> HashMap<String, (u64, u64, Vec<String>)> {
    let mut m: HashMap<String, (u64, u64, Vec<String>)> = HashMap::new();
    for v in log_since(u64::MAX / 2) {
        let Some(job) = v["job"].as_str().filter(|j| !j.is_empty()) else { continue };
        let e = m.entry(job.to_string()).or_default();
        e.0 += v["in"].as_u64().unwrap_or(0) + v["out"].as_u64().unwrap_or(0);
        e.1 += 1;
        let who = format!("{} {}", v["provider"].as_str().unwrap_or(""), v["model"].as_str().unwrap_or("")).trim().to_string();
        if !e.2.contains(&who) {
            e.2.push(who);
        }
    }
    m
}

/// Average tokens one short costs with this provider (only shorts that used it).
fn avg_tokens_per_short(provider: &str) -> Option<u64> {
    let mut per_job: HashMap<String, u64> = HashMap::new();
    for v in log_since(30 * 86400) {
        if v["provider"] == provider {
            if let Some(job) = v["job"].as_str().filter(|j| !j.is_empty()) {
                *per_job.entry(job.into()).or_default() += v["in"].as_u64().unwrap_or(0) + v["out"].as_u64().unwrap_or(0);
            }
        }
    }
    let n = per_job.len() as u64;
    (n > 0).then(|| per_job.values().sum::<u64>() / n)
}
