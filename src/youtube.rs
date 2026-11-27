//! YouTube Data API v3: sign-in (OAuth desktop flow), upload + scheduling, trend research, comments.

use crate::llm::http_err;
use crate::settings::{config_dir, Settings};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// yt-analytics.readonly: Studio-style stats (watch time, retention, traffic sources). Added later, so older logins must reconnect.
const SCOPES: &str = "https://www.googleapis.com/auth/youtube.upload https://www.googleapis.com/auth/youtube.force-ssl https://www.googleapis.com/auth/yt-analytics.readonly";
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const API: &str = "https://www.googleapis.com/youtube/v3";

/// (channel id, access token, fetched at): one cached token, only valid for the channel it belongs to.
static TOKEN: Mutex<Option<(String, String, Instant)>> = Mutex::new(None);

fn token_file() -> std::path::PathBuf {
    crate::settings::channel_dir().join("youtube_token.json")
}

fn client(s: &Settings) -> Result<(&str, &str)> {
    let (id, secret) = (s.google_client_id.trim(), s.google_client_secret.trim());
    if id.is_empty() || secret.is_empty() {
        bail!("add your Google OAuth client ID and secret in Settings first");
    }
    Ok((id, secret))
}

pub fn is_connected() -> bool {
    token_file().exists()
}

pub fn disconnect() -> Result<()> {
    *TOKEN.lock().unwrap() = None;
    if token_file().exists() {
        fs::remove_file(token_file())?;
    }
    Ok(())
}

fn access_token(s: &Settings) -> Result<String> {
    if let Some((chan, tok, at)) = TOKEN.lock().unwrap().as_ref() {
        if *chan == crate::settings::active_channel() && at.elapsed() < Duration::from_secs(45 * 60) {
            return Ok(tok.clone());
        }
    }
    let saved: Value = serde_json::from_str(&fs::read_to_string(token_file()).context("connect your YouTube channel in Settings first")?)?;
    let refresh = saved["refresh_token"].as_str().context("saved YouTube login is invalid; reconnect in Settings")?;
    let (id, secret) = client(s)?;
    let tok: Value = ureq::post(TOKEN_URI)
        .send_form(&[("client_id", id), ("client_secret", secret), ("refresh_token", refresh), ("grant_type", "refresh_token")])
        .map_err(http_err)
        .context("YouTube login expired; reconnect in Settings")?
        .into_json()?;
    let access = tok["access_token"].as_str().context("no access_token from Google")?.to_string();
    *TOKEN.lock().unwrap() = Some((crate::settings::active_channel(), access.clone(), Instant::now()));
    Ok(access)
}

/// Browser sign-in with a loopback redirect, state check and PKCE. Returns the channel name.
pub fn connect(s: &Settings) -> Result<String> {
    let (id, secret) = client(s)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let (state, verifier) = (random_hex(), random_hex() + &random_hex());
    let challenge = {
        use base64::Engine;
        use sha2::Digest;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()))
    };
    let url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent%20select_account&state={state}&code_challenge={challenge}&code_challenge_method=S256",
        enc(id), enc(&redirect), enc(SCOPES)
    );
    open_url(&url);

    listener.set_nonblocking(true)?; // poll so an abandoned sign-in times out instead of hanging
    let started = Instant::now();
    let code = loop {
        let mut stream = match listener.accept() {
            Ok((st, _)) => st,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() > Duration::from_secs(300) {
                    bail!("sign-in timed out after 5 minutes");
                }
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            continue; // browser preconnect that never sends a request
        }
        let query = line.split_whitespace().nth(1).and_then(|p| p.split_once('?')).map(|q| q.1).unwrap_or("");
        let param = |k: &str| query.split('&').find_map(|kv| kv.split_once('=').filter(|(a, _)| *a == k).map(|(_, v)| dec(v)));
        // Ignore favicon requests and anything not carrying our state (stray local requests can't cancel sign-in).
        if param("state").as_deref() != Some(state.as_str()) {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            continue;
        }
        let page = "<html><body style='font-family:system-ui;background:#0b0b10;color:#fff;display:grid;place-items:center;height:100vh'><h2>Connected. You can close this tab and return to AIYT Studio.</h2></body></html>";
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{page}", page.len());
        if let Some(err) = param("error") {
            bail!("sign-in cancelled: {err}");
        }
        break param("code").context("Google sent no sign-in code")?;
    };

    let tok: Value = ureq::post(TOKEN_URI)
        .send_form(&[("code", &code), ("client_id", id), ("client_secret", secret), ("redirect_uri", &redirect), ("grant_type", "authorization_code"), ("code_verifier", &verifier)])
        .map_err(http_err)?
        .into_json()?;
    let refresh = tok["refresh_token"].as_str().context("Google did not return a refresh token")?;
    let access = tok["access_token"].as_str().context("no access_token from Google")?.to_string();
    // Whichever channel was picked on Google's screen becomes a linked channel and the active one.
    let (chan_id, title) = register_channel(&access)?;
    crate::settings::write_atomic(&token_file(), &json!({"refresh_token": refresh}).to_string())?;
    *TOKEN.lock().unwrap() = Some((chan_id, access, Instant::now()));
    Ok(title)
}

fn get(s: &Settings, path: &str, query: &[(&str, &str)]) -> Result<Value> {
    let call = || {
        let mut req = ureq::get(&format!("{API}/{path}")).set("Authorization", &format!("Bearer {}", access_token(s)?));
        for (k, v) in query {
            req = req.query(k, v);
        }
        req.call().map_err(http_err)
    };
    let resp = match call() {
        // Cached token revoked or expired early: drop it and refresh once.
        Err(e) if e.to_string().starts_with("HTTP 401") => {
            *TOKEN.lock().unwrap() = None;
            call()?
        }
        r => r?,
    };
    Ok(resp.into_json()?)
}

/// Authenticated JSON request (POST/PUT) with the same one-shot 401 refresh.
fn send(s: &Settings, method: &str, url: &str, body: &Value) -> Result<Value> {
    let call = || ureq::request(method, url).set("Authorization", &format!("Bearer {}", access_token(s)?)).send_json(body).map_err(http_err);
    let resp = match call() {
        Err(e) if e.to_string().starts_with("HTTP 401") => {
            *TOKEN.lock().unwrap() = None;
            call()?
        }
        r => r?,
    };
    Ok(resp.into_json().unwrap_or(Value::Null))
}

/// (channel id, channel title)
pub fn channel(s: &Settings) -> Result<(String, String)> {
    let v = get(s, "channels", &[("part", "snippet"), ("mine", "true")])?;
    let c = &v["items"][0];
    Ok((c["id"].as_str().context("this Google account has no YouTube channel")?.into(), c["snippet"]["title"].as_str().unwrap_or("").into()))
}

/// Uploads `short.mp4` with metadata from `script.json`. `publish_at` (RFC 3339) schedules it: YouTube keeps it
/// private and publishes it at that time, so this PC does not need to be on.
pub fn upload(s: &Settings, dir: &Path, privacy: &str, publish_at: Option<&str>) -> Result<String> {
    if !["private", "unlisted", "public"].contains(&privacy) {
        bail!("privacy must be private, unlisted or public");
    }
    let bytes = fs::read(dir.join("short.mp4")).context("this short has no video yet")?;
    let script: Value = serde_json::from_str(&fs::read_to_string(dir.join("script.json"))?)?;

    let title: String = script["title"].as_str().unwrap_or("New short").chars().filter(|c| *c != '<' && *c != '>').take(100).collect();
    let mut description = script["description"].as_str().unwrap_or("").replace(['<', '>'], "");
    // Licence credit for the background music (CC BY requires it).
    if let Ok(credit) = fs::read_to_string(dir.join("music_credit.txt")) {
        if !credit.trim().is_empty() && !description.contains(credit.trim()) {
            description = format!("{}\n\n{}", description.trim_end(), credit.trim());
        }
    }
    if !description.to_lowercase().contains("#shorts") {
        description += "\n\n#Shorts";
    }
    let tags = fit_tags(script["tags"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default());
    // AI voice + AI visuals: YouTube asks creators to disclose realistic synthetic media.
    let mut status = json!({"privacyStatus": privacy, "selfDeclaredMadeForKids": false, "containsSyntheticMedia": true});
    if let Some(at) = publish_at.filter(|a| !a.is_empty()) {
        status = json!({"privacyStatus": "private", "publishAt": at, "selfDeclaredMadeForKids": false, "containsSyntheticMedia": true});
    }
    let meta = json!({"snippet": {"title": title, "description": description, "tags": tags, "categoryId": "24"}, "status": status});

    let auth = format!("Bearer {}", access_token(s)?);
    let start = ureq::post("https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status")
        .set("Authorization", &auth)
        .set("X-Upload-Content-Type", "video/mp4")
        .set("X-Upload-Content-Length", &bytes.len().to_string())
        .send_json(meta)
        .map_err(http_err)?;
    let session = start.header("Location").context("YouTube did not return an upload URL")?.to_string();
    let done: Value = ureq::put(&session).set("Authorization", &auth).set("Content-Type", "video/mp4").send_bytes(&bytes).map_err(http_err)?.into_json()?;
    let id = done["id"].as_str().context("YouTube returned no video id")?.to_string();

    if let Ok(jpg) = fs::read(dir.join("thumbnail.jpg")) {
        // Needs a phone-verified channel; harmless if it fails.
        let _ = ureq::post(&format!("https://www.googleapis.com/upload/youtube/v3/thumbnails/set?videoId={id}"))
            .set("Authorization", &auth)
            .set("Content-Type", "image/jpeg")
            .send_bytes(&jpg);
    }
    Ok(id)
}

/// YouTube allows 500 characters of tags in total (tags with spaces count their quotes).
fn fit_tags(tags: Vec<&str>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut total = 0;
    let mut out = vec![];
    for t in tags {
        let t: String = t.trim().trim_start_matches('#').chars().filter(|c| *c != '<' && *c != '>' && *c != ',').collect();
        let cost = t.chars().count() + if t.contains(' ') { 2 } else { 0 } + 1;
        if t.is_empty() || !seen.insert(t.to_lowercase()) || total + cost > 500 {
            continue;
        }
        total += cost;
        out.push(t);
    }
    out
}

/// Top Shorts for a niche over the last 30 days, plus the tags those winners use most.
pub fn trending(s: &Settings, niche: &str) -> Result<(Vec<Value>, Vec<Value>)> {
    let since = rfc3339(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() - 30 * 86400);
    let q = format!("{niche} #shorts");
    let found = get(s, "search", &[("part", "id"), ("type", "video"), ("videoDuration", "short"), ("order", "viewCount"), ("maxResults", "30"), ("publishedAfter", &since), ("q", &q)])?;
    let ids: Vec<&str> = found["items"].as_array().into_iter().flatten().filter_map(|i| i["id"]["videoId"].as_str()).collect();
    if ids.is_empty() {
        return Ok((vec![], vec![]));
    }
    let vids = get(s, "videos", &[("part", "snippet,statistics"), ("id", &ids.join(","))])?;
    let mut videos = vec![];
    let mut tags: HashMap<String, (String, u64, u64)> = HashMap::new(); // lower -> (display, count, views)
    for v in vids["items"].as_array().into_iter().flatten() {
        let views: u64 = v["statistics"]["viewCount"].as_str().and_then(|n| n.parse().ok()).unwrap_or(0);
        for t in v["snippet"]["tags"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            let e = tags.entry(t.to_lowercase()).or_insert((t.to_string(), 0, 0));
            e.1 += 1;
            e.2 += views;
        }
        videos.push(json!({
            "title": v["snippet"]["title"], "channel": v["snippet"]["channelTitle"], "views": views,
            "url": format!("https://youtube.com/shorts/{}", v["id"].as_str().unwrap_or(""))
        }));
    }
    videos.sort_by_key(|v| std::cmp::Reverse(v["views"].as_u64().unwrap_or(0)));
    let mut tags: Vec<_> = tags.into_values().collect();
    tags.sort_by(|a, b| (b.1 as f64 * ((b.2 + 1) as f64).ln()).total_cmp(&(a.1 as f64 * ((a.2 + 1) as f64).ln())));
    let tags = tags.into_iter().take(30).map(|(t, c, v)| json!({"tag": t, "count": c, "views": v})).collect();
    Ok((videos, tags))
}

/// Recent comments on your channel that you have not answered (or skipped) yet.
pub fn unanswered_comments(s: &Settings) -> Result<Vec<Value>> {
    let (me, _) = channel(s)?;
    let handled = handled_comments();
    let v = get(s, "commentThreads", &[("part", "snippet,replies"), ("allThreadsRelatedToChannelId", &me), ("maxResults", "50"), ("order", "time"), ("textFormat", "plainText")])?;
    let mut out = vec![];
    for t in v["items"].as_array().into_iter().flatten() {
        let top = &t["snippet"]["topLevelComment"]["snippet"];
        let id = t["id"].as_str().unwrap_or("");
        let mine = |c: &Value| c["authorChannelId"]["value"].as_str() == Some(me.as_str());
        let answered = t["replies"]["comments"].as_array().into_iter().flatten().any(|r| mine(&r["snippet"]));
        if id.is_empty() || mine(top) || answered || handled.iter().any(|h| h == id) {
            continue;
        }
        out.push(json!({"id": id, "video_id": t["snippet"]["videoId"], "author": top["authorDisplayName"], "text": top["textDisplay"], "published": top["publishedAt"]}));
    }
    // Attach video titles so the AI (and you) have context.
    let vids: Vec<String> = out.iter().filter_map(|c| c["video_id"].as_str().map(String::from)).collect::<std::collections::BTreeSet<_>>().into_iter().take(50).collect();
    if !vids.is_empty() {
        let titles = get(s, "videos", &[("part", "snippet"), ("id", &vids.join(","))])?;
        let map: HashMap<&str, &str> = titles["items"].as_array().into_iter().flatten().filter_map(|v| Some((v["id"].as_str()?, v["snippet"]["title"].as_str()?))).collect();
        for c in &mut out {
            let title = c["video_id"].as_str().and_then(|v| map.get(v)).copied().unwrap_or("");
            c["video_title"] = title.into();
        }
    }
    Ok(out)
}

pub fn reply(s: &Settings, thread_id: &str, text: &str) -> Result<()> {
    send(s, "POST", &format!("{API}/comments?part=snippet"), &json!({"snippet": {"parentId": thread_id, "textOriginal": text}}))?;
    let _ = mark_handled(thread_id); // the reply is posted; failing here must not invite a duplicate retry
    Ok(())
}

/// Creative Commons (CC BY) videos for a query: licensed for reuse, including commercially, with credit.
pub fn search_cc(s: &Settings, query: &str) -> Result<Vec<Value>> {
    Ok(search_cc_page(s, query, &ClipSearch::default())?.0)
}

/// Search options from the Clips page.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct ClipSearch {
    /// viewCount (default) | relevance | date | rating
    pub order: String,
    /// any | short (<4 min) | medium (4-20 min, default) | long (>20 min)
    pub duration: String,
    /// Prefer English: YouTube ranks English results first.
    pub english: bool,
    pub page: String,
}

/// One page (up to 50) of Creative Commons videos with full stats. Returns (videos, next page token).
pub fn search_cc_page(s: &Settings, query: &str, o: &ClipSearch) -> Result<(Vec<Value>, String)> {
    let order = if ["viewCount", "relevance", "date", "rating"].contains(&o.order.as_str()) { o.order.as_str() } else { "viewCount" };
    let duration = if ["any", "short", "medium", "long"].contains(&o.duration.as_str()) { o.duration.as_str() } else { "medium" };
    let mut q = vec![("part", "id"), ("type", "video"), ("videoLicense", "creativeCommon"), ("order", order), ("maxResults", "50"), ("q", query), ("videoDuration", duration)];
    if o.english {
        q.push(("relevanceLanguage", "en"));
    }
    if !o.page.is_empty() {
        q.push(("pageToken", o.page.as_str()));
    }
    let found = get(s, "search", &q)?;
    let next = found["nextPageToken"].as_str().unwrap_or("").to_string();
    let ids: Vec<&str> = found["items"].as_array().into_iter().flatten().filter_map(|i| i["id"]["videoId"].as_str()).collect();
    if ids.is_empty() {
        return Ok((vec![], next));
    }
    let vids = get(s, "videos", &[("part", "snippet,statistics,contentDetails,status"), ("id", &ids.join(","))])?;
    let num = |v: &Value| v.as_str().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    let items = vids["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["status"]["license"] == "creativeCommon") // double-check the license on the video itself
        .map(|v| {
            let id = v["id"].as_str().unwrap_or("");
            let sn = &v["snippet"];
            let lang = sn["defaultAudioLanguage"].as_str().or(sn["defaultLanguage"].as_str()).unwrap_or("").to_lowercase();
            let url = format!("https://www.youtube.com/watch?v={id}");
            json!({
                "source": "youtube", "id": id, "url": url, "title": sn["title"], "channel": sn["channelTitle"],
                "views": num(&v["statistics"]["viewCount"]), "likes": num(&v["statistics"]["likeCount"]), "comments": num(&v["statistics"]["commentCount"]),
                "duration": iso_duration(v["contentDetails"]["duration"].as_str().unwrap_or("")), "language": lang, "published": sn["publishedAt"],
                "thumb": format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"), "license": "CC BY",
                "used_secs": crate::ledger::used_seconds(&crate::ledger::source_key(&url))
            })
        })
        .collect();
    Ok((items, next))
}

/// Titles of your latest uploads (to understand what your channel is about).
pub fn my_titles(s: &Settings) -> Result<Vec<String>> {
    let ch = get(s, "channels", &[("part", "contentDetails"), ("mine", "true")])?;
    let uploads = ch["items"][0]["contentDetails"]["relatedPlaylists"]["uploads"].as_str().context("no uploads playlist")?.to_string();
    let items = get(s, "playlistItems", &[("part", "snippet"), ("playlistId", &uploads), ("maxResults", "25")])?;
    Ok(items["items"].as_array().into_iter().flatten().filter_map(|i| i["snippet"]["title"].as_str().map(String::from)).collect())
}

/// Updates channel description, keywords and (optionally) banner. Name and profile picture are not settable through the API.
pub fn apply_branding(s: &Settings, description: &str, keywords: &str, banner: Option<&Path>) -> Result<()> {
    let (id, _) = channel(s)?;
    let current = get(s, "channels", &[("part", "brandingSettings"), ("id", &id)])?;
    // brandingSettings is replaced as a whole on update, so start from the current one.
    let mut b = current["items"][0]["brandingSettings"].clone();
    b["channel"]["description"] = description.chars().take(1000).collect::<String>().into();
    b["channel"]["keywords"] = keywords.chars().take(500).collect::<String>().into();
    if let Some(path) = banner {
        let jpg = fs::read(path)?;
        let r: Value = ureq::post("https://www.googleapis.com/upload/youtube/v3/channelBanners/insert")
            .set("Authorization", &format!("Bearer {}", access_token(s)?))
            .set("Content-Type", "image/jpeg")
            .send_bytes(&jpg)
            .map_err(http_err)?
            .into_json()?;
        b["image"] = json!({"bannerExternalUrl": r["url"].as_str().context("banner upload returned no url")?});
    }
    send(s, "PUT", &format!("{API}/channels?part=brandingSettings"), &json!({"id": id, "brandingSettings": b}))?;
    Ok(())
}

fn iso_duration(d: &str) -> u64 {
    let (mut total, mut n) = (0, 0);
    for c in d.chars() {
        match c {
            '0'..='9' => n = n * 10 + c.to_digit(10).unwrap() as u64,
            'H' => (total, n) = (total + n * 3600, 0),
            'M' => (total, n) = (total + n * 60, 0),
            'S' => (total, n) = (total + n, 0),
            _ => n = 0,
        }
    }
    total
}

/// "YYYY-MM-DDTHH:MM:SS(.fff)Z" -> Unix seconds (days-from-civil, H. Hinnant).
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + hh * 3600 + mm * 60 + ss).ok()
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn to_rfc3339(secs: u64) -> String {
    rfc3339(secs)
}

fn handled_comments() -> Vec<String> {
    fs::read_to_string(crate::settings::channel_dir().join("handled_comments.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn mark_handled(thread_id: &str) -> Result<()> {
    let mut h = handled_comments();
    h.push(thread_id.to_string());
    let skip = h.len().saturating_sub(5000); // keep the file small
    fs::write(crate::settings::channel_dir().join("handled_comments.json"), serde_json::to_string(&h[skip..])?)?;
    Ok(())
}

pub fn open_url(url: &str) {
    let mut c = if cfg!(windows) {
        let mut c = crate::llm::command("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else {
        let mut c = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" });
        c.arg(url);
        c
    };
    let _ = c.spawn();
}

/// 128 bits from the OS CSPRNG, hex encoded.
fn random_hex() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS random number generator unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let hex = b.get(i + 1..i + 3).filter(|h| h.iter().all(u8::is_ascii_hexdigit));
        match (b[i], hex) {
            (b'%', Some(h)) => {
                out.push(u8::from_str_radix(std::str::from_utf8(h).unwrap(), 16).unwrap());
                i += 3;
            }
            (b'+', _) => { out.push(b' '); i += 1; }
            (c, _) => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into()
}

/// Unix seconds -> "YYYY-MM-DDTHH:MM:SSZ" (civil-from-days, H. Hinnant).
fn rfc3339(secs: u64) -> String {
    let z = (secs / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let r = secs % 86400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", r / 3600, r / 60 % 60, r % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(enc("http://127.0.0.1:80"), "http%3A%2F%2F127.0.0.1%3A80");
        assert_eq!(dec("4%2F0Ab+x%zz%4"), "4/0Ab x%zz%4");
        assert_eq!(dec(&enc("a b/ü")), "a b/ü");
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        for t in [0, 951_782_400, 1_700_000_000, 1_795_000_123] {
            assert_eq!(parse_rfc3339(&rfc3339(t)), Some(t));
        }
        assert_eq!(parse_rfc3339("2026-11-27T16:00:00.000Z"), Some(1_795_795_200));
        assert_eq!(parse_rfc3339("garbage"), None);
        assert_eq!((iso_duration("PT1H2M3S"), iso_duration("PT4M"), iso_duration("P0D")), (3723, 240, 0));
        assert_eq!(random_hex().len(), 32);
        let long = "x".repeat(120);
        let t = fit_tags(vec!["#Cats", "cats", "cute cats", &long, &long, &long, &long, &long]);
        assert_eq!(t[..2], ["Cats".to_string(), "cute cats".to_string()]);
        assert!(t.iter().map(|t| t.len() + 3).sum::<usize>() <= 500);
    }
}

/// (channel title, channel description) of the signed-in channel.
pub fn channel_profile(s: &Settings) -> Result<(String, String)> {
    let v = get(s, "channels", &[("part", "snippet"), ("mine", "true")])?;
    let sn = &v["items"][0]["snippet"];
    Ok((sn["title"].as_str().unwrap_or("").into(), sn["description"].as_str().unwrap_or("").into()))
}

/// Posts a top-level comment from the channel on one of its videos. Returns the comment thread id.
/// (The YouTube API cannot pin comments; the app links to YouTube Studio for that one click.)
pub fn post_comment(s: &Settings, video_id: &str, text: &str) -> Result<String> {
    let body = json!({"snippet": {"videoId": video_id, "topLevelComment": {"snippet": {"textOriginal": text}}}});
    let v = send(s, "POST", &format!("{API}/commentThreads?part=snippet"), &body)?;
    Ok(v["id"].as_str().unwrap_or_default().to_string())
}

/// Channel totals, the latest 50 uploads with their stats, and a daily history of subs/views (kept locally).
pub fn analytics(s: &Settings) -> Result<Value> {
    let ch = get(s, "channels", &[("part", "snippet,statistics,contentDetails"), ("mine", "true")])?;
    let c = &ch["items"][0];
    let num = |v: &Value| v.as_str().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    let (subs, views, videos) = (num(&c["statistics"]["subscriberCount"]), num(&c["statistics"]["viewCount"]), num(&c["statistics"]["videoCount"]));

    let mut list = vec![];
    if let Some(uploads) = c["contentDetails"]["relatedPlaylists"]["uploads"].as_str() {
        let items = get(s, "playlistItems", &[("part", "contentDetails"), ("playlistId", uploads), ("maxResults", "50")])?;
        let ids: Vec<&str> = items["items"].as_array().into_iter().flatten().filter_map(|i| i["contentDetails"]["videoId"].as_str()).collect();
        if !ids.is_empty() {
            let vids = get(s, "videos", &[("part", "snippet,statistics,contentDetails,status"), ("id", &ids.join(","))])?;
            for v in vids["items"].as_array().into_iter().flatten() {
                list.push(json!({
                    "id": v["id"], "title": v["snippet"]["title"], "published": v["snippet"]["publishedAt"],
                    "privacy": v["status"]["privacyStatus"], "publish_at": v["status"]["publishAt"],
                    "views": num(&v["statistics"]["viewCount"]), "likes": num(&v["statistics"]["likeCount"]),
                    "comments": num(&v["statistics"]["commentCount"]), "duration": iso_duration(v["contentDetails"]["duration"].as_str().unwrap_or("")),
                    "thumb": format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", v["id"].as_str().unwrap_or(""))
                }));
            }
        }
    }

    // One snapshot per day so growth can be shown over time.
    let path = crate::settings::channel_dir().join("analytics_history.json");
    let mut hist: Vec<Value> = fs::read_to_string(&path).ok().and_then(|x| serde_json::from_str(&x).ok()).unwrap_or_default();
    let day = now_secs() / 86400;
    hist.retain(|h| h["day"].as_u64() != Some(day));
    hist.push(json!({"day": day, "subs": subs, "views": views}));
    let skip = hist.len().saturating_sub(400);
    let hist = hist[skip..].to_vec();
    let _ = crate::settings::write_atomic(&path, &serde_json::to_string(&hist)?);

    Ok(json!({"title": c["snippet"]["title"], "subs": subs, "views": views, "videos": videos,
              "hidden_subs": c["statistics"]["hiddenSubscriberCount"], "list": list, "history": hist}))
}

/// YouTube's own "trending now" chart for a country (optionally one category), Shorts-length first. 1 quota unit.
pub fn trending_chart(s: &Settings, region: &str, category: &str) -> Result<Vec<Value>> {
    let mut q = vec![("part", "snippet,statistics,contentDetails"), ("chart", "mostPopular"), ("regionCode", region), ("maxResults", "50")];
    if !category.is_empty() {
        q.push(("videoCategoryId", category));
    }
    let v = get(s, "videos", &q)?;
    let num = |x: &Value| x.as_str().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    Ok(v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| {
            let id = v["id"].as_str().unwrap_or("");
            json!({"id": id, "url": format!("https://www.youtube.com/watch?v={id}"), "title": v["snippet"]["title"], "channel": v["snippet"]["channelTitle"],
                   "views": num(&v["statistics"]["viewCount"]), "likes": num(&v["statistics"]["likeCount"]), "comments": num(&v["statistics"]["commentCount"]),
                   "duration": iso_duration(v["contentDetails"]["duration"].as_str().unwrap_or("")), "published": v["snippet"]["publishedAt"],
                   "thumb": format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg")})
        })
        .collect())
}

/// YouTube Studio-style numbers from the YouTube Analytics API for the last `days` days:
/// totals, per-video retention, and where views come from.
pub fn studio_report(s: &Settings, days: u64) -> Result<Value> {
    let end = now_secs() - 86400; // Analytics data lags ~1-2 days
    let (start_d, end_d) = (rfc3339(end - days * 86400)[..10].to_string(), rfc3339(end)[..10].to_string());
    let report = |metrics: &str, dims: &str, extra: &[(&str, &str)]| -> Result<Value> {
        let token = access_token(s)?;
        let mut r = ureq::get("https://youtubeanalytics.googleapis.com/v2/reports")
            .set("Authorization", &format!("Bearer {token}"))
            .query("ids", "channel==MINE")
            .query("startDate", &start_d)
            .query("endDate", &end_d)
            .query("metrics", metrics);
        if !dims.is_empty() {
            r = r.query("dimensions", dims);
        }
        for (k, v) in extra {
            r = r.query(k, v);
        }
        match r.call() {
            Ok(resp) => Ok(resp.into_json()?),
            Err(e) => {
                let msg = http_err(e).to_string();
                if msg.contains("insufficient") || msg.contains("ACCESS_TOKEN_SCOPE_INSUFFICIENT") {
                    bail!("Studio stats need one more permission: click Reconnect in Settings → YouTube channel");
                }
                if msg.contains("accessNotConfigured") || msg.contains("has not been used") || msg.contains("SERVICE_DISABLED") {
                    bail!("enable the \"YouTube Analytics API\" in your Google Cloud project (APIs & Services → Library), then try again");
                }
                bail!("{msg}")
            }
        }
    };
    // Rows come back as arrays in column order; turn them into objects.
    let rows = |v: &Value| -> Vec<Value> {
        let cols: Vec<String> = v["columnHeaders"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str().map(String::from)).collect();
        v["rows"].as_array().into_iter().flatten().map(|r| Value::Object(cols.iter().cloned().zip(r.as_array().cloned().unwrap_or_default()).collect())).collect()
    };
    let core = "views,estimatedMinutesWatched,averageViewDuration,averageViewPercentage,subscribersGained,subscribersLost,likes,shares,comments";
    let totals = rows(&report(core, "", &[])?).into_iter().next().unwrap_or_default();
    let daily = rows(&report("views,subscribersGained", "day", &[("sort", "day")])?);
    let mut videos = rows(&report("views,averageViewDuration,averageViewPercentage,likes,subscribersGained", "video", &[("sort", "-views"), ("maxResults", "25")])?);
    let traffic = rows(&report("views", "insightTrafficSourceType", &[("sort", "-views")])?);
    // Attach titles (one cheap Data API call).
    let ids: Vec<String> = videos.iter().filter_map(|v| v["video"].as_str().map(String::from)).collect();
    if !ids.is_empty() {
        let t = get(s, "videos", &[("part", "snippet"), ("id", &ids.join(","))])?;
        let titles: HashMap<&str, &str> = t["items"].as_array().into_iter().flatten().filter_map(|v| Some((v["id"].as_str()?, v["snippet"]["title"].as_str()?))).collect();
        for v in &mut videos {
            let title = v["video"].as_str().and_then(|id| titles.get(id)).copied().unwrap_or("");
            v["title"] = title.into();
        }
    }
    Ok(json!({"start": start_d, "end": end_d, "totals": totals, "daily": daily, "videos": videos, "traffic": traffic}))
}

/// Top comments on one video (plain text), for the AI to read what viewers are asking.
pub fn video_comments(s: &Settings, video_id: &str, max: u32) -> Result<Vec<String>> {
    let v = get(s, "commentThreads", &[("part", "snippet"), ("videoId", video_id), ("maxResults", &max.to_string()), ("order", "relevance"), ("textFormat", "plainText")])?;
    Ok(v["items"].as_array().into_iter().flatten().filter_map(|t| t["snippet"]["topLevelComment"]["snippet"]["textDisplay"].as_str().map(|c| format!("- {}", c.chars().take(200).collect::<String>()))).collect())
}

/// One YouTube Analytics API report as row objects (needs the yt-analytics.readonly permission).
fn analytics_rows(s: &Settings, start: &str, end: &str, metrics: &str, dims: &str) -> Result<Vec<Value>> {
    let mut r = ureq::get("https://youtubeanalytics.googleapis.com/v2/reports")
        .set("Authorization", &format!("Bearer {}", access_token(s)?))
        .query("ids", "channel==MINE")
        .query("startDate", start)
        .query("endDate", end)
        .query("metrics", metrics);
    if !dims.is_empty() {
        r = r.query("dimensions", dims);
    }
    let v: Value = r.call().map_err(http_err)?.into_json()?;
    let cols: Vec<String> = v["columnHeaders"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str().map(String::from)).collect();
    Ok(v["rows"].as_array().into_iter().flatten().map(|r| Value::Object(cols.iter().cloned().zip(r.as_array().cloned().unwrap_or_default()).collect())).collect())
}

/// Progress towards the YouTube Partner Program: subscribers, Shorts views (90 days), long-form watch hours (365 days),
/// uploads (90 days), plus growth rates from the app's daily snapshots for "when will we get there".
pub fn monetization(s: &Settings) -> Result<Value> {
    let a = analytics(s)?;
    let now = now_secs();
    let day = |ago: u64| rfc3339(now.saturating_sub(86400 + ago * 86400))[..10].to_string();
    let list = a["list"].as_array().cloned().unwrap_or_default();
    let published_within = |v: &Value, days: u64| v["published"].as_str().and_then(parse_rfc3339).is_some_and(|t| now.saturating_sub(t) <= days * 86400);
    let uploads_90 = list.iter().filter(|v| published_within(v, 90) && v["privacy"] != "private").count();

    // Exact numbers from YouTube Analytics when allowed; otherwise an estimate from recent uploads.
    let by_type = |days: u64, metric: &str| -> Option<HashMap<String, f64>> {
        analytics_rows(s, &day(days), &day(0), metric, "creatorContentType")
            .ok()
            .map(|rows| rows.iter().filter_map(|r| Some((r["creatorContentType"].as_str()?.to_string(), r[metric].as_f64()?))).collect())
    };
    let (shorts_views, watch_hours, exact) = match (by_type(90, "views"), by_type(365, "estimatedMinutesWatched")) {
        (Some(v), Some(m)) => (v.get("SHORTS").copied().unwrap_or(0.0), m.get("VIDEO_ON_DEMAND").copied().unwrap_or(0.0) / 60.0, true),
        _ => {
            let shorts: f64 = list.iter().filter(|v| published_within(v, 90) && v["duration"].as_u64().unwrap_or(0) <= 180).map(|v| v["views"].as_f64().unwrap_or(0.0)).sum();
            (shorts, 0.0, false)
        }
    };

    // Growth per day from the daily snapshots (needs at least two days of history).
    let hist = a["history"].as_array().cloned().unwrap_or_default();
    let rate = |key: &str| -> Option<f64> {
        let (first, last) = (hist.first()?, hist.last()?);
        let days = (last["day"].as_u64()? as f64 - first["day"].as_u64()? as f64).max(0.0);
        (days >= 1.0).then(|| (last[key].as_f64().unwrap_or(0.0) - first[key].as_f64().unwrap_or(0.0)) / days)
    };
    let likes: u64 = list.iter().map(|v| v["likes"].as_u64().unwrap_or(0)).sum();
    Ok(json!({
        // The channel total lags behind per-video counts; use whichever is higher.
        "subs": a["subs"], "views": a["views"].as_u64().unwrap_or(0).max(list.iter().map(|v| v["views"].as_u64().unwrap_or(0)).sum::<u64>()), "likes": likes, "uploads_90": uploads_90,
        "shorts_views_90": shorts_views.round(), "watch_hours_365": watch_hours.round(), "exact": exact,
        "subs_per_day": rate("subs"), "views_per_day": rate("views"), "history_days": hist.len(),
        "tiers": [
            {"name": "Fan funding tier", "unlocks": "Channel memberships, Super Thanks, Super Chat, Shopping",
             "subs": 500, "uploads": 3, "shorts_views": 3_000_000, "watch_hours": 3000},
            {"name": "Full Partner Program", "unlocks": "Ad revenue and Shorts Feed revenue sharing",
             "subs": 1000, "uploads": 0, "shorts_views": 10_000_000, "watch_hours": 4000}
        ]
    }))
}

/// uploadStatus / failure / rejection / privacy for videos (50 per call, 1 quota unit each call).
pub fn processing_status(s: &Settings, ids: &[String]) -> Result<Value> {
    let mut out = serde_json::Map::new();
    for chunk in ids.chunks(50) {
        let v = get(s, "videos", &[("part", "status,processingDetails"), ("id", &chunk.join(","))])?;
        for item in v["items"].as_array().into_iter().flatten() {
            let st = &item["status"];
            let state = match st["uploadStatus"].as_str().unwrap_or("") {
                "processed" => "live",
                "uploaded" => "processing",
                "failed" => "failed",
                "rejected" => "rejected",
                "deleted" => "deleted",
                other => other,
            };
            let reason = st["failureReason"].as_str().or(st["rejectionReason"].as_str()).unwrap_or("");
            out.insert(item["id"].as_str().unwrap_or("").into(), json!({"state": state, "reason": reason, "privacy": st["privacyStatus"],
                "progress": item["processingDetails"]["processingStatus"]}));
        }
        // Videos YouTube no longer returns were removed on YouTube.
        for id in chunk {
            out.entry(id.clone()).or_insert(json!({"state": "missing", "reason": "not found on YouTube (deleted?)"}));
        }
    }
    Ok(Value::Object(out))
}

/// Look up the channel behind an access token, add it to channels.json and make it the active channel.
fn register_channel(access: &str) -> Result<(String, String)> {
    let v: Value = ureq::get(&format!("{API}/channels"))
        .set("Authorization", &format!("Bearer {access}"))
        .query("part", "snippet")
        .query("mine", "true")
        .call()
        .map_err(http_err)?
        .into_json()?;
    let c = &v["items"][0];
    let id = c["id"].as_str().context("this Google account has no YouTube channel; create one on YouTube first")?.to_string();
    let title = c["snippet"]["title"].as_str().unwrap_or("").to_string();
    let thumb = c["snippet"]["thumbnails"]["default"]["url"].as_str().unwrap_or("").to_string();
    let mut list = crate::settings::channels();
    list.retain(|e| e.id != id);
    list.push(crate::settings::ChannelEntry { id: id.clone(), title: title.clone(), thumb });
    crate::settings::save_channels(&list)?;
    crate::settings::set_active_channel(&id)?;
    Ok((id, title))
}

/// One-time move from the single-channel layout: the old login, stats and settings become that channel's profile.
pub fn migrate_legacy() -> Result<()> {
    let legacy = config_dir().join("youtube_token.json");
    if !crate::settings::channels().is_empty() || !legacy.exists() {
        return Ok(());
    }
    let s = Settings::load(); // no active channel yet: these are the old single-channel settings
    let saved: Value = serde_json::from_str(&fs::read_to_string(&legacy)?)?;
    let refresh = saved["refresh_token"].as_str().context("old login is invalid")?;
    let (cid, secret) = client(&s)?;
    let tok: Value = ureq::post(TOKEN_URI)
        .send_form(&[("client_id", cid), ("client_secret", secret), ("refresh_token", refresh), ("grant_type", "refresh_token")])
        .map_err(http_err)?
        .into_json()?;
    let access = tok["access_token"].as_str().context("old login expired")?;
    let profile = crate::settings::ChannelProfile::from(&s);
    let (id, _) = register_channel(access)?;
    let dir = crate::settings::channel_dir();
    for f in ["youtube_token.json", "video_stats.json", "smart_picks.json", "auto_state.json", "analytics_history.json", "handled_comments.json", "cta_tuned_day.txt"] {
        if config_dir().join(f).exists() {
            fs::rename(config_dir().join(f), dir.join(f))?;
        }
    }
    crate::settings::write_atomic(&dir.join("profile.json"), &serde_json::to_string_pretty(&profile)?)?;
    // Existing shorts belong to that channel.
    for j in crate::pipeline::list_jobs_all() {
        if let Some(jid) = j["id"].as_str() {
            if let Ok(mut st) = crate::pipeline::load_state(jid) {
                if st.channel_id.is_empty() {
                    st.channel_id = id.clone();
                    let _ = crate::pipeline::save_state(jid, &st);
                }
            }
        }
    }
    Ok(())
}

/// Creates a public playlist (one per content pillar on a new channel). Returns its id.
pub fn create_playlist(s: &Settings, title: &str, description: &str) -> Result<String> {
    let body = json!({"snippet": {"title": title.chars().take(150).collect::<String>(), "description": description.chars().take(5000).collect::<String>()}, "status": {"privacyStatus": "public"}});
    let v = send(s, "POST", &format!("{API}/playlists?part=snippet,status"), &body)?;
    Ok(v["id"].as_str().unwrap_or_default().to_string())
}
