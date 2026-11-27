//! "Why is this video engaging?" Downloads a video, transcribes it locally (Whisper), measures pacing from scene
//! cuts, grabs keyframes, then asks your AI to break down the hook, structure and retention tricks.

use crate::llm::{self, command, run_full};
use crate::settings::{out_dir, Settings};
use crate::{media, tools};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs;

/// Analysis may look at more sites than clipping (we only study these videos, never reuse them).
pub fn check_watch_url(url: &str) -> Result<()> {
    let rest = url.trim().strip_prefix("https://").context("links must start with https://")?;
    let host = rest.split(['/', '?']).next().unwrap_or("").trim_start_matches("www.").trim_start_matches("m.").to_lowercase();
    let ok = ["youtube.com", "youtu.be", "tiktok.com", "vm.tiktok.com", "instagram.com", "x.com", "twitter.com", "twitch.tv", "clips.twitch.tv", "kick.com", "facebook.com", "fb.watch"];
    if !ok.contains(&host.as_str()) {
        bail!("paste a YouTube, TikTok, Instagram, X, Facebook, Twitch or Kick link");
    }
    Ok(())
}

/// Times (seconds) where the picture changes sharply: ffmpeg scene detection.
fn scene_cuts(dir: &std::path::Path, file: &str) -> Result<Vec<f64>> {
    let (_, err) = run_full(
        command("ffmpeg").current_dir(dir).args(["-hide_banner", "-i", file, "-an", "-vf", "select='gt(scene,0.32)',showinfo", "-f", "null", "-"]),
        "ffmpeg",
    )?;
    Ok(String::from_utf8_lossy(&err)
        .lines()
        .filter(|l| l.contains("showinfo") && l.contains("pts_time:"))
        .filter_map(|l| l.split("pts_time:").nth(1)?.split_whitespace().next()?.parse().ok())
        .collect())
}

pub fn analyze(s: &Settings, url: &str, log: &dyn Fn(&str)) -> Result<Value> {
    check_watch_url(url)?;
    let key = crate::ledger::source_key(url).chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>();
    let dir = out_dir().join("_watch").join(key.chars().take(60).collect::<String>());
    fs::create_dir_all(&dir)?;
    if !dir.join("source.mp4").exists() {
        log("Downloading the video (first 10 minutes)…");
        tools::download_for_analysis(url, &dir, log)?;
    }
    let info: Value = fs::read_to_string(dir.join("source.info.json")).ok().and_then(|x| serde_json::from_str(&x).ok()).unwrap_or_default();
    let dur = media::duration(&dir.join("source.mp4"))?;

    log("Listening (local Whisper transcript)…");
    let transcript = if dir.join("watch.srt").exists() {
        tools::parse_srt(&fs::read_to_string(dir.join("watch.srt"))?)
    } else {
        // ffmpeg runs in the video's folder; the Whisper model sits two levels up in .aiyt-tools.
        tools::transcribe_with(&dir, "source.mp4", "watch.srt", "../../.aiyt-tools/ggml-base.bin", "auto", log).unwrap_or_default()
    };
    log("Watching for cuts and pacing…");
    let cuts = scene_cuts(&dir, "source.mp4").unwrap_or_default();
    log("Grabbing keyframes…");
    let frames: Vec<String> = (0..6)
        .filter_map(|i| {
            let t = dur * (i as f64 + 0.5) / 6.0;
            let name = format!("frame_{i}.jpg");
            media::ffmpeg(&dir, &["-ss", &format!("{t:.2}"), "-i", "source.mp4", "-frames:v", "1", "-vf", "scale=360:-2", "-q:v", "4", &name]).ok()?;
            Some(dir.join(name).to_string_lossy().to_string())
        })
        .collect();

    let words: usize = transcript.iter().map(|t| t.2.split_whitespace().count()).sum();
    let hook: String = transcript.iter().take_while(|t| t.0 < 3.5).map(|t| t.2.as_str()).collect::<Vec<_>>().join(" ");
    let metrics = json!({
        "duration": dur.round(), "cuts": cuts.len(),
        "avg_shot_seconds": if cuts.is_empty() { dur } else { (dur / (cuts.len() + 1) as f64 * 10.0).round() / 10.0 },
        "cuts_per_minute": (cuts.len() as f64 / (dur / 60.0).max(0.1) * 10.0).round() / 10.0,
        "words_per_minute": (words as f64 / (dur / 60.0).max(0.1)).round(),
        "first_cut_at": cuts.first().copied(), "hook_words": hook,
        "views": info["view_count"], "likes": info["like_count"], "comments": info["comment_count"],
    });
    let lines: String = transcript.iter().map(|(a, _, t)| format!("[{a:.0}s] {t}\n")).collect::<String>().chars().take(9000).collect();
    let cut_list: Vec<String> = cuts.iter().take(80).map(|c| format!("{c:.1}")).collect();
    log("AI is breaking down why it works…");
    let channel = if s.channel_about.is_empty() { String::new() } else { format!("My channel: {}\n", s.channel_about) };
    let mut report = llm::generate_json(s, "research", &format!(
        "You are a short-form video analyst. Explain precisely why this video holds attention, using the evidence below.\n\
         Title: {}\nCreator: {}\nViews: {} · Likes: {} · Comments: {}\nMeasured: {}\nScene cuts at (s): {}\nTranscript:\n{}\n{channel}\
         Return ONLY JSON: {{\"verdict\": 2 sentences, \"hook\": {{\"what\": what happens in the first 3 seconds, \"why\": why it stops the scroll}}, \
         \"structure\": [{{\"time\": \"0-3s\", \"beat\": what happens and its purpose}}] (4-8 beats), \"retention\": [5 specific retention techniques used], \
         \"pacing\": one sentence on edit speed and rhythm, \"emotion\": the main emotions it triggers, \"cta\": how it drives likes/comments/follows, \
         \"copy_this\": [5 concrete things to copy in my own shorts], \"ideas\": [3 short ideas that reuse this formula{}]}}",
        info["title"].as_str().unwrap_or(""), info["uploader"].as_str().unwrap_or(""), info["view_count"], info["like_count"], info["comment_count"],
        metrics, cut_list.join(", "), if lines.is_empty() { "(no speech detected)" } else { &lines },
        if channel.is_empty() { "" } else { " for my channel" }
    ))?;
    report["metrics"] = metrics;
    report["frames"] = frames.into();
    report["title"] = info["title"].clone();
    report["creator"] = info["uploader"].clone();
    report["url"] = url.into();
    fs::write(dir.join("report.json"), serde_json::to_string_pretty(&report)?)?;
    Ok(report)
}
