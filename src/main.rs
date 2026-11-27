#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auto;
mod growth;
mod ledger;
mod llm;
mod media;
mod pipeline;
mod settings;
mod stock;
mod tools;
mod trends;
mod twitch;
mod usage;
mod watch;
mod youtube;

use serde::Deserialize;
use serde_json::{json, Value};
use settings::{out_dir, Settings};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager};

type Res<T> = Result<T, String>;

/// Run blocking work (HTTP, ffmpeg, CLIs) off the UI thread.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Res<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?.map_err(|e| format!("{e:#}"))
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

/// Only one generate/upload/autopilot task at a time.
static BUSY: AtomicBool = AtomicBool::new(false);
struct Busy;
impl Busy {
    fn take() -> anyhow::Result<Busy> {
        llm::CANCEL.store(false, Ordering::SeqCst); // a new task starts un-cancelled
        if BUSY.swap(true, Ordering::SeqCst) {
            anyhow::bail!("another task is still running; wait for it to finish");
        }
        Ok(Busy)
    }
}
impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::SeqCst);
    }
}

/// Long AI commands that are not queue tasks still start un-cancelled.
fn fresh() {
    // Never un-cancel a short-making job that the user is cancelling right now.
    if !BUSY.load(Ordering::SeqCst) {
        llm::CANCEL.store(false, Ordering::SeqCst);
    }
}

/// Road to monetization: Partner Program progress and growth rates.
#[tauri::command]
async fn monetization() -> Res<Value> {
    fresh();
    blocking(|| youtube::monetization(&Settings::load())).await
}

/// Cancel button: stops the running task right away (kills ffmpeg, yt-dlp or the AI app mid-step).
#[tauri::command]
fn cancel_task() {
    llm::CANCEL.store(true, Ordering::SeqCst);
}

fn logger(app: AppHandle) -> impl Fn(&str) {
    move |m: &str| {
        let _ = app.emit("log", m.to_string());
    }
}

fn on_path(bin: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|d| ["", ".exe", ".cmd"].iter().any(|ext| d.join(format!("{bin}{ext}")).is_file()))
}

#[tauri::command]
fn get_settings() -> Settings {
    Settings::load()
}

#[tauri::command]
fn save_settings(settings: Settings) -> Res<()> {
    settings.save().map_err(err)
}

#[tauri::command]
fn app_info() -> Value {
    json!({
        "out_dir": out_dir(),
        "connected": youtube::is_connected(),
        "busy": BUSY.load(Ordering::SeqCst),
        "tools": {
            "ffmpeg": on_path("ffmpeg"), "claude": on_path("claude"), "codex": on_path("codex"),
            "opencode": on_path("opencode"), "ollama": on_path("ollama")
        }
    })
}

#[tauri::command]
fn open_link(url: String) -> Res<()> {
    if !url.starts_with("https://") {
        return Err("only https links can be opened".into());
    }
    youtube::open_url(&url);
    Ok(())
}

#[tauri::command]
async fn yt_channel() -> Res<String> {
    blocking(|| Ok(youtube::channel(&Settings::load())?.1)).await
}

#[tauri::command]
async fn yt_connect() -> Res<String> {
    blocking(|| youtube::connect(&Settings::load())).await
}

#[tauri::command]
/// Unlink the active channel (its shorts stay on disk) and switch to another linked channel, if any.
fn yt_disconnect() -> Res<()> {
    let active = settings::active_channel();
    youtube::disconnect().map_err(err)?;
    let mut list = settings::channels();
    list.retain(|c| c.id != active);
    settings::save_channels(&list).map_err(err)?;
    settings::set_active_channel(list.first().map(|c| c.id.as_str()).unwrap_or("")).map_err(err)
}

#[tauri::command]
fn list_channels() -> Value {
    json!({"channels": settings::channels(), "active": settings::active_channel()})
}

/// Work on another linked channel: its login, profile, shorts, stats and automation take over.
#[tauri::command]
fn switch_channel(app: AppHandle, id: String) -> Res<()> {
    if !settings::channels().iter().any(|c| c.id == id) {
        return Err("that channel is not linked".into());
    }
    if BUSY.load(Ordering::SeqCst) {
        return Err("wait for the current shorts to finish (or Cancel) before switching channels".into());
    }
    settings::set_active_channel(&id).map_err(err)?;
    let _ = app.emit("channel-changed", ());
    Ok(())
}

#[tauri::command]
async fn suggest_niches() -> Res<Value> {
    fresh();
    blocking(|| pipeline::suggest_niches(&Settings::load())).await
}

fn research_with_data(s: &Settings, niche: &str, count: usize, log: &dyn Fn(&str)) -> anyhow::Result<Value> {
    let (videos, tags) = if youtube::is_connected() {
        log("Pulling this month's top Shorts for the niche…");
        youtube::trending(s, niche).unwrap_or_else(|e| {
            log(&format!("  live data unavailable: {e:#}"));
            (vec![], vec![])
        })
    } else {
        log("YouTube not connected: researching without live data");
        (vec![], vec![])
    };
    log("AI is analysing the niche…");
    pipeline::research(s, niche, count, &videos, &tags)
}

#[tauri::command]
async fn research(app: AppHandle, niche: String, count: usize) -> Res<Value> {
    fresh();
    blocking(move || research_with_data(&Settings::load(), &niche, count.clamp(1, 30), &logger(app))).await
}

#[tauri::command]
async fn generate(app: AppHandle, topics: Vec<String>, tags: Vec<String>) -> Res<Vec<String>> {
    blocking(move || {
        let _busy = Busy::take()?;
        let (s, log) = (Settings::load(), logger(app.clone()));
        // Queue everything first so the Library shows the whole batch, then work through it.
        let ids = fresh_topics(&topics, &log).iter().map(|t| pipeline::create_job(t, &tags)).collect::<anyhow::Result<Vec<_>>>()?;
        let _ = app.emit("jobs-changed", ());
        run_queue(&app, &s, "script", &ids, &log, |_, _| Ok(()));
        Ok(ids)
    })
    .await
}

/// Drops empty topics, repeats within the batch, and topics already published on the channel.
fn fresh_topics(topics: &[String], log: &dyn Fn(&str)) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    topics
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && seen.insert(ledger::topic_key(t)))
        .filter(|t| match ledger::published_conflict("", &[(ledger::topic_key(t), 0.0, 0.0)]) {
            Some(_) => {
                log(&format!("  skipped “{t}”: already published"));
                false
            }
            None => true,
        })
        .collect()
}

/// Makes a short sample with the current voice settings so you can hear it before generating.
#[tauri::command]
async fn preview_voice(app: AppHandle, voice: Option<String>) -> Res<String> {
    fresh();
    blocking(move || {
        let mut s = Settings::load();
        if let Some(v) = voice.filter(|v| !v.is_empty()) {
            s.voice = v; // preview a specific engine from Settings
        }
        let dir = out_dir().join("_preview");
        std::fs::create_dir_all(&dir)?;
        let _ = std::fs::remove_file(dir.join("voice.wav"));
        std::fs::write(dir.join("narration.txt"), "This is how your shorts will sound. Stick around, because the ending is the best part!")?;
        media::voice(&s, &dir, &logger(app))?;
        Ok(dir.join("voice.wav").to_string_lossy().to_string())
    })
    .await
}

#[tauri::command]
fn kokoro_voices() -> Vec<&'static str> {
    tools::KOKORO_VOICES.to_vec()
}

#[tauri::command]
fn piper_voices() -> Vec<&'static str> {
    tools::PIPER_VOICES.to_vec()
}

/// GPU status for Settings: NVIDIA card present? NVIDIA runtime files present?
#[tauri::command]
fn gpu_status() -> Value {
    json!({"nvidia": on_path("nvidia-smi"), "cuda": tools::cuda_runtime().is_some()})
}

/// Downloads NVIDIA's CUDA 12 + cuDNN 9 runtime (~2.6 GB) and the GPU voice engine, so local voices use the GPU.
#[tauri::command]
async fn setup_gpu_voice(app: AppHandle) -> Res<()> {
    blocking(move || {
        // Runs alongside everything else (it is just a download); only one GPU setup at a time.
        static SETUP: AtomicBool = AtomicBool::new(false);
        if SETUP.swap(true, Ordering::SeqCst) {
            anyhow::bail!("the GPU pack is already downloading");
        }
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                SETUP.store(false, Ordering::SeqCst);
            }
        }
        let _done = Done;
        let log = logger(app);
        if !on_path("nvidia-smi") {
            anyhow::bail!("no NVIDIA GPU driver found on this PC");
        }
        tools::tts_engine(&log, true)?;
        tools::setup_cuda(&log)
    })
    .await
}

/// One day of the daily automation: smart picks first, then AI trend picks (general + your niche + YouTube's
/// most popular), make them all, publish each at its time (right away if its time already passed).
fn run_auto_day(app: &AppHandle, day: i64) -> anyhow::Result<()> {
    let _busy = Busy::take()?;
    let (s, log) = (Settings::load(), logger(app.clone()));
    let plan = s.auto.clone();
    let n = plan.per_day.clamp(1, 20) as usize;
    auto::mark_started(day);
    let _ = app.emit("auto-changed", ());
    log(&format!("⚙ Daily automation, day {} of {}: finding {n} shorts…", day - plan.start_day + 1, plan.days));
    // 1) A short of yours is blowing up: its follow-up goes first.
    let smart: Vec<Value> = growth::picks().into_iter().filter(|p| p["status"] == "new").take(n).collect();
    let mut topics: Vec<String> = smart.iter().filter_map(|p| p["topic"].as_str().map(String::from)).collect();
    for p in &smart {
        let _ = growth::set_status(p["video_id"].as_str().unwrap_or(""), "made");
    }
    // 2) Fill the rest from what is trending in general and in your niche.
    if topics.len() < n {
        let google = trends::google_trends("US").unwrap_or_default();
        let chart = youtube::trending_chart(&s, "US", "").unwrap_or_default();
        let mut s2 = s.clone();
        let niche = trends::niche_query(&mut s2).and_then(|q| youtube::trending(&s2, &q).map(|(v, _)| v)).unwrap_or_default();
        let picks = trends::opportunities(&s, &google, &chart, &niche)?;
        topics.extend(picks["picks"].as_array().into_iter().flatten().filter_map(|p| p["topic"].as_str().map(String::from)));
    }
    let topics: Vec<String> = fresh_topics(&topics, &log).into_iter().take(n).collect();
    let ids = topics.iter().map(|t| pipeline::create_job(t, &[])).collect::<anyhow::Result<Vec<_>>>()?;
    let _ = app.emit("jobs-changed", ());
    let slots = auto::slot_minutes(&plan, ids.len());
    let mut uploaded = 0;
    let made = run_queue(app, &s, "script", &ids, &log, |i, id| {
        let at = auto::slot_rfc3339(day, slots[i], plan.tz_offset_min);
        upload_one(&s, &UploadItem { id: id.into(), privacy: "private".into(), publish_at: Some(at) }, &log)?;
        uploaded += 1;
        Ok(())
    });
    auto::record(day, made, uploaded, if llm::cancelled() { "cancelled" } else { "" });
    log(&format!("⚙ Daily automation done: {made} made, {uploaded} scheduled or published"));
    let _ = app.emit("auto-changed", ());
    Ok(())
}

#[tauri::command]
fn auto_status() -> Value {
    let s = Settings::load();
    let mut v = auto::status(&s.auto);
    v["plan"] = serde_json::to_value(&s.auto).unwrap_or_default();
    v
}

/// "Run today's batch now", ignoring the start time (still only once per day).
#[tauri::command]
async fn auto_run_now(app: AppHandle) -> Res<()> {
    blocking(move || {
        let plan = Settings::load().auto;
        let (day, _) = auto::local_now(plan.tz_offset_min);
        run_auto_day(&app, day)
    })
    .await
}

#[tauri::command]
fn smart_picks() -> Vec<Value> {
    growth::picks().into_iter().filter(|p| p["status"] == "new").collect()
}

#[tauri::command]
fn dismiss_pick(video_id: String) -> Res<()> {
    growth::set_status(&video_id, "dismissed").map_err(err)
}

/// Check your shorts for anything blowing up right now (also runs every 30 minutes on its own).
#[tauri::command]
async fn check_growth(app: AppHandle) -> Res<usize> {
    fresh();
    blocking(move || growth::check(&Settings::load(), &logger(app))).await
}

/// Trends page: today's rising Google searches + YouTube's trending chart.
#[tauri::command]
async fn trends_now(geo: String, category: String) -> Res<Value> {
    fresh();
    blocking(move || {
        let s = Settings::load();
        let google = trends::google_trends(&geo)?;
        let (youtube, note) = if youtube::is_connected() {
            match youtube::trending_chart(&s, &geo.to_uppercase(), &category) {
                Ok(v) => (v, String::new()),
                Err(e) => (vec![], format!("YouTube trending unavailable: {e:#}")),
            }
        } else {
            (vec![], "Connect YouTube in Settings to also see YouTube's trending chart".into())
        };
        // What is hot in this channel's own kind of content (top Shorts in the niche, last 30 days).
        let (niche, niche_videos) = if youtube::is_connected() {
            let mut s2 = s.clone();
            match trends::niche_query(&mut s2).and_then(|q| youtube::trending(&s2, &q).map(|(v, _)| (q, v))) {
                Ok(x) => x,
                Err(e) => (format!("({e:#})"), vec![]),
            }
        } else {
            (String::new(), vec![])
        };
        Ok(json!({"google": google, "youtube": youtube, "note": note, "niche": niche, "niche_videos": niche_videos}))
    })
    .await
}

#[tauri::command]
async fn trend_picks(app: AppHandle, google: Vec<Value>, youtube: Vec<Value>, niche: Vec<Value>) -> Res<Value> {
    fresh();
    blocking(move || {
        logger(app)("AI is matching today's trends to your channel…");
        trends::opportunities(&Settings::load(), &google, &youtube, &niche)
    })
    .await
}

/// "Why is this video engaging?" (local download + Whisper + cut detection, then your AI explains it).
#[tauri::command]
async fn watch_video(app: AppHandle, url: String) -> Res<Value> {
    fresh();
    blocking(move || watch::analyze(&Settings::load(), url.trim(), &logger(app))).await
}

/// YouTube Studio-style stats for your channel, optionally with AI advice on what performs.
#[tauri::command]
async fn studio_insights(app: AppHandle, days: u64, advise: bool) -> Res<Value> {
    fresh();
    blocking(move || {
        let s = Settings::load();
        let mut r = youtube::studio_report(&s, days.clamp(7, 365))?;
        if advise && r["videos"].as_array().is_some_and(|v| !v.is_empty()) {
            logger(app)("AI is reading your stats…");
            let advice = llm::generate_json(&s, "research", &format!(
                "You are a YouTube Shorts growth analyst. From these channel stats, explain what performs and what to do next. \
                 averageViewPercentage above 100 means people rewatch.\nStats: {}\n\
                 Return ONLY JSON: {{\"summary\": 2 sentences, \"working\": [3 things that work, citing videos/numbers], \
                 \"fix\": [3 things to fix], \"next\": [5 specific next shorts to make]}}",
                serde_json::to_string(&r)?.chars().take(12000).collect::<String>()
            ))?;
            r["advice"] = advice;
        }
        Ok(r)
    })
    .await
}

/// Builds queued jobs one at a time, reporting progress. `after` runs for each finished job (e.g. upload).
/// On Cancel, jobs that never started are marked so they can be retried or deleted.
fn run_queue(app: &AppHandle, s: &Settings, task: &str, ids: &[String], log: &dyn Fn(&str), mut after: impl FnMut(usize, &str) -> anyhow::Result<()>) -> usize {
    let provider = s.pick(task).0;
    let before = usage::used_pct(&provider, true);
    let mut done = 0;
    for (i, id) in ids.iter().enumerate() {
        if llm::cancelled() {
            break;
        }
        let _ = app.emit("queue", json!({"index": i, "total": ids.len(), "id": id}));
        let topic = pipeline::load_state(id).map(|st| st.topic).unwrap_or_default();
        log(&format!("▶ {}/{}: {topic}", i + 1, ids.len()));
        match pipeline::build_job(s, id, log).and_then(|_| after(i, id)) {
            Ok(()) => done += 1,
            Err(e) if !llm::cancelled() => log(&format!("✖ {e:#}")),
            Err(_) => {}
        }
        let _ = app.emit("jobs-changed", ());
    }
    for id in ids {
        if let Ok(mut st) = pipeline::load_state(id) {
            if st.status == "queued" {
                st.status = "failed".into();
                st.error = "Cancelled before it started. Press Retry to make it, or Delete.".into();
                let _ = pipeline::save_state(id, &st);
            }
        }
    }
    let _ = app.emit("queue", json!({"index": ids.len(), "total": ids.len()}));
    usage::learn(&provider, before, usage::used_pct(&provider, true), done);
    let _ = app.emit("jobs-changed", ());
    let _ = app.emit("usage-changed", ());
    done
}

/// Create page: read the signed-in channel and suggest the best shorts for it. Saves a channel profile for scripts.
#[tauri::command]
async fn channel_ideas(app: AppHandle, count: usize) -> Res<Value> {
    fresh();
    blocking(move || {
        let (mut s, log) = (Settings::load(), logger(app));
        if !youtube::is_connected() {
            anyhow::bail!("connect your YouTube channel in Settings first");
        }
        log("Reading your channel…");
        let (title, description) = youtube::channel_profile(&s)?;
        let recent = youtube::my_titles(&s).unwrap_or_default();
        log("AI is working out the best shorts for your channel…");
        let v = pipeline::channel_ideas(&s, &title, &description, &recent, count.clamp(1, 20))?;
        if let Some(about) = v["about"].as_str() {
            s.channel_about = about.to_string();
            s.save()?;
        }
        log("Ideas ready");
        Ok(v)
    })
    .await
}

#[tauri::command]
async fn usage_summary(fresh: bool) -> Res<Value> {
    blocking(move || Ok(usage::summary(&Settings::load(), fresh))).await
}

#[tauri::command]
async fn list_models(writer: String) -> Res<Vec<String>> {
    blocking(move || Ok(llm::list_models(&Settings::load(), &writer))).await
}

#[tauri::command]
async fn retry_job(app: AppHandle, id: String) -> Res<()> {
    blocking(move || {
        let _busy = Busy::take()?;
        let r = pipeline::build_job(&Settings::load(), &id, &logger(app.clone()));
        let _ = app.emit("jobs-changed", ());
        r
    })
    .await
}

#[tauri::command]
fn list_jobs() -> Vec<Value> {
    pipeline::list_jobs()
}

#[tauri::command]
fn save_job_meta(id: String, title: String, description: String, tags: Vec<String>) -> Res<()> {
    pipeline::save_meta(&id, &title, &description, &tags).map_err(err)
}

#[tauri::command]
fn delete_job(id: String) -> Res<()> {
    let dir = pipeline::job_dir(&id).map_err(err)?;
    if !dir.join("job.json").exists() {
        return Err("not a job folder".into());
    }
    let _ = ledger::release(&id); // unpublished moments become available again
    std::fs::remove_dir_all(dir).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_job_folder(id: String) -> Res<()> {
    // Empty id opens the output folder itself.
    let dir = if id.is_empty() { out_dir().clone() } else if id == "_brand" { brand_dir() } else { pipeline::job_dir(&id).map_err(err)? };
    llm::command("explorer").arg(dir).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct UploadItem {
    id: String,
    privacy: String,
    publish_at: Option<String>,
}

/// Posts each short's AI engagement comment once the short is publicly visible: right after a public upload,
/// or once a scheduled short's time has passed (checked every 10 minutes while the app is open).
fn post_due_comments(s: &Settings, log: &dyn Fn(&str)) {
    if !s.auto_comment || !youtube::is_connected() {
        return;
    }
    for job in pipeline::list_jobs() {
        let id = job["id"].as_str().unwrap_or_default();
        let Ok(mut st) = pipeline::load_state(id) else { continue };
        if st.video_id.is_empty() || !st.comment_id.is_empty() {
            continue;
        }
        let went_live = st.status == "scheduled" && youtube::parse_rfc3339(&st.publish_at).is_some_and(|t| t + 60 <= youtube::now_secs());
        if !(went_live || (st.status == "uploaded" && st.privacy != "private")) {
            continue;
        }
        let script: Value = pipeline::job_dir(id).ok().and_then(|d| std::fs::read_to_string(d.join("script.json")).ok()).and_then(|x| serde_json::from_str(&x).ok()).unwrap_or_default();
        let text = script["comment"].as_str().filter(|c| !c.trim().is_empty()).unwrap_or("What did you think? Drop your answer below 👇").trim().to_string();
        match youtube::post_comment(s, &st.video_id, &text) {
            Ok(cid) => {
                st.comment_id = if cid.is_empty() { "posted".into() } else { cid };
                if went_live {
                    (st.status, st.privacy) = ("uploaded".into(), "public".into());
                }
                let _ = pipeline::save_state(id, &st);
                log(&format!("💬 Engagement comment posted on “{}”. Pin it from the Library.", script["title"].as_str().unwrap_or(&st.topic)));
            }
            Err(e) => {
                // Comments turned off (403) will never work; anything else is retried on the next pass.
                if e.to_string().contains("HTTP 403") {
                    st.comment_id = "unavailable".into();
                    let _ = pipeline::save_state(id, &st);
                }
                log(&format!("  engagement comment not posted yet: {e:#}"));
            }
        }
    }
}

#[tauri::command]
async fn analytics() -> Res<Value> {
    fresh();
    blocking(|| {
        let s = Settings::load();
        let mut v = youtube::analytics(&s)?;
        let mine: std::collections::HashSet<String> = pipeline::list_jobs().iter().filter_map(|j| j["video_id"].as_str().filter(|x| !x.is_empty()).map(String::from)).collect();
        for item in v["list"].as_array_mut().into_iter().flatten() {
            item["by_app"] = mine.contains(item["id"].as_str().unwrap_or("")).into();
        }
        v["rpm"] = s.rpm.into();
        Ok(v)
    })
    .await
}

/// After YouTube says "uploadLimitExceeded", no upload is attempted before this time (Unix seconds).
static UPLOAD_BLOCKED_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Upload shorts that were queued by the daily upload limit (runs from the background loop).
fn retry_pending_uploads(s: &Settings, log: &dyn Fn(&str)) {
    if !youtube::is_connected() || youtube::now_secs() < UPLOAD_BLOCKED_UNTIL.load(Ordering::SeqCst) || BUSY.load(Ordering::SeqCst) {
        return;
    }
    for job in pipeline::list_jobs().into_iter().filter(|j| j["pending_upload"] == true && j["video_id"].as_str().unwrap_or("").is_empty()) {
        let id = job["id"].as_str().unwrap_or_default().to_string();
        let at = job["pending_at"].as_str().filter(|a| !a.is_empty()).map(String::from);
        let item = UploadItem { id, privacy: job["pending_privacy"].as_str().unwrap_or("public").to_string(), publish_at: at };
        if let Err(e) = upload_one(s, &item, log) {
            log(&format!("✖ queued upload: {e:#}"));
        }
        if youtube::now_secs() < UPLOAD_BLOCKED_UNTIL.load(Ordering::SeqCst) {
            break; // still capped
        }
    }
}

/// YouTube's own view of each published short: processing, failed, rejected (and why), or live.
#[tauri::command]
async fn published_status() -> Res<Value> {
    fresh();
    blocking(|| {
        let ids: Vec<String> = pipeline::list_jobs().iter().filter_map(|j| j["video_id"].as_str().filter(|v| !v.is_empty()).map(String::from)).collect();
        youtube::processing_status(&Settings::load(), &ids)
    })
    .await
}

fn upload_one(s: &Settings, item: &UploadItem, log: &dyn Fn(&str)) -> anyhow::Result<()> {
    let mut st = pipeline::load_state(&item.id)?;
    if !st.video_id.is_empty() {
        anyhow::bail!("already on YouTube (https://youtube.com/shorts/{})", st.video_id);
    }
    // Never publish the same content twice: same topic/title, or the same moment of a source video.
    let script: Value = serde_json::from_str(&std::fs::read_to_string(pipeline::job_dir(&item.id)?.join("script.json"))?)?;
    let title = script["title"].as_str().unwrap_or(&st.topic).to_string();
    let mut keys = vec![(ledger::topic_key(&title), 0.0, 0.0)];
    if st.source_url.is_empty() {
        keys.push((ledger::topic_key(&st.topic), 0.0, 0.0));
    } else {
        keys.push((ledger::source_key(&st.source_url), script["start"].as_f64().unwrap_or(0.0), script["end"].as_f64().unwrap_or(0.0)));
    }
    if let Some(dupe) = ledger::published_conflict(&item.id, &keys) {
        anyhow::bail!("not uploaded: this is the same content as “{dupe}”, which is already on your channel");
    }
    let mut privacy = item.privacy.clone();
    let mut at = item.publish_at.clone().filter(|a| !a.is_empty());
    if let Some(a) = at.clone() {
        // Generation ran past the publish time: post it right away instead of scheduling it.
        let t = youtube::parse_rfc3339(&a).ok_or_else(|| anyhow::anyhow!("invalid schedule time {a}"))?;
        if t < youtube::now_secs() + 5 * 60 {
            log(&format!("  publish time {a} has passed; publishing now"));
            at = None;
            privacy = "public".into();
        }
    }
    // YouTube's daily per-channel upload cap: queue it instead of failing, and retry once the cap resets.
    let queue_it = |st: &mut pipeline::JobState, why: &str| -> anyhow::Result<()> {
        (st.pending_upload, st.pending_privacy, st.pending_at) = (true, item.privacy.clone(), item.publish_at.clone().unwrap_or_default());
        st.error = why.into();
        pipeline::save_state(&item.id, st)?;
        log(&format!("  ⏳ {why}"));
        Ok(())
    };
    let blocked = UPLOAD_BLOCKED_UNTIL.load(Ordering::SeqCst);
    if youtube::now_secs() < blocked {
        return queue_it(&mut st, &format!("Queued: YouTube's daily upload limit is reached; the app retries after {}", youtube::to_rfc3339(blocked)));
    }
    log(&format!("Uploading “{}”{}…", st.topic, at.as_ref().map(|a| format!(" (scheduled {a})")).unwrap_or_default()));
    let vid = match youtube::upload(s, &pipeline::job_dir(&item.id)?, &privacy, at.as_deref()) {
        Ok(v) => v,
        Err(e) if e.to_string().contains("uploadLimitExceeded") => {
            // The cap is a rolling 24 hours; try again in 6 hours rather than hammering it (each try costs quota).
            UPLOAD_BLOCKED_UNTIL.store(youtube::now_secs() + 6 * 3600, Ordering::SeqCst);
            return queue_it(&mut st, "Queued: your channel hit YouTube's daily upload limit (new or unverified channels get a small one). The app uploads it automatically later; verifying your channel by phone raises the limit");
        }
        Err(e) => return Err(e),
    };
    (st.pending_upload, st.pending_privacy, st.pending_at) = (false, String::new(), String::new());
    st.error.clear();
    st.video_id = vid.clone();
    st.status = if at.is_some() { "scheduled".into() } else { "uploaded".into() };
    st.publish_at = at.unwrap_or_default();
    st.privacy = privacy.clone();
    // The video is live on YouTube now; if recording that fails, say which video it was so it is not uploaded twice.
    pipeline::save_state(&item.id, &st).map_err(|e| anyhow::anyhow!("uploaded as https://youtube.com/shorts/{vid} but could not record it: {e}"))?;
    if st.source_url.is_empty() {
        ledger::reserve(&ledger::topic_key(&st.topic), 0.0, 0.0, &item.id, &title)?;
    }
    ledger::mark_published(&item.id, Some((&ledger::topic_key(&title), &title)))?;
    log(&format!("✔ Uploaded: https://youtube.com/shorts/{vid}"));
    Ok(())
}

#[tauri::command]
async fn upload(app: AppHandle, items: Vec<UploadItem>) -> Res<usize> {
    blocking(move || {
        let _busy = Busy::take()?;
        let (s, log) = (Settings::load(), logger(app.clone()));
        let mut ok = 0;
        for item in &items {
            if llm::cancelled() {
                break;
            }
            match upload_one(&s, item, &log) {
                Ok(()) => ok += 1,
                Err(e) => log(&format!("✖ {e:#}")),
            }
            let _ = app.emit("jobs-changed", ());
        }
        post_due_comments(&s, &log);
        Ok(ok)
    })
    .await
}

/// Niche -> research -> N shorts -> upload now or schedule one per slot.
#[tauri::command]
async fn autopilot(app: AppHandle, niche: String, count: usize, slots: Vec<String>, privacy: String) -> Res<usize> {
    blocking(move || {
        let _busy = Busy::take()?;
        let (s, log) = (Settings::load(), logger(app.clone()));
        let uploading = !slots.is_empty() || !privacy.is_empty();
        if uploading && !youtube::is_connected() {
            anyhow::bail!("connect your YouTube channel in Settings first");
        }
        let count = count.clamp(1, 30);
        let plan = research_with_data(&s, &niche, count, &log)?;
        let ideas: Vec<String> = plan["ideas"].as_array().into_iter().flatten().filter_map(|i| i.as_str().map(String::from)).take(count).collect();
        let tags: Vec<String> = plan["tags"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)).collect();
        log(&format!("Plan: {} shorts. Angle: {}", ideas.len(), plan["angle"].as_str().unwrap_or("")));
        let ids = fresh_topics(&ideas, &log).iter().map(|idea| pipeline::create_job(idea, &tags)).collect::<anyhow::Result<Vec<_>>>()?;
        let _ = app.emit("jobs-changed", ());
        let done = run_queue(&app, &s, "script", &ids, &log, |i, id| {
            if !uploading {
                return Ok(());
            }
            let item = UploadItem { id: id.into(), privacy: if privacy.is_empty() { "private".into() } else { privacy.clone() }, publish_at: slots.get(i).cloned() };
            upload_one(&s, &item, &log)
        });
        post_due_comments(&s, &log);
        log(&format!("Autopilot finished: {done}/{} shorts done", ideas.len()));
        Ok(done)
    })
    .await
}

#[tauri::command]
async fn fetch_comments(app: AppHandle) -> Res<Vec<Value>> {
    fresh();
    blocking(move || {
        let (s, log) = (Settings::load(), logger(app));
        log("Fetching new comments…");
        let mut comments = youtube::unanswered_comments(&s)?;
        if !comments.is_empty() {
            log(&format!("Drafting {} replies…", comments.len()));
            match pipeline::draft_replies(&s, &comments) {
                Ok(drafts) => comments.iter_mut().zip(drafts).for_each(|(c, d)| c["draft"] = d.into()),
                Err(e) => log(&format!("✖ could not draft replies ({e:#}); showing comments without drafts")),
            }
        }
        Ok(comments)
    })
    .await
}

#[tauri::command]
async fn reply_comment(id: String, text: String) -> Res<()> {
    blocking(move || {
        if text.trim().is_empty() {
            anyhow::bail!("reply is empty");
        }
        youtube::reply(&Settings::load(), &id, text.trim())
    })
    .await
}

#[tauri::command]
fn skip_comment(id: String) -> Res<()> {
    youtube::mark_handled(&id).map_err(err)
}

#[tauri::command]
async fn find_clips(query: String, opts: youtube::ClipSearch) -> Res<Value> {
    fresh();
    blocking(move || {
        let (items, next) = youtube::search_cc_page(&Settings::load(), query.trim(), &opts)?;
        Ok(json!({"items": items, "next": next}))
    })
    .await
}

/// Creative Commons videos that fit what your channel already posts.
#[tauri::command]
async fn channel_clips(app: AppHandle) -> Res<Value> {
    fresh();
    blocking(move || {
        let (s, log) = (Settings::load(), logger(app));
        log("Reading your channel's latest uploads…");
        // A brand-new channel has no uploads yet: use its profile (niche + description) instead.
        let mut titles = youtube::my_titles(&s).unwrap_or_default();
        if titles.is_empty() {
            titles = [s.channel_about.clone(), s.niche_query.clone()].into_iter().filter(|t| !t.trim().is_empty()).collect();
        }
        if titles.is_empty() {
            anyhow::bail!("tell the app what this channel is about first (Channel setup, or Suggest for my channel on Create)");
        }
        let queries = pipeline::channel_queries(&s, &titles)?;
        let mut seen = std::collections::HashSet::new();
        let mut results = vec![];
        for q in &queries {
            log(&format!("Searching Creative Commons videos: {q}"));
            results.extend(youtube::search_cc(&s, q)?.into_iter().filter(|v| seen.insert(v["id"].to_string())));
        }
        Ok(json!({"queries": queries, "results": results}))
    })
    .await
}

#[tauri::command]
async fn twitch_clips(login: String, days: u64) -> Res<Vec<Value>> {
    fresh();
    blocking(move || twitch::top_clips(&Settings::load(), &login, days)).await
}

#[derive(Deserialize)]
struct ClipItem {
    url: String,
    title: String,
    channel: String,
}

#[tauri::command]
async fn make_clips(app: AppHandle, items: Vec<ClipItem>, tags: Vec<String>, per: u32) -> Res<usize> {
    blocking(move || {
        let _busy = Busy::take()?;
        let (s, log) = (Settings::load(), logger(app.clone()));
        let mut ids = vec![];
        for it in &items {
            let title = if it.title.trim().is_empty() { "Clip" } else { it.title.trim() };
            match pipeline::create_clip_jobs(it.url.trim(), title, it.channel.trim(), &tags, per) {
                Ok(group) => ids.extend(group),
                Err(e) => log(&format!("✖ {}: {e:#}", it.url)),
            }
        }
        let _ = app.emit("jobs-changed", ());
        Ok(run_queue(&app, &s, "clips", &ids, &log, |_, _| Ok(())))
    })
    .await
}

/// Channel art lives per channel: VideosAIYT_brand<channel id>.
fn brand_dir() -> std::path::PathBuf {
    let c = settings::active_channel();
    out_dir().join("_brand").join(if c.is_empty() { "default".to_string() } else { c })
}

/// Viral research for a niche -> AI channel redesign -> banner + profile picture. Changes nothing on YouTube.
#[tauri::command]
async fn rebrand(app: AppHandle, niche: String, writer: String, image_model: String, pending: bool) -> Res<Value> {
    fresh();
    blocking(move || {
        let (s, log) = (with_choices(Settings::load(), &writer, &image_model), logger(app));
        let (mut videos, mut tags, mut titles) = (vec![], vec![], vec![]);
        if youtube::is_connected() {
            log("Reading this month's viral Shorts in the niche…");
            if !pending {
                titles = youtube::my_titles(&s).unwrap_or_default(); // redesigning the current channel
            }
            (videos, tags) = youtube::trending(&s, &niche).unwrap_or_default();
        }
        log(if pending { "AI is designing your new channel…" } else { "AI is redesigning the channel…" });
        let mut plan = pipeline::rebrand_plan(&s, &niche, &titles, &videos, &tags)?;
        let name = plan["names"][0].as_str().unwrap_or(&niche).to_string();
        let initials = plan["initials"].as_str().map(String::from).unwrap_or_else(|| name.chars().take(1).collect());
        // A new channel's kit waits in _brand\_pending until the channel is created and linked.
        let dir = if pending { pending_dir() } else { brand_dir() };
        std::fs::create_dir_all(&dir)?;
        log("Designing banner and profile picture…");
        let style = &plan["style"];
        let tagline = plan["tagline"].as_str().unwrap_or("").to_string();
        // Designed by the image model (logo + art) when one is available; otherwise a clean styled version.
        let kit = media::BrandKit { name: name.clone(), tagline: tagline.clone(), initials: initials.clone(), primary: style["primary"].as_str().unwrap_or("").into(), secondary: style["secondary"].as_str().unwrap_or("").into(), niche: niche.clone(), vibe: style["vibe"].as_str().unwrap_or("").into() };
        plan["art"] = match media::brand_art_ai(&s, &dir, &kit, &log) {
            Ok(()) => "ai".into(),
            Err(e) => {
                log(&format!("  AI art unavailable ({e:#}); making a simple styled version"));
                media::brand_images(&dir, &name, &tagline, &initials, style["primary"].as_str().unwrap_or(""), style["secondary"].as_str().unwrap_or(""))?;
                format!("basic: {e:#}").into()
            }
        };
        plan["banner"] = dir.join("banner.jpg").to_string_lossy().to_string().into();
        plan["avatar"] = dir.join("avatar.png").to_string_lossy().to_string().into();
        plan["top_videos"] = videos.into();
        plan["niche"] = niche.clone().into();
        if pending {
            plan["writer"] = writer.into();
            plan["image_model"] = image_model.into();
            settings::write_atomic(&settings::config_dir().join("pending_kit.json"), &serde_json::to_string_pretty(&plan)?)?;
        }
        log("Channel plan ready");
        Ok(plan)
    })
    .await
}

#[tauri::command]
fn flux_status() -> Value {
    json!({"ready": tools::flux_ready(), "gemini_key": !Settings::load().gemini_key.trim().is_empty()})
}

/// Downloads local FLUX (~10 GB) in the background; everything else keeps working meanwhile.
#[tauri::command]
async fn setup_flux(app: AppHandle) -> Res<()> {
    blocking(move || {
        static SETUP: AtomicBool = AtomicBool::new(false);
        if SETUP.swap(true, Ordering::SeqCst) {
            anyhow::bail!("FLUX is already downloading");
        }
        let r = tools::setup_flux(&logger(app));
        SETUP.store(false, Ordering::SeqCst);
        r
    })
    .await
}

/// Where releases are published; the app checks it for a newer version.
const RELEASES: &str = "https://api.github.com/repos/clyde369/aiyt-studio/releases/latest";

/// {current, latest, newer, url}: a newer GitHub release than this build? (Silent if offline.)
#[tauri::command]
async fn check_update() -> Res<Value> {
    blocking(|| {
        let current = env!("CARGO_PKG_VERSION");
        let v: Value = ureq::get(RELEASES).set("User-Agent", "aiyt-studio").call().map_err(llm::http_err)?.into_json()?;
        let latest = v["tag_name"].as_str().unwrap_or("").trim_start_matches('v').to_string();
        let parse = |s: &str| s.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
        Ok(json!({"current": current, "latest": latest, "newer": !latest.is_empty() && parse(&latest) > parse(current), "url": v["html_url"]}))
    })
    .await
}

/// Settings with the wizard's choices: a specific AI for every job ("" = keep) and image model.
fn with_choices(mut s: Settings, writer: &str, image_model: &str) -> Settings {
    if !writer.is_empty() {
        s.writer = writer.into();
        s.writer_model.clear();
        s.task_models.clear();
    }
    if !image_model.is_empty() {
        s.image_model = image_model.into();
    }
    s
}

fn pending_dir() -> std::path::PathBuf {
    out_dir().join("_brand").join("_pending")
}

/// The new-channel kit made before the channel existed (None once applied).
#[tauri::command]
fn pending_kit() -> Option<Value> {
    std::fs::read_to_string(settings::config_dir().join("pending_kit.json")).ok().and_then(|x| serde_json::from_str(&x).ok())
}

/// Wizard step 1: AI suggests niches, checks each against this month's real Shorts, and scores the chance of success.
#[tauri::command]
async fn niche_finder(app: AppHandle, writer: String) -> Res<Value> {
    fresh();
    blocking(move || {
        let (s, log) = (with_choices(Settings::load(), &writer, ""), logger(app));
        log("AI is shortlisting niches…");
        let ideas = pipeline::suggest_niches(&s)?;
        let names: Vec<String> = ideas["niches"].as_array().into_iter().flatten().filter_map(|n| n["name"].as_str().map(String::from)).take(8).collect();
        let mut evidence = vec![];
        if youtube::is_connected() {
            for n in &names {
                log(&format!("Checking real Shorts for “{n}”…"));
                let (videos, _) = youtube::trending(&s, n).unwrap_or_default();
                let mut views: Vec<u64> = videos.iter().filter_map(|v| v["views"].as_u64()).collect();
                views.sort_unstable_by(|a, b| b.cmp(a));
                let top10 = &views[..views.len().min(10)];
                let median = top10.get(top10.len() / 2).copied().unwrap_or(0);
                evidence.push(json!({"niche": n, "shorts_found": views.len(), "median_top10_views": median, "best_views": views.first(),
                    "over_100k": views.iter().filter(|v| **v >= 100_000).count(),
                    "examples": videos.iter().take(3).map(|v| v["title"].clone()).collect::<Vec<_>>()}));
            }
        }
        log("AI is scoring each niche…");
        let mut scored = pipeline::score_niches(&s, &ideas, &evidence)?;
        scored["live_data"] = (!evidence.is_empty()).into();
        Ok(scored)
    })
    .await
}

/// Wizard final step: the new channel is linked: apply the kit, set its profile, and plan its first shorts.
#[tauri::command]
async fn launch_channel(app: AppHandle) -> Res<String> {
    fresh();
    blocking(move || {
        let log = logger(app.clone());
        let kit = pending_kit().ok_or_else(|| anyhow::anyhow!("no new-channel kit is waiting"))?;
        let mut s = with_choices(Settings::load(), kit["writer"].as_str().unwrap_or(""), kit["image_model"].as_str().unwrap_or(""));
        if !youtube::is_connected() {
            anyhow::bail!("link the new channel first");
        }
        // The kit's art becomes this channel's art.
        std::fs::create_dir_all(brand_dir())?;
        for f in ["banner.jpg", "avatar.png"] {
            let _ = std::fs::copy(pending_dir().join(f), brand_dir().join(f));
        }
        log("Applying the banner, description, keywords and playlists…");
        let kw: Vec<String> = kit["keywords"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|k| k.as_str())
            .map(|k| if k.contains(' ') { format!("\"{}\"", k.replace('"', "")) } else { k.to_string() })
            .collect();
        youtube::apply_branding(&s, kit["description"].as_str().unwrap_or(""), &kw.join(" "), Some(brand_dir().join("banner.jpg").as_path()))?;
        for p in kit["pillars"].as_array().into_iter().flatten() {
            if let Some(t) = p["name"].as_str() {
                let _ = youtube::create_playlist(&s, t, p["why"].as_str().unwrap_or(""));
            }
        }
        // This channel's profile: niche + voice for every future script.
        let niche = kit["niche"].as_str().unwrap_or("").to_string();
        let pillars: Vec<&str> = kit["pillars"].as_array().into_iter().flatten().filter_map(|p| p["name"].as_str()).collect();
        let mut saved = Settings::load();
        saved.channel_about = format!("{}: {niche}. {} Content pillars: {}.", kit["names"][0].as_str().unwrap_or(""), kit["tagline"].as_str().unwrap_or(""), pillars.join(", "));
        saved.niche_query = niche.clone();
        saved.save()?;
        s.channel_about = saved.channel_about.clone();
        s.niche_query = niche;
        // Start hunting: the best shorts to launch with, from what is popular in the niche right now.
        log("Finding the best first shorts for the niche…");
        let topics = best_topics(&s, 6, &log).unwrap_or_default();
        growth::add_starter_picks(&topics)?;
        let _ = std::fs::remove_file(settings::config_dir().join("pending_kit.json"));
        let _ = app.emit("smart-pick", topics.len());
        Ok(format!("Your new channel is set up. {} launch ideas are waiting on the Create page", topics.len()))
    })
    .await
}

/// The best topics to make right now for the active channel: follow-ups to shorts that are taking off first,
/// then AI picks from Google trends, YouTube's trending chart and the most popular Shorts in the niche.
fn best_topics(s: &Settings, n: usize, log: &dyn Fn(&str)) -> anyhow::Result<Vec<String>> {
    let mut topics: Vec<String> = growth::picks()
        .into_iter()
        .filter(|p| p["status"] == "new")
        .filter_map(|p| p["topic"].as_str().map(String::from))
        .take(n)
        .collect();
    if topics.len() < n {
        let google = trends::google_trends("US").unwrap_or_default();
        let chart = youtube::trending_chart(s, "US", "").unwrap_or_default();
        let mut s2 = s.clone();
        let niche = trends::niche_query(&mut s2).and_then(|q| youtube::trending(&s2, &q).map(|(v, _)| v)).unwrap_or_default();
        let picks = trends::opportunities(s, &google, &chart, &niche)?;
        topics.extend(picks["picks"].as_array().into_iter().flatten().filter_map(|p| p["topic"].as_str().map(String::from)));
    }
    Ok(fresh_topics(&topics, log).into_iter().take(n).collect())
}

/// ⚡ Make N shorts now for the active channel from the best topics; optionally upload each as soon as it is made.
#[tauri::command]
async fn quick_batch(app: AppHandle, count: usize, auto_upload: bool) -> Res<Vec<String>> {
    blocking(move || {
        let _busy = Busy::take()?;
        let (s, log) = (Settings::load(), logger(app.clone()));
        if !youtube::is_connected() {
            anyhow::bail!("link this channel first");
        }
        log(&format!("⚡ Finding the {count} best shorts to make for this channel…"));
        let topics = best_topics(&s, count.clamp(1, 10), &log)?;
        for p in growth::picks().iter().filter(|p| topics.iter().any(|t| Some(t.as_str()) == p["topic"].as_str())) {
            let _ = growth::set_status(p["video_id"].as_str().unwrap_or(""), "made");
        }
        let ids = topics.iter().map(|t| pipeline::create_job(t, &[])).collect::<anyhow::Result<Vec<_>>>()?;
        let _ = app.emit("jobs-changed", ());
        run_queue(&app, &s, "script", &ids, &log, |_, id| {
            if auto_upload {
                upload_one(&s, &UploadItem { id: id.into(), privacy: "public".into(), publish_at: None }, &log)?;
            }
            Ok(())
        });
        Ok(ids)
    })
    .await
}

/// Re-render the art with a different name, tagline or colours.
#[tauri::command]
async fn brand_art(app: AppHandle, name: String, tagline: String, initials: String, primary: String, secondary: String, ai: bool, niche: String, vibe: String, pending: bool) -> Res<String> {
    blocking(move || {
        // A new channel's kit keeps its art in _brand_pending until the channel is linked.
        let dir = if pending { pending_dir() } else { brand_dir() };
        std::fs::create_dir_all(&dir)?;
        if ai {
            let kit = media::BrandKit { name: name.clone(), tagline: tagline.clone(), initials: initials.clone(), primary: primary.clone(), secondary: secondary.clone(), niche, vibe };
            match media::brand_art_ai(&Settings::load(), &dir, &kit, &logger(app.clone())) {
                Ok(()) => return Ok("ai".into()),
                Err(e) => {
                    media::brand_images(&dir, &name, &tagline, &initials, &primary, &secondary)?;
                    return Ok(format!("basic: {e:#}"));
                }
            }
        }
        media::brand_images(&dir, &name, &tagline, &initials, &primary, &secondary)?;
        Ok("basic".into())
    })
    .await
}

/// Writes description, keywords and (optionally) the banner to the live channel. Only runs when the user clicks Apply.
#[tauri::command]
async fn apply_brand(description: String, keywords: Vec<String>, banner: bool, playlists: Vec<Value>, about: String) -> Res<String> {
    blocking(move || {
        let mut s = Settings::load();
        let kw: Vec<String> = keywords
            .iter()
            .map(|k| k.replace('"', "").trim().to_string())
            .filter(|k| !k.is_empty())
            .map(|k| if k.contains(' ') { format!("\"{k}\"") } else { k })
            .collect();
        let b = brand_dir().join("banner.jpg");
        youtube::apply_branding(&s, &description, &kw.join(" "), banner.then_some(b.as_path()))?;
        // One public playlist per content pillar, so the channel page has organised sections from day one.
        let mut made = 0;
        for p in &playlists {
            if let Some(title) = p["name"].as_str().filter(|t| !t.trim().is_empty()) {
                youtube::create_playlist(&s, title, p["why"].as_str().unwrap_or(""))?;
                made += 1;
            }
        }
        // The new channel profile steers every script from now on.
        if !about.trim().is_empty() {
            s.channel_about = about.trim().to_string();
            s.niche_query.clear(); // re-derived from the new profile
            s.save()?;
        }
        Ok(format!("Channel updated{}", if made > 0 { format!(", {made} playlists created") } else { String::new() }))
    })
    .await
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let cfg = app.path().app_config_dir()?;
            let out = app.path().video_dir()?.join("AIYT");
            std::fs::create_dir_all(&cfg)?;
            std::fs::create_dir_all(&out)?;
            let _ = settings::CONFIG_DIR.set(cfg);
            let _ = settings::OUT_DIR.set(out);
            // Engagement comments for shorts that went live (scheduled ones go live while the PC may be off).
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                post_due_comments(&Settings::load(), &logger(handle.clone()));
                retry_pending_uploads(&Settings::load(), &logger(handle.clone()));
                std::thread::sleep(std::time::Duration::from_secs(570));
            });
            // Smart picks: snapshot stats every 30 minutes and plan follow-ups for shorts that take off.
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(90));
                if youtube::is_connected() && !BUSY.load(Ordering::SeqCst) {
                    match growth::check(&Settings::load(), &logger(handle.clone())) {
                        Ok(n) if n > 0 => drop(handle.emit("smart-pick", n)),
                        Ok(_) => {}
                        Err(e) => logger(handle.clone())(&format!("  smart-pick check skipped: {e:#}")),
                    }
                    if let Err(e) = growth::tune_cta(&Settings::load(), &logger(handle.clone())) {
                        let _ = e; // usually just "needs the Analytics permission"; nothing to tune yet
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(1710));
            });
            // Daily automation: check every minute whether today's batch is due.
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
                let plan = Settings::load().auto;
                if let Some(day) = auto::due(&plan) {
                    if BUSY.load(Ordering::SeqCst) {
                        continue; // another task is running; try again next minute
                    }
                    if let Err(e) = run_auto_day(&handle, day) {
                        logger(handle.clone())(&format!("✖ daily automation: {e:#}"));
                    }
                }
            });
            // One-time: the old single-channel setup becomes the first linked channel (needs network; retried next start if offline).
            if let Err(e) = youtube::migrate_legacy() {
                eprintln!("channel migration skipped: {e:#}");
            }
            let _ = Settings::load(); // selects the active channel for everything below
            ledger::seed_published(&pipeline::list_jobs());
            // A job still "generating" at startup was interrupted by a crash or close.
            for job in pipeline::list_jobs().iter().filter(|j| j["status"] == "generating" || j["status"] == "queued") {
                let id = job["id"].as_str().unwrap_or_default();
                if let Ok(mut st) = pipeline::load_state(id) {
                    st.status = "failed".into();
                    st.error = "interrupted; press Retry".into();
                    let _ = pipeline::save_state(id, &st);
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings, save_settings, app_info, open_link, yt_channel, yt_connect, yt_disconnect, suggest_niches, research,
            generate, retry_job, list_jobs, save_job_meta, delete_job, open_job_folder, upload, autopilot, fetch_comments,
            reply_comment, skip_comment, find_clips, channel_clips, twitch_clips, make_clips, rebrand, brand_art, apply_brand, cancel_task, usage_summary, list_models, channel_ideas, analytics, preview_voice, kokoro_voices, piper_voices, gpu_status, setup_gpu_voice, trends_now, trend_picks, watch_video, studio_insights, auto_status, auto_run_now, smart_picks, dismiss_pick, check_growth, monetization, published_status, list_channels, switch_channel, pending_kit, niche_finder, launch_channel, quick_batch, flux_status, setup_flux, check_update
        ])
        .run(tauri::generate_context!())
        .expect("error while running AIYT Studio");
}
