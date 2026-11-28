use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

pub static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();
pub static OUT_DIR: OnceLock<PathBuf> = OnceLock::new();

/// App settings, API keys and private tokens live here (per-user app data folder).
pub fn config_dir() -> &'static PathBuf {
    CONFIG_DIR.get().expect("config dir not initialised")
}

/// Every short gets its own folder in here (Videos\AIYT).
pub fn out_dir() -> &'static PathBuf {
    OUT_DIR.get().expect("output dir not initialised")
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    /// gemini | claude | codex | opencode | ollama
    pub writer: String,
    /// Optional model override for the writer (e.g. "sonnet", "gpt-5", "anthropic/claude-sonnet-4", "llama3.2:3b").
    pub writer_model: String,
    /// kokoro | piper | gemini | local
    pub voice: String,
    pub gemini_voice: String,
    /// Local Kokoro voice name, e.g. af_heart (see tools::KOKORO_VOICES).
    pub kokoro_voice: String,
    /// Where Kokoro runs: auto (GPU if an NVIDIA card is found, else CPU) | gpu (GPU only) | cpu.
    pub kokoro_device: String,
    /// Render videos on the NVIDIA GPU (NVENC): auto | off.
    pub gpu_encode: String,
    /// House style for every short: red hook banner on top + captions with an accent colour.
    pub style_hook: bool,
    pub style_hook_color: String,
    pub style_accent: String,
    /// Language for Whisper subtitles (auto-detect guessed wrong on music-heavy clips).
    pub caption_language: String,
    /// Percent of AI shorts that get a spoken subscribe call (+ animated button) at ~75%; the rest end on a comment question.
    pub cta_rate: u32,
    /// Let the app raise/lower `cta_rate` from your stats (retention and subscribers gained, CTA vs no CTA).
    pub cta_auto: bool,
    /// Piper voice name, e.g. en_US-lessac-medium (see tools::PIPER_VOICES).
    pub piper_voice: String,
    /// pexels | pixabay | ai | gradient
    pub visuals: String,
    pub seconds: u32,
    /// Optional folder of royalty-free music; a random track is mixed quietly under the voice.
    pub music_dir: String,
    pub gemini_key: String,
    pub pexels_key: String,
    pub pixabay_key: String,
    pub google_client_id: String,
    pub google_client_secret: String,
    pub gemini_text_model: String,
    pub gemini_tts_model: String,
    pub gemini_image_model: String,
    /// AI images (channel art, AI-visual shorts): flux (local, free) | gemini (paid key).
    pub image_model: String,
    pub ollama_host: String,
    pub twitch_client_id: String,
    pub twitch_client_secret: String,
    /// Per-job AI choice: "script" | "research" | "clips" | "replies" -> writer + model. Missing = the default writer.
    pub task_models: std::collections::HashMap<String, TaskModel>,
    /// AI summary of the connected channel (niche, audience, style); added to every script prompt.
    pub channel_about: String,
    /// Short YouTube search query for this channel's niche (AI-derived once, editable).
    pub niche_query: String,
    pub auto: AutoPlan,
    /// YouTube channel id the app is working on (multi-channel); per-channel settings live in channels/<id>/profile.json.
    pub active_channel: String,
    /// Shorts revenue estimate: US$ earned per 1,000 views (YouTube Shorts RPM is usually about 0.03–0.10).
    pub rpm: f64,
    /// Post the AI engagement comment on every short once it is public.
    pub auto_comment: bool,
    /// Kokoro speaking pace: 1 = normal, above 1 = slower (stories set it per voice).
    pub kokoro_pace: f64,
    /// Story channel settings (animated story series instead of fact shorts).
    pub story: StoryPrefs,
}

/// A story channel makes animated story series: "fast" = drawn with code (seconds per picture), "slow" = AI pictures.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct StoryPrefs {
    /// This channel makes stories (Autopilot and ⚡ Make 3 then make the next story parts).
    pub enabled: bool,
    /// pixel (8-bit pixel art) | fast (drawn with code) | slow (AI pictures)
    pub style: String,
    /// Target length of one part.
    pub seconds: u32,
    /// Slow style pictures: local (SDXL on this PC, free) | gemini (paid key).
    pub images: String,
    /// Kokoro voice of the narrator.
    pub narrator_voice: String,
    /// Parts in a new series (the last one resolves the story).
    pub parts: u32,
}

impl Default for StoryPrefs {
    fn default() -> Self {
        Self { enabled: false, style: "pixel".into(), seconds: 45, images: "local".into(), narrator_voice: "bm_george".into(), parts: 10 }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            writer: "gemini".into(),
            writer_model: String::new(),
            voice: "gemini".into(),
            gemini_voice: "Puck".into(),
            kokoro_voice: "af_heart".into(),
            kokoro_device: "auto".into(),
            gpu_encode: "auto".into(),
            style_hook: true,
            style_hook_color: "#FF2D55".into(),
            style_accent: "#FFD400".into(),
            caption_language: "en".into(),
            cta_rate: 33,
            cta_auto: true,
            piper_voice: "en_US-lessac-medium".into(),
            visuals: "pexels".into(),
            seconds: 40,
            music_dir: String::new(),
            gemini_key: String::new(),
            pexels_key: String::new(),
            pixabay_key: String::new(),
            google_client_id: String::new(),
            google_client_secret: String::new(),
            gemini_text_model: "gemini-flash-latest".into(),
            gemini_tts_model: "gemini-3.8-flash-tts".into(),
            gemini_image_model: "gemini-3.1-flash-image".into(),
            image_model: "flux".into(),
            ollama_host: "http://localhost:11434".into(),
            twitch_client_id: String::new(),
            twitch_client_secret: String::new(),
            task_models: Default::default(),
            channel_about: String::new(),
            niche_query: String::new(),
            auto: AutoPlan::default(),
            active_channel: String::new(),
            rpm: 0.05,
            auto_comment: true,
            kokoro_pace: 1.0,
            story: StoryPrefs::default(),
        }
    }
}

/// Write via temp file + rename so a crash mid-save never leaves a half-written file (and lost keys).
pub fn write_atomic(path: &std::path::Path, data: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Settings that belong to one YouTube channel (each channel keeps its own; switching swaps them).
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct ChannelProfile {
    pub channel_about: String,
    pub niche_query: String,
    pub auto: AutoPlan,
    pub style_hook: bool,
    pub style_hook_color: String,
    pub style_accent: String,
    pub cta_rate: u32,
    pub cta_auto: bool,
    pub story: StoryPrefs,
}

impl Default for ChannelProfile {
    fn default() -> Self {
        Self::from(&Settings::default())
    }
}

impl From<&Settings> for ChannelProfile {
    fn from(s: &Settings) -> Self {
        Self {
            channel_about: s.channel_about.clone(), niche_query: s.niche_query.clone(), auto: s.auto.clone(), style_hook: s.style_hook,
            style_hook_color: s.style_hook_color.clone(), style_accent: s.style_accent.clone(), cta_rate: s.cta_rate, cta_auto: s.cta_auto,
            story: s.story.clone(),
        }
    }
}

/// The channel the app is working on (its id), cached for `channel_dir()`.
static ACTIVE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn active_channel() -> String {
    ACTIVE.lock().unwrap().clone()
}

/// Per-channel state (login, stats, picks, automation…). Falls back to the shared folder before any channel exists.
pub fn channel_dir() -> PathBuf {
    let a = active_channel();
    if a.is_empty() { config_dir().clone() } else { config_dir().join("channels").join(a) }
}

impl Settings {
    pub fn load() -> Self {
        let mut s: Settings = fs::read_to_string(config_dir().join("settings.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        *ACTIVE.lock().unwrap() = s.active_channel.clone();
        if !s.active_channel.is_empty() {
            // A channel with no profile yet starts clean (it must not inherit another channel's niche or plan).
            let p: ChannelProfile = fs::read_to_string(channel_dir().join("profile.json")).ok().and_then(|x| serde_json::from_str(&x).ok()).unwrap_or_default();
            s.apply_profile(p);
        }
        s
    }

    pub fn save(&self) -> Result<()> {
        write_atomic(&config_dir().join("settings.json"), &serde_json::to_string_pretty(self)?)?;
        if !self.active_channel.is_empty() && self.active_channel == active_channel() {
            write_atomic(&channel_dir().join("profile.json"), &serde_json::to_string_pretty(&ChannelProfile::from(self))?)?;
        }
        Ok(())
    }

    fn apply_profile(&mut self, p: ChannelProfile) {
        (self.channel_about, self.niche_query, self.auto) = (p.channel_about, p.niche_query, p.auto);
        (self.style_hook, self.style_hook_color, self.style_accent) = (p.style_hook, p.style_hook_color, p.style_accent);
        (self.cta_rate, self.cta_auto) = (p.cta_rate, p.cta_auto);
        self.story = p.story;
    }
}

/// Linked YouTube channels (channels.json).
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct ChannelEntry {
    pub id: String,
    pub title: String,
    pub thumb: String,
}

pub fn channels() -> Vec<ChannelEntry> {
    fs::read_to_string(config_dir().join("channels.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_channels(c: &[ChannelEntry]) -> Result<()> {
    write_atomic(&config_dir().join("channels.json"), &serde_json::to_string_pretty(c)?)
}

/// Make `id` the active channel: settings.json points at it and its profile is loaded from now on.
pub fn set_active_channel(id: &str) -> Result<()> {
    let mut base: Settings = fs::read_to_string(config_dir().join("settings.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    base.active_channel = id.to_string();
    write_atomic(&config_dir().join("settings.json"), &serde_json::to_string_pretty(&base)?)?;
    *ACTIVE.lock().unwrap() = id.to_string();
    fs::create_dir_all(channel_dir())?;
    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct TaskModel {
    /// Empty = use the default writer.
    pub writer: String,
    pub model: String,
}

impl Settings {
    /// (writer, model) for a job type, falling back to the default writer and model.
    pub fn pick(&self, task: &str) -> (String, String) {
        match self.task_models.get(task) {
            Some(t) if !t.writer.is_empty() => (t.writer.clone(), t.model.trim().to_string()),
            _ => (self.writer.clone(), self.writer_model.trim().to_string()),
        }
    }

    /// Every writer some job type currently uses.
    pub fn tasks_in_use(&self) -> Vec<String> {
        ["script", "research", "clips", "replies"].iter().map(|t| self.pick(t).0).collect()
    }
}

/// Daily automation: every day for `days` days, make `per_day` shorts at `make_at` and publish them at `times`.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct AutoPlan {
    pub enabled: bool,
    pub per_day: u32,
    pub days: u32,
    /// First day, as a local calendar day number (days since 1970-01-01).
    pub start_day: i64,
    /// Local "HH:MM" publish times, one per short.
    pub times: Vec<String>,
    /// Local "HH:MM" to start making the day's shorts.
    pub make_at: String,
    /// Minutes UTC is ahead of local time (JavaScript getTimezoneOffset()).
    pub tz_offset_min: i64,
}

impl Default for AutoPlan {
    fn default() -> Self {
        Self { enabled: false, per_day: 3, days: 7, start_day: 0, times: vec!["12:00".into(), "16:00".into(), "19:00".into()], make_at: "08:00".into(), tz_offset_min: 0 }
    }
}
