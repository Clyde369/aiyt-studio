//! Winning formula: what this channel's best shorts have in common (from its own numbers), new shorts made exactly
//! that way, and a forecast of when the channel reaches each Partner Program tier.

use crate::settings::{channel_dir, write_atomic, Settings};
use crate::{llm, pipeline, youtube};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;

fn path() -> std::path::PathBuf {
    channel_dir().join("formula.json")
}

/// The last analysis for this channel.
pub fn load() -> Option<Value> {
    fs::read_to_string(path()).ok().and_then(|x| serde_json::from_str(&x).ok())
}

fn save(v: &Value) -> Result<()> {
    write_atomic(&path(), &serde_json::to_string_pretty(v)?)
}

/// The channel's public shorts, fastest-growing first: views per day since upload (fair to new and old shorts),
/// engagement, and how the app made them when it did.
fn performance(s: &Settings) -> Result<Vec<Value>> {
    let a = youtube::analytics(s)?;
    let now = youtube::now_secs();
    let made: HashMap<String, String> = pipeline::list_jobs_all()
        .into_iter()
        .filter_map(|j| Some((j["video_id"].as_str().filter(|v| !v.is_empty())?.to_string(), j["id"].as_str()?.to_string())))
        .collect();
    let mut list: Vec<Value> = a["list"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["privacy"] == "public" && v["duration"].as_u64().unwrap_or(0) <= 180)
        .filter_map(|v| {
            let published = youtube::parse_rfc3339(v["published"].as_str()?)?;
            let days = (now.saturating_sub(published) as f64 / 86400.0).max(1.0);
            let views = v["views"].as_f64().unwrap_or(0.0);
            let mut x = json!({
                "id": v["id"], "title": v["title"], "views": views, "days": (days * 10.0).round() / 10.0, "per_day": (views / days).round(),
                "duration": v["duration"], "like_rate": if views > 0.0 { v["likes"].as_f64().unwrap_or(0.0) / views } else { 0.0 },
                "comments": v["comments"], "published": v["published"],
            });
            if let Some(job) = made.get(v["id"].as_str().unwrap_or("")) {
                x["made"] = how_made(job);
            }
            Some(x)
        })
        .collect();
    list.sort_by(|a, b| b["per_day"].as_f64().unwrap_or(0.0).total_cmp(&a["per_day"].as_f64().unwrap_or(0.0)));
    Ok(list)
}

/// What the app put into a short it made: hook, length in words, visuals, thumbnail text, mood.
fn how_made(job_id: &str) -> Value {
    let Some(script) = pipeline::job_dir(job_id).ok().and_then(|d| fs::read_to_string(d.join("script.json")).ok()).and_then(|x| serde_json::from_str::<Value>(&x).ok()) else {
        return Value::Null;
    };
    let narration = script["narration"].as_str().unwrap_or("");
    let hook: String = narration.split_inclusive(['.', '!', '?']).next().unwrap_or("").trim().chars().take(160).collect();
    let visuals = if script["story"].is_object() {
        "animated story"
    } else if script["start"].is_number() {
        "clip from another video"
    } else if script["scenes"].is_array() && !script["keywords"].is_array() {
        "AI pictures"
    } else {
        "stock video"
    };
    json!({"hook": hook, "words": narration.split_whitespace().count(), "visuals": visuals, "thumbnail_text": script["thumbnail_text"],
           "on_screen_hook": script["hook"], "mood": script["mood"], "asks_to_subscribe": script["cta"]})
}

/// Deep analysis of the channel's best shorts against its weakest: the exact formula, the evidence, and 10 new shorts
/// that follow it with predicted views.
pub fn analyse(s: &Settings, log: &dyn Fn(&str)) -> Result<Value> {
    if !youtube::is_connected() {
        bail!("link this channel to YouTube first (Settings)");
    }
    log("Reading every short on the channel and its numbers…");
    let mut shorts = performance(s)?;
    if shorts.len() < 3 {
        bail!("needs at least 3 public shorts on this channel to find what works");
    }
    // Retention from YouTube Analytics when allowed (it is the strongest signal for Shorts).
    match youtube::studio_report(s, 90) {
        Ok(r) => {
            let by_id: HashMap<String, Value> = r["videos"].as_array().into_iter().flatten().filter_map(|v| Some((v["video"].as_str()?.to_string(), v.clone()))).collect();
            for x in shorts.iter_mut() {
                if let Some(v) = by_id.get(x["id"].as_str().unwrap_or("")) {
                    x["avg_view_pct"] = v["averageViewPercentage"].clone();
                    x["subs_gained"] = v["subscribersGained"].clone();
                }
            }
        }
        Err(e) => log(&format!("  retention unavailable ({e:#}); using views and likes")),
    }
    let n = shorts.len();
    let k = (n * 3 / 10).max(3).min(n);
    let top: Vec<&Value> = shorts.iter().take(k).collect();
    let bottom: Vec<&Value> = if n >= 6 { shorts.iter().rev().take(k.min(n - k)).collect() } else { vec![] };
    log("Reading what viewers say on your best shorts…");
    let mut comments = vec![];
    for v in top.iter().take(3) {
        if let Ok(c) = youtube::video_comments(s, v["id"].as_str().unwrap_or(""), 8) {
            comments.push(format!("On “{}”:\n{}", v["title"].as_str().unwrap_or(""), c.join("\n")));
        }
    }
    let line = |v: &Value| {
        let mut l = format!(
            "- \"{}\" | {} views/day | {} views in {} days | {}s | likes {:.1}%",
            v["title"].as_str().unwrap_or(""), v["per_day"], v["views"], v["days"], v["duration"], v["like_rate"].as_f64().unwrap_or(0.0) * 100.0
        );
        if let Some(p) = v["avg_view_pct"].as_f64() {
            l.push_str(&format!(" | watched {p:.0}% on average"));
        }
        let m = &v["made"];
        if m.is_object() {
            l.push_str(&format!(" | made: opens with \"{}\", {} words, {}, thumbnail \"{}\"", m["hook"].as_str().unwrap_or(""), m["words"], m["visuals"].as_str().unwrap_or(""), m["thumbnail_text"].as_str().unwrap_or("")));
        }
        l
    };
    let titles: Vec<String> = shorts.iter().filter_map(|v| v["title"].as_str().map(String::from)).collect();
    log("AI is working out your winning formula…");
    let mut v = llm::generate_json(s, "research", &format!(
        "You are a YouTube Shorts analyst. Think deeply and be specific: this channel wants to make more shorts EXACTLY like its winners.\n\
         Channel: {}\n\nTOP shorts (fastest views per day first):\n{}\n\nWEAKEST shorts:\n{}\n\nWhat viewers comment on the top shorts:\n{}\n\n\
         Compare the top shorts with the weakest ones: topic type, title wording and structure, the opening line, beat structure, length, pacing, \
         tone, visuals, thumbnail text, asking to subscribe or not. Only claim what the numbers support. \
         \"watched above 100%\" means people rewatch.\n\
         Every title already on the channel (never suggest these or near-copies): {}\n\
         Return ONLY JSON: {{\"summary\": 2 sentences, \
         \"formula\": {{\"name\": a short name for the formula, \"title_pattern\": the title template with one example, \"hook\": the opening-line pattern with one example, \
         \"structure\": [the beats in order, with rough timing], \"length_seconds\": number, \"pacing\": words per second and sentence style, \"tone\": tone of voice, \
         \"visuals\": what is on screen, \"thumbnail_text\": the on-screen text pattern, \"topics\": [kinds of topics that win here], \"avoid\": [what the weak shorts did]}}, \
         \"evidence\": [5 findings comparing top and weakest shorts, each citing titles and numbers], \
         \"suggestions\": [10 {{\"topic\": a specific new short that follows the formula, \"title\": its title in the formula's pattern, \"hook\": its opening line, \
         \"why\": one sentence, \"predicted_views\": {{\"low\": number, \"high\": number}} (first-month views, based on this channel's own numbers)}}], \
         \"posting\": one sentence on posting times or frequency if the data shows anything, else null}}",
        s.channel_about,
        top.iter().map(|v| line(v)).collect::<Vec<_>>().join("\n"),
        if bottom.is_empty() { "(too few shorts to compare)".to_string() } else { bottom.iter().map(|v| line(v)).collect::<Vec<_>>().join("\n") },
        if comments.is_empty() { "(none)".to_string() } else { comments.join("\n") },
        titles.join("; ")
    ))?;
    if !v["formula"].is_object() || !v["suggestions"].is_array() {
        bail!("the AI did not return a formula; try again");
    }
    v["analysed_at"] = youtube::now_secs().into();
    v["shorts"] = n.into();
    v["top"] = top.iter().map(|v| json!({"id": v["id"], "title": v["title"], "per_day": v["per_day"], "views": v["views"], "avg_view_pct": v["avg_view_pct"]})).collect::<Vec<_>>().into();
    v["top_visuals"] = top.iter().map(|v| v["made"]["visuals"].as_str().unwrap_or("unknown").to_string()).collect::<Vec<_>>().into();
    v["top_median_views"] = median(top.iter().map(|v| v["views"].as_f64().unwrap_or(0.0)).collect()).into();
    v["use_for_auto"] = load().map(|o| o["use_for_auto"].clone()).unwrap_or(Value::Bool(false));
    save(&v)?;
    Ok(v)
}

fn median(mut x: Vec<f64>) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    x.sort_by(f64::total_cmp);
    x[x.len() / 2]
}

/// The formula as instructions for the script writer.
pub fn brief(f: &Value) -> String {
    let g = &f["formula"];
    let s = |k: &str| g[k].as_str().unwrap_or("").to_string();
    let list = |k: &str| g[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("; ")).unwrap_or_default();
    format!(
        "Formula: {}\nTitle pattern: {}\nOpening line: {}\nStructure: {}\nLength: {} seconds\nPacing: {}\nTone: {}\nVisuals: {}\nThumbnail text: {}\nAvoid: {}",
        s("name"), s("title_pattern"), s("hook"), list("structure"), g["length_seconds"], s("pacing"), s("tone"), s("visuals"), s("thumbnail_text"), list("avoid")
    )
}

fn length(f: &Value) -> u32 {
    f["formula"]["length_seconds"].as_f64().map(|x| (x.round() as u32).clamp(15, 90)).unwrap_or(0)
}

/// The winners were mostly clips of other videos (so new shorts should be clips too, not stock footage).
pub fn clip_based(f: &Value) -> bool {
    let v: Vec<&str> = f["top_visuals"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
    !v.is_empty() && v.iter().filter(|x| **x == "clip from another video").count() * 2 > v.len()
}

/// Queues shorts for these topics that follow the formula exactly (and marks those suggestions as used). When the
/// winners were clips, each topic becomes a clip of the most-viewed fresh Creative Commons video about it.
pub fn queue(s: &Settings, topics: &[String], log: &dyn Fn(&str)) -> Result<Vec<String>> {
    let mut f = load().context("analyse your shorts first (Analytics → Winning formula)")?;
    let (text, secs) = (brief(&f), length(&f));
    let mut ids = vec![];
    for t in topics.iter().filter(|t| !t.trim().is_empty()) {
        if !clip_based(&f) {
            ids.push(pipeline::create_job_with(t, &[], &text, secs)?);
            continue;
        }
        log(&format!("Finding a Creative Commons video for “{t}”…"));
        let found = youtube::search_cc(s, t).unwrap_or_default();
        // Fresh enough: at least half of the video has not been used for a short yet.
        let pick = found.iter().find(|v| v["duration"].as_f64().unwrap_or(0.0) >= 40.0 && v["used_secs"].as_f64().unwrap_or(0.0) * 2.0 < v["duration"].as_f64().unwrap_or(0.0));
        let Some(v) = pick else {
            log(&format!("  no fresh Creative Commons video for “{t}”; making it with your usual visuals"));
            ids.push(pipeline::create_job_with(t, &[], &text, secs)?);
            continue;
        };
        for id in pipeline::create_clip_jobs(v["url"].as_str().unwrap_or(""), v["title"].as_str().unwrap_or(t), v["channel"].as_str().unwrap_or(""), &[], 1)? {
            let mut st = pipeline::load_state(&id)?;
            (st.formula, st.seconds) = (text.clone(), secs);
            pipeline::save_state(&id, &st)?;
            ids.push(id);
        }
    }
    for s in f["suggestions"].as_array_mut().into_iter().flatten() {
        if topics.iter().any(|t| Some(t.as_str()) == s["topic"].as_str()) {
            s["used"] = true.into();
        }
    }
    save(&f)?;
    Ok(ids)
}

/// Autopilot: up to `n` unused suggestions when the channel chose to follow its formula.
pub fn auto_topics(n: usize) -> Vec<String> {
    let Some(f) = load().filter(|f| f["use_for_auto"] == true) else { return vec![] };
    f["suggestions"].as_array().into_iter().flatten().filter(|s| s["used"] != true).filter_map(|s| s["topic"].as_str().map(String::from)).take(n).collect()
}

pub fn set_auto(on: bool) -> Result<()> {
    let mut f = load().context("analyse your shorts first")?;
    f["use_for_auto"] = on.into();
    save(&f)
}

/// Days until a rolling 90-day view total reaches `need`, given the last 90 days of daily views (oldest first) and
/// the projected views on future day `t` (1 = tomorrow). None when not within `max_days`.
fn days_until(last90: &[f64], future: impl Fn(u32) -> f64, need: f64, max_days: u32) -> Option<u32> {
    let mut window: std::collections::VecDeque<f64> = last90.iter().copied().collect();
    while window.len() < 90 {
        window.push_front(0.0);
    }
    while window.len() > 90 {
        window.pop_front();
    }
    let mut sum: f64 = window.iter().sum();
    if sum >= need {
        return Some(0);
    }
    for t in 1..=max_days {
        sum -= window.pop_front().unwrap_or(0.0);
        let v = future(t).max(0.0);
        window.push_back(v);
        sum += v;
        if sum >= need {
            return Some(t);
        }
    }
    None
}

/// When each Partner Program tier is reached at the current pace, and when posting the formula every day, plus the
/// Shorts revenue to expect once in. Simple, honest models: flat current pace; formula = each new short earning the
/// lifetime views of your typical top short, ramping up over two weeks.
pub fn forecast(s: &Settings) -> Result<Value> {
    let m = youtube::monetization(s)?;
    let (daily, basis) = match youtube::daily_stats(s, 90) {
        Ok(d) if !d.is_empty() => (d, "your last 90 days in YouTube Analytics"),
        _ => (vec![], "your recent uploads (for exact daily numbers: Settings → YouTube → Reconnect, so the app can read YouTube Analytics)"),
    };
    let views: Vec<f64> = daily.iter().map(|d| d["views"].as_f64().unwrap_or(0.0)).collect();
    let subs_gained: Vec<f64> = daily.iter().map(|d| d["subscribersGained"].as_f64().unwrap_or(0.0)).collect();
    let recent = |x: &[f64]| -> Option<f64> { (!x.is_empty()).then(|| x.iter().rev().take(28).sum::<f64>() / x.len().min(28) as f64) };
    // No daily numbers from YouTube Analytics: estimate today's pace from the views of the last 4 weeks of uploads.
    let from_uploads = || -> Option<f64> {
        let a = youtube::analytics(s).ok()?;
        let now = youtube::now_secs();
        let ages: Vec<(f64, f64)> = a["list"].as_array()?.iter().filter_map(|v| {
            let age = now.saturating_sub(youtube::parse_rfc3339(v["published"].as_str()?)?) as f64 / 86400.0;
            (age <= 28.0 && v["privacy"] == "public").then(|| (age, v["views"].as_f64().unwrap_or(0.0)))
        }).collect();
        let span = ages.iter().map(|a| a.0).fold(1.0f64, f64::max).min(28.0);
        (!ages.is_empty()).then(|| ages.iter().map(|a| a.1).sum::<f64>() / span)
    };
    let views_now = recent(&views).or_else(from_uploads).or(m["views_per_day"].as_f64()).unwrap_or(0.0).max(0.0);
    let subs_now = recent(&subs_gained).or(m["subs_per_day"].as_f64()).unwrap_or(0.0).max(0.0);
    let last90: Vec<f64> = if views.is_empty() { vec![m["shorts_views_90"].as_f64().unwrap_or(0.0) / 90.0; 90] } else { views.clone() };
    let subs = m["subs"].as_f64().unwrap_or(0.0);
    let rpm = s.rpm.max(0.0);

    let f = load();
    // How many the channel would post: the daily automation plan, else the upload limit YouTube allows it (learned), else 3.
    let cap = fs::read_to_string(channel_dir().join("upload_cap.json")).ok().and_then(|x| serde_json::from_str::<Value>(&x).ok()).and_then(|c| c["cap"].as_u64());
    let posts = if s.auto.enabled { s.auto.per_day.max(1) as u64 } else { cap.unwrap_or(3).clamp(1, 20) } as f64;
    let per_short = f.as_ref().and_then(|f| f["top_median_views"].as_f64()).unwrap_or(0.0);
    let target = views_now.max(posts * per_short);
    let scenario = |name: &str, daily_views: &dyn Fn(u32) -> f64| -> Value {
        // Subscribers come with views: the recent rate, else the channel's lifetime subscribers per view.
        let lifetime = m["subs"].as_f64().unwrap_or(0.0) / m["views"].as_f64().unwrap_or(0.0).max(1.0);
        let subs_per_view = if views_now > 0.0 && subs_now > 0.0 { subs_now / views_now } else { lifetime };
        let tiers: Vec<Value> = m["tiers"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| {
                let views_days = days_until(&last90, daily_views, t["shorts_views"].as_f64().unwrap_or(0.0), 730);
                let mut need_subs = (t["subs"].as_f64().unwrap_or(0.0) - subs).max(0.0);
                let mut subs_days = Some(0u32);
                for d in 1..=730u32 {
                    if need_subs <= 0.0 {
                        break;
                    }
                    need_subs -= daily_views(d) * subs_per_view;
                    subs_days = (need_subs <= 0.0).then_some(d);
                }
                if need_subs > 0.0 {
                    subs_days = None;
                }
                let eta = match (views_days, subs_days) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    _ => None,
                };
                json!({"name": t["name"], "views_needed_per_day": (t["shorts_views"].as_f64().unwrap_or(0.0) / 90.0).round(), "views_days": views_days, "subs_days": subs_days, "eta_days": eta,
                       "monthly_revenue": eta.map(|d| (daily_views(d.max(1)) * 30.0 * rpm / 1000.0 * 100.0).round() / 100.0)})
            })
            .collect();
        json!({"name": name, "views_per_day": daily_views(30).round(), "tiers": tiers})
    };
    let current = scenario("At your current pace", &|_| views_now);
    let with_formula = (per_short > 0.0).then(|| {
        let ramp = move |t: u32| views_now + (target - views_now) * (t as f64 / 14.0).min(1.0);
        let mut v = scenario(&format!("Posting your formula {posts} times a day"), &ramp);
        v["per_short"] = per_short.round().into();
        v
    });
    let scenarios: Vec<Value> = std::iter::once(current).chain(with_formula).collect();
    Ok(json!({
        "basis": basis, "views_per_day": views_now.round(), "subs_per_day": (subs_now * 10.0).round() / 10.0, "rpm": rpm,
        "monthly_revenue_now": (views_now * 30.0 * rpm / 1000.0 * 100.0).round() / 100.0,
        "scenarios": scenarios,
    }))
}

/// Live on the active channel (read-only on YouTube): `cargo test formula_live -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn formula_live() {
    let appdata = std::path::PathBuf::from(std::env::var("APPDATA").unwrap()).join("com.aiyt.studio");
    let _ = crate::settings::CONFIG_DIR.set(appdata);
    let _ = crate::settings::OUT_DIR.set(std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap()).join("Videos").join("AIYT"));
    let s = Settings::load();
    let f = analyse(&s, &|m| println!("{m}")).unwrap();
    println!("{}", serde_json::to_string_pretty(&json!({"summary": f["summary"], "formula": f["formula"], "evidence": f["evidence"], "top": f["top"], "first_suggestions": f["suggestions"].as_array().map(|a| a.iter().take(4).cloned().collect::<Vec<_>>())})).unwrap());
    println!("BRIEF:\n{}", brief(&f));
    println!("FORECAST:\n{}", serde_json::to_string_pretty(&forecast(&s).unwrap()).unwrap());
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_window_counts_only_the_last_90_days() {
        // Already there.
        assert_eq!(days_until(&[100.0; 90], |_| 0.0, 9000.0, 10), Some(0));
        // 50/day for 90 days = 4500; 100/day from now on adds 50 net per day: 4500 + 50t >= 5000 at t = 10.
        assert_eq!(days_until(&[50.0; 90], |_| 100.0, 5000.0, 400), Some(10));
        // Old views drop out of the window: steady 10/day can never reach 2000.
        assert_eq!(days_until(&[10.0; 90], |_| 10.0, 2000.0, 730), None);
        // Fewer than 90 known days count the rest as zero.
        assert_eq!(days_until(&[30.0; 30], |_| 30.0, 2700.0, 200), Some(60));
    }

    #[test]
    fn brief_lists_the_formula() {
        let f = json!({"formula": {"name": "DIY shock", "title_pattern": "I built X", "hook": "Start mid-action", "structure": ["hook", "build", "twist"],
                                    "length_seconds": 32.4, "avoid": ["slow intros"]}});
        let b = brief(&f);
        assert!(b.contains("Formula: DIY shock") && b.contains("Structure: hook; build; twist") && b.contains("Avoid: slow intros"), "{b}");
        assert_eq!(length(&f), 32);
        assert_eq!(median(vec![5.0, 1.0, 9.0]), 5.0);
    }
}
