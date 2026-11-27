//! Smart picks: tracks every recent upload's views, likes, comments (and retention when allowed), spots a short
//! that is blowing up, and has the AI propose a follow-up that is similar but not the same.

use crate::settings::{write_atomic, Settings};
use crate::{llm, youtube};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;

fn now() -> u64 {
    youtube::now_secs()
}

fn read(name: &str) -> Value {
    fs::read_to_string(crate::settings::channel_dir().join(name)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}))
}

/// video id -> [{t, views, likes, comments}], newest last.
pub fn snapshot(s: &Settings) -> Result<Value> {
    let a = youtube::analytics(s)?;
    let mut hist = read("video_stats.json");
    let t = now();
    for v in a["list"].as_array().into_iter().flatten() {
        let Some(id) = v["id"].as_str() else { continue };
        let entry = hist[id].as_array().cloned().unwrap_or_default();
        let mut entry: Vec<Value> = entry.into_iter().filter(|x| t.saturating_sub(x["t"].as_u64().unwrap_or(0)) < 14 * 86400).collect();
        entry.push(json!({"t": t, "views": v["views"], "likes": v["likes"], "comments": v["comments"]}));
        hist[id] = entry.into();
    }
    write_atomic(&crate::settings::channel_dir().join("video_stats.json"), &hist.to_string())?;
    Ok(a)
}

/// Views per hour over roughly the last `window` seconds, from the snapshots.
fn velocity(points: &[Value], window: u64) -> f64 {
    let Some(last) = points.last() else { return 0.0 };
    let t1 = last["t"].as_u64().unwrap_or(0);
    let first = points.iter().find(|p| t1.saturating_sub(p["t"].as_u64().unwrap_or(0)) <= window).unwrap_or(last);
    let hours = (t1.saturating_sub(first["t"].as_u64().unwrap_or(0))) as f64 / 3600.0;
    if hours < 0.4 {
        return 0.0;
    }
    (last["views"].as_u64().unwrap_or(0).saturating_sub(first["views"].as_u64().unwrap_or(0))) as f64 / hours
}

/// Shorts whose views per hour (last ~6 h) are at least 3× the channel's typical rate and clearly growing.
/// ponytail: simple ratio-vs-median heuristic; upgrade to per-age baselines once there is months of history.
pub fn blowing_up(list: &[Value]) -> Vec<(Value, f64, f64)> {
    let hist = read("video_stats.json");
    let rates: Vec<(Value, f64)> = list
        .iter()
        .filter_map(|v| {
            let pts = hist[v["id"].as_str()?].as_array()?.clone();
            Some((v.clone(), velocity(&pts, 6 * 3600)))
        })
        .collect();
    let mut sorted: Vec<f64> = rates.iter().map(|r| r.1).collect();
    sorted.sort_by(f64::total_cmp);
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0).max(1.0);
    rates.into_iter().filter(|(_, r)| *r >= 3.0 * median && *r >= 10.0).map(|(v, r)| (v, r, r / median)).collect()
}

pub fn picks() -> Vec<Value> {
    read("smart_picks.json")["picks"].as_array().cloned().unwrap_or_default()
}

fn save_picks(p: &[Value]) -> Result<()> {
    write_atomic(&crate::settings::channel_dir().join("smart_picks.json"), &json!({"picks": p}).to_string())
}

pub fn set_status(video_id: &str, status: &str) -> Result<()> {
    let mut p = picks();
    p.iter_mut().filter(|x| x["video_id"] == video_id).for_each(|x| x["status"] = status.into());
    save_picks(&p)
}

/// Snapshot stats, and for each short blowing up (and not already handled) ask the AI for a follow-up.
/// Returns the number of new picks.
pub fn check(s: &Settings, log: &dyn Fn(&str)) -> Result<usize> {
    let a = snapshot(s)?;
    let list = a["list"].as_array().cloned().unwrap_or_default();
    let mut all = picks();
    let mut made = 0;
    // Retention per video when the analytics permission is there (ignored otherwise).
    let retention: HashMap<String, f64> = youtube::studio_report(s, 28)
        .ok()
        .and_then(|r| r["videos"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|v| Some((v["video"].as_str()?.to_string(), v["averageViewPercentage"].as_f64()?)))
        .collect();
    for (v, rate, ratio) in blowing_up(&list) {
        let id = v["id"].as_str().unwrap_or("").to_string();
        if all.iter().any(|p| p["video_id"] == id.as_str()) {
            continue; // one pick per spike
        }
        let title = v["title"].as_str().unwrap_or("").to_string();
        log(&format!("🔥 “{title}” is taking off ({rate:.0} views/hour, {ratio:.1}× your normal). Planning a follow-up…"));
        let comments = youtube::video_comments(s, &id, 20).unwrap_or_default();
        let ret = retention.get(&id).map(|r| format!("{r:.0}% average viewed")).unwrap_or_else(|| "unknown".into());
        let plan = llm::generate_json(s, "research", &format!(
            "One of my YouTube Shorts is blowing up. Plan ONE follow-up short that rides the same interest but is NOT the same video: \
             a sequel, the next question viewers ask, a twist, or the opposite case.\nThe short: \"{title}\"\n\
             Stats: {} views, {} likes, {} comments, {rate:.0} views/hour ({ratio:.1}× my normal), retention {ret}\n\
             Top comments:\n{}\nMy channel: {}\n\
             Return ONLY JSON: {{\"topic\": specific short topic with the hook built in, \"hook\": first line spoken, \"why\": one sentence on why this follow-up will work}}",
            v["views"], v["likes"], v["comments"], comments.join("\n"), s.channel_about
        ))?;
        all.insert(0, json!({
            "video_id": id, "video_title": title, "views": v["views"], "comments": v["comments"], "rate": rate.round(), "ratio": (ratio * 10.0).round() / 10.0,
            "retention": retention.get(&id), "topic": plan["topic"], "hook": plan["hook"], "why": plan["why"], "created": now(), "status": "new"
        }));
        made += 1;
    }
    all.truncate(50);
    save_picks(&all)?;
    Ok(made)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn velocity_is_views_per_hour() {
        let pts = vec![json!({"t": 0, "views": 100}), json!({"t": 3 * 3600, "views": 400}), json!({"t": 6 * 3600, "views": 1000})];
        assert_eq!(velocity(&pts, 6 * 3600), 150.0); // 900 views over 6 h
        assert_eq!(velocity(&pts, 3 * 3600), 200.0); // last 3 h only
        assert_eq!(velocity(&pts[..1], 6 * 3600), 0.0); // one point: unknown
    }
}

/// Once a day: compare shorts with and without the subscribe call (average % viewed and subscribers per 1,000
/// views) and nudge the call-to-action rate. Needs the YouTube Analytics permission and 5+ shorts in each group.
pub fn tune_cta(s: &Settings, log: &dyn Fn(&str)) -> Result<()> {
    let marker = crate::settings::channel_dir().join("cta_tuned_day.txt");
    let today = (now() / 86400).to_string();
    if !s.cta_auto || fs::read_to_string(&marker).ok().as_deref() == Some(today.as_str()) {
        return Ok(());
    }
    let report = youtube::studio_report(s, 90)?;
    fs::write(&marker, &today)?;
    // Which published shorts had the call to action (recorded in each script).
    let mut had_cta: HashMap<String, bool> = HashMap::new();
    for j in crate::pipeline::list_jobs() {
        let (Some(id), Some(vid)) = (j["id"].as_str(), j["video_id"].as_str().filter(|v| !v.is_empty())) else { continue };
        let script: Value = crate::pipeline::job_dir(id).ok().and_then(|d| fs::read_to_string(d.join("script.json")).ok()).and_then(|x| serde_json::from_str(&x).ok()).unwrap_or_default();
        if let Some(c) = script["cta"].as_bool() {
            had_cta.insert(vid.to_string(), c);
        }
    }
    let (mut with, mut without): (Vec<(f64, f64, f64)>, Vec<(f64, f64, f64)>) = (vec![], vec![]);
    for v in report["videos"].as_array().into_iter().flatten() {
        let Some(c) = v["video"].as_str().and_then(|id| had_cta.get(id)) else { continue };
        let row = (v["views"].as_f64().unwrap_or(0.0), v["averageViewPercentage"].as_f64().unwrap_or(0.0), v["subscribersGained"].as_f64().unwrap_or(0.0));
        if *c { with.push(row) } else { without.push(row) }
    }
    if with.len() < 5 || without.len() < 5 {
        return Ok(()); // not enough evidence yet
    }
    let stats = |g: &[(f64, f64, f64)]| {
        let views: f64 = g.iter().map(|r| r.0).sum::<f64>().max(1.0);
        (g.iter().map(|r| r.1).sum::<f64>() / g.len() as f64, g.iter().map(|r| r.2).sum::<f64>() / views * 1000.0)
    };
    let ((ret_c, subs_c), (ret_n, subs_n)) = (stats(&with), stats(&without));
    let mut rate = s.cta_rate as i32;
    if ret_n - ret_c > 5.0 && subs_c <= subs_n * 1.2 {
        rate -= 10; // the ask costs retention without earning more subscribers
    } else if subs_c > subs_n * 1.2 && ret_n - ret_c < 3.0 {
        rate += 10; // it earns subscribers and viewers do not mind
    }
    let rate = rate.clamp(10, 60) as u32;
    if rate != s.cta_rate {
        let mut fresh = Settings::load();
        fresh.cta_rate = rate;
        fresh.save()?;
        log(&format!("📣 Call-to-action rate {} → {rate}% (with CTA: {ret_c:.0}% viewed, {subs_c:.1} subs/1K views; without: {ret_n:.0}%, {subs_n:.1})", s.cta_rate));
    }
    Ok(())
}

/// Launch ideas for a brand-new channel: shown first, like smart picks, until made or dismissed.
pub fn add_starter_picks(topics: &[String]) -> Result<()> {
    let mut all = picks();
    let t = now();
    for (i, topic) in topics.iter().enumerate().rev() {
        all.insert(0, json!({"video_id": format!("starter-{t}-{i}"), "kind": "starter", "topic": topic, "status": "new", "created": t,
                             "why": "Launch idea: built from what is most popular in your niche right now"}));
    }
    save_picks(&all)
}
