//! Top Twitch clips for a streamer (Helix API, app access token).

use crate::llm::http_err;
use crate::settings::Settings;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub fn top_clips(s: &Settings, login: &str, days: u64) -> Result<Vec<Value>> {
    let (id, secret) = (s.twitch_client_id.trim(), s.twitch_client_secret.trim());
    if id.is_empty() || secret.is_empty() {
        bail!("add your Twitch client ID and secret in Settings (free at dev.twitch.tv/console)");
    }
    let login = login.trim().trim_start_matches('@').to_lowercase();
    if login.is_empty() || !login.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("enter a Twitch username, e.g. xqc");
    }
    let tok: Value = ureq::post("https://id.twitch.tv/oauth2/token")
        .send_form(&[("client_id", id), ("client_secret", secret), ("grant_type", "client_credentials")])
        .map_err(http_err)?
        .into_json()?;
    let auth = format!("Bearer {}", tok["access_token"].as_str().context("Twitch returned no token")?);
    let helix = |path: &str, q: &[(&str, &str)]| -> Result<Value> {
        let mut r = ureq::get(&format!("https://api.twitch.tv/helix/{path}")).set("Client-Id", id).set("Authorization", &auth);
        for (k, v) in q {
            r = r.query(k, v);
        }
        Ok(r.call().map_err(http_err)?.into_json()?)
    };
    let user = helix("users", &[("login", &login)])?;
    let broadcaster = user["data"][0]["id"].as_str().with_context(|| format!("Twitch user '{login}' not found"))?.to_string();
    let since = crate::youtube::to_rfc3339(crate::youtube::now_secs().saturating_sub(days.clamp(1, 365) * 86400));
    let clips = helix("clips", &[("broadcaster_id", &broadcaster), ("first", "50"), ("started_at", &since)])?;
    Ok(clips["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| json!({
            "source": "twitch", "id": c["id"], "url": c["url"], "title": c["title"], "channel": c["broadcaster_name"],
            "views": c["view_count"], "duration": c["duration"].as_f64().unwrap_or(0.0).round() as u64,
            "thumb": c["thumbnail_url"], "license": "Needs permission", "language": c["language"], "published": c["created_at"], "likes": 0, "comments": 0,
            "used_secs": c["url"].as_str().map(|u| crate::ledger::used_seconds(&crate::ledger::source_key(u))).unwrap_or(0.0)
        }))
        .collect())
}
