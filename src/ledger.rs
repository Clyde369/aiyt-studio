//! Remembers what content has been used, so the same moment of a video (or the same topic) is never made
//! or uploaded twice. Entries for published shorts are kept forever, even if the job folder is deleted.

use crate::settings::{config_dir, write_atomic};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::Mutex;

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Entry {
    /// "yt:<video id>", "twitch:<clip slug>", "kick:<path>" for clips; "topic:<normalized>" for AI shorts.
    pub key: String,
    pub start: f64,
    pub end: f64,
    pub job: String,
    pub title: String,
    pub published: bool,
}

static LOCK: Mutex<()> = Mutex::new(());

fn path() -> std::path::PathBuf {
    config_dir().join("used_content.json")
}

fn load() -> Vec<Entry> {
    fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save(e: &[Entry]) -> Result<()> {
    write_atomic(&path(), &serde_json::to_string(e)?)
}

/// Same video, however the link was written (watch?v=, youtu.be/, /shorts/, extra parameters…).
pub fn source_key(url: &str) -> String {
    let u = url.trim();
    let after = |pat: &str| u.split_once(pat).map(|(_, r)| r.split(['&', '?', '/', '#']).next().unwrap_or("").to_string());
    if u.contains("youtube.com") || u.contains("youtu.be") {
        if let Some(id) = after("v=").or_else(|| after("youtu.be/")).or_else(|| after("/shorts/")).filter(|i| !i.is_empty()) {
            return format!("yt:{id}");
        }
    }
    if u.contains("twitch.tv") {
        let slug = u.split(['?', '#']).next().unwrap_or(u).trim_end_matches('/').rsplit('/').next().unwrap_or("");
        return format!("twitch:{}", slug.to_lowercase());
    }
    if u.contains("kick.com") {
        let path = u.split_once("kick.com").map(|(_, r)| r).unwrap_or(u).trim_end_matches('/');
        return format!("kick:{}", path.to_lowercase());
    }
    format!("url:{}", u.to_lowercase())
}

/// Topic identity for AI shorts: lowercase letters/digits only, so punctuation or casing changes do not slip through.
pub fn topic_key(topic: &str) -> String {
    format!("topic:{}", topic.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>())
}

fn overlap(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.1.min(b.1) - a.0.max(b.0)).max(0.0)
}

/// A moment counts as "already used" when a quarter or more of it overlaps something used before.
pub fn overlaps_used(used: &[(f64, f64)], r: (f64, f64)) -> bool {
    let len = (r.1 - r.0).max(1.0);
    used.iter().any(|u| overlap(*u, r) >= len * 0.25)
}

/// Every range of this source already taken by any short (made, queued or published).
pub fn used_ranges(key: &str) -> Vec<(f64, f64)> {
    load().into_iter().filter(|e| e.key == key).map(|e| (e.start, e.end)).collect()
}

pub fn used_seconds(key: &str) -> f64 {
    used_ranges(key).iter().map(|r| r.1 - r.0).sum()
}

pub fn reserve(key: &str, start: f64, end: f64, job: &str, title: &str) -> Result<()> {
    let _g = LOCK.lock().unwrap();
    let mut e = load();
    e.retain(|x| !(x.job == job && x.key == key && !x.published));
    e.push(Entry { key: key.into(), start, end, job: job.into(), title: title.into(), published: false });
    save(&e)
}

/// Called after a successful upload.
pub fn mark_published(job: &str, topic_key: Option<(&str, &str)>) -> Result<()> {
    let _g = LOCK.lock().unwrap();
    let mut e = load();
    e.iter_mut().filter(|x| x.job == job).for_each(|x| x.published = true);
    if let Some((key, title)) = topic_key {
        e.push(Entry { key: key.into(), job: job.into(), title: title.into(), published: true, ..Default::default() });
    }
    save(&e)
}

/// A deleted, never-published job frees its moments again.
pub fn release(job: &str) -> Result<()> {
    let _g = LOCK.lock().unwrap();
    let mut e = load();
    e.retain(|x| x.job != job || x.published);
    save(&e)
}

/// Is this exact content already on the channel (from another job)? Returns the title it was published as.
pub fn published_conflict(job: &str, keys: &[(String, f64, f64)]) -> Option<String> {
    let all = load();
    keys.iter().find_map(|(key, s, en)| {
        all.iter()
            .filter(|x| x.published && x.job != job && &x.key == key)
            .find(|x| key.starts_with("topic:") || overlaps_used(&[(x.start, x.end)], (*s, *en)))
            .map(|x| x.title.clone())
    })
}

/// Titles already made or published, so the AI does not suggest them again.
pub fn published_titles() -> Vec<String> {
    load().into_iter().filter(|e| e.published && !e.title.is_empty()).map(|e| e.title).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_overlap() {
        for u in ["https://www.youtube.com/watch?v=abc123&t=30", "https://youtu.be/abc123?si=x", "https://youtube.com/shorts/abc123"] {
            assert_eq!(source_key(u), "yt:abc123", "{u}");
        }
        assert_eq!(source_key("https://clips.twitch.tv/FunnyClip-xyz/"), "twitch:funnyclip-xyz");
        assert_eq!(source_key("https://www.twitch.tv/streamer/clip/FunnyClip-xyz?filter=clips"), "twitch:funnyclip-xyz");
        assert_eq!(topic_key("Why do Cats PURR?!"), topic_key("why do cats purr"));
        let used = [(100.0, 150.0)];
        assert!(overlaps_used(&used, (120.0, 160.0))); // 30 of 40 s reused
        assert!(!overlaps_used(&used, (145.0, 200.0))); // 5 of 55 s: fine
        assert!(!overlaps_used(&used, (0.0, 40.0)));
    }
#[test]
    fn never_reuses_or_republishes() {
        let _ = crate::settings::CONFIG_DIR.set(std::env::temp_dir().join("aiyt-ledger-test"));
        std::fs::create_dir_all(config_dir()).unwrap();
        let key = format!("yt:test{}", std::process::id());
        reserve(&key, 100.0, 150.0, "jobA", "Clip A").unwrap();
        reserve(&key, 300.0, 340.0, "jobB", "Clip B").unwrap();
        mark_published("jobA", Some((&topic_key("Clip A"), "Clip A"))).unwrap();
        // jobA's moment is taken for everyone else; an untouched part is fine.
        assert_eq!(published_conflict("jobC", &[(key.clone(), 110.0, 150.0)]).as_deref(), Some("Clip A"));
        assert!(published_conflict("jobC", &[(key.clone(), 200.0, 240.0)]).is_none());
        // Same title again is blocked even from a different video.
        assert!(published_conflict("jobD", &[(topic_key("clip a!"), 0.0, 0.0)]).is_some());
        // Deleting an unpublished job frees its moment; a published one stays used.
        release("jobB").unwrap();
        release("jobA").unwrap();
        let used = used_ranges(&key);
        assert!(used.contains(&(100.0, 150.0)) && !used.contains(&(300.0, 340.0)), "{used:?}");
    }
}


/// Shorts published before the ledger existed (or from another copy of the app's data) get recorded once at startup.
pub fn seed_published(jobs: &[serde_json::Value]) {
    let known: std::collections::HashSet<String> = load().into_iter().filter(|e| e.published).map(|e| e.job).collect();
    for j in jobs {
        let (Some(id), Some(vid)) = (j["id"].as_str(), j["video_id"].as_str()) else { continue };
        if vid.is_empty() || known.contains(id) {
            continue;
        }
        let title = j["title"].as_str().unwrap_or("").to_string();
        let script: serde_json::Value = crate::pipeline::job_dir(id).ok().and_then(|d| fs::read_to_string(d.join("script.json")).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let src = j["source_url"].as_str().unwrap_or("");
        let _ = if src.is_empty() {
            reserve(&topic_key(j["topic"].as_str().unwrap_or(&title)), 0.0, 0.0, id, &title)
        } else {
            reserve(&source_key(src), script["start"].as_f64().unwrap_or(0.0), script["end"].as_f64().unwrap_or(0.0), id, &title)
        };
        let _ = mark_published(id, Some((&topic_key(&title), &title)));
    }
}
