//! Free stock footage that is licensed for commercial (monetized) use: Pexels and Pixabay.

use crate::llm::http_err;
use crate::settings::Settings;
use anyhow::{bail, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::path::Path;

/// Download one clip for `query` into `out`. Tries the preferred library first, then the other one.
/// Returns the source page URL (for credits). `used` stops the same clip appearing twice in a short.
pub fn fetch_clip(s: &Settings, preferred: &str, query: &str, out: &Path, used: &mut HashSet<String>) -> Result<String> {
    let order = if preferred == "pixabay" { ["pixabay", "pexels"] } else { ["pexels", "pixabay"] };
    let mut last_err = None;
    for src in order {
        let found = match src {
            "pexels" if !s.pexels_key.trim().is_empty() => pexels(s, query, used),
            "pixabay" if !s.pixabay_key.trim().is_empty() => pixabay(s, query, used),
            _ => continue,
        };
        match found {
            Ok(Some((id, url, page))) => {
                let mut reader = ureq::get(&url).call().map_err(http_err)?.into_reader();
                std::io::copy(&mut reader, &mut File::create(out)?)?;
                used.insert(id);
                return Ok(page);
            }
            Ok(None) => {}
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(e),
        None if s.pexels_key.trim().is_empty() && s.pixabay_key.trim().is_empty() => bail!("add a free Pexels or Pixabay API key in Settings"),
        None => bail!("no stock clip found for \"{query}\""),
    }
}

/// (unique id, download url, page url)
type Hit = Option<(String, String, String)>;

fn pexels(s: &Settings, query: &str, used: &HashSet<String>) -> Result<Hit> {
    let v: Value = ureq::get("https://api.pexels.com/videos/search")
        .set("Authorization", s.pexels_key.trim())
        .query("query", query)
        .query("orientation", "portrait")
        .query("per_page", "15")
        .call()
        .map_err(http_err)?
        .into_json()?;
    for video in v["videos"].as_array().into_iter().flatten() {
        let id = format!("pexels-{}", video["id"]);
        if used.contains(&id) {
            continue;
        }
        // Closest to 1080 wide without pulling 4K files.
        let best = video["video_files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|f| f["file_type"] == "video/mp4" && f["width"].as_i64().unwrap_or(0) >= 540 && f["width"].as_i64().unwrap_or(0) <= 1440)
            .min_by_key(|f| (f["width"].as_i64().unwrap_or(0) - 1080).abs());
        if let Some(link) = best.and_then(|f| f["link"].as_str()) {
            return Ok(Some((id, link.into(), video["url"].as_str().unwrap_or("").into())));
        }
    }
    Ok(None)
}

fn pixabay(s: &Settings, query: &str, used: &HashSet<String>) -> Result<Hit> {
    let v: Value = ureq::get("https://pixabay.com/api/videos/")
        .query("key", s.pixabay_key.trim())
        .query("q", &query.chars().take(100).collect::<String>())
        .query("safesearch", "true")
        .query("per_page", "15")
        .call()
        .map_err(http_err)?
        .into_json()?;
    for hit in v["hits"].as_array().into_iter().flatten() {
        let id = format!("pixabay-{}", hit["id"]);
        if used.contains(&id) {
            continue;
        }
        let file = ["large", "medium", "small"].iter().map(|k| &hit["videos"][k]).find(|f| f["url"].as_str().is_some_and(|u| !u.is_empty()) && f["width"].as_i64().unwrap_or(0) <= 1920);
        if let Some(url) = file.and_then(|f| f["url"].as_str()) {
            return Ok(Some((id, url.into(), hit["pageURL"].as_str().unwrap_or("").into())));
        }
    }
    Ok(None)
}
