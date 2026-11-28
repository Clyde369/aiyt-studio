//! Story channels: animated story series told in parts. Every part is a normal job (Library, upload, schedule,
//! Autopilot) whose script.json carries "story". Fast style draws everything with code (story/fast.html); Slow style
//! uses AI pictures (story/slow.html). Both are animated by GiveMotion with one Kokoro voice per character, word-timed
//! captions, sound effects and music.

use crate::settings::{channel_dir, write_atomic, Settings};
use crate::{llm, media, pipeline, tools, youtube};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const FAST_PAGE: &str = include_str!("../story/fast.html");
const SLOW_PAGE: &str = include_str!("../story/slow.html");
/// 8-bit style: pixel-art pictures, an RPG dialogue box for the subtitles, pixel fonts (both SIL Open Font License).
const PIXEL_PAGE: &str = include_str!("../story/pixel.html");
const PIXEL_FONTS: &[(&str, &[u8])] = &[
    ("PressStart2P-Regular.ttf", include_bytes!("../story/fonts/PressStart2P-Regular.ttf")),
    ("VT323-Regular.ttf", include_bytes!("../story/fonts/VT323-Regular.ttf")),
    ("OFL-PressStart2P.txt", include_bytes!("../story/fonts/OFL-PressStart2P.txt")),
    ("OFL-VT323.txt", include_bytes!("../story/fonts/OFL-VT323.txt")),
];
const PIXEL_MOODS: &[&str] = &["calm", "tense", "scared", "excited", "whisper", "angry"];
const PIXEL_SFX: &[&str] = &["none", "heartbeat", "creak", "knock", "thunder", "static", "whoosh", "sting", "wind", "splash", "hammer"];
/// What the picture model must never paint in the 8-bit style.
const PIXEL_NEGATIVE: &str = "photo, realistic, 3d render, blurry, smooth gradients, text, letters, words, speech bubble, watermark, logo, comic panels, grid, collage, split screen, deformed, extra limbs, gore, blood";

/// Code-drawn cast and sets of the Fast style (they must match story/fast.html).
pub const KINDS: &[&str] = &[
    "toaster", "phone", "fridge", "router", "laptop", "mug", "alarm_clock", "car", "plant", "lamp", "tv", "washing_machine", "cat", "dog",
    "person_man", "person_woman", "person_kid", "person_old",
];
/// The Fast-style page has shipped (until then every series uses the Slow style).
pub fn fast_ready() -> bool {
    !FAST_PAGE.contains("placeholder: replaced by the Fast template")
}
pub const BACKGROUNDS: &[&str] = &["kitchen", "living_room", "bedroom", "office", "street", "park", "classroom", "shop", "space", "bathroom"];
const MOODS: &[&str] = &["neutral", "happy", "angry", "sad", "shocked", "smug", "scared", "dead"];
const MOVES: &[&str] = &["bounce", "shake", "jump", "spin", "fall", "enter", "exit", "grow"];
const CAMERAS: &[&str] = &["push_in", "pull_out", "pan_left", "pan_right", "punch_zoom", "shake"];
const SFX: &[&str] = &["none", "whoosh", "boing", "thud", "ding", "record_scratch"];
/// Character voices the AI may cast (Kokoro), with a hint of how each sounds.
const VOICES: &[(&str, &str)] = &[
    ("af_heart", "warm young woman"), ("af_bella", "bright energetic young woman"), ("af_nicole", "soft breathy woman"), ("af_sky", "light youthful girl"),
    ("bf_emma", "British woman"), ("am_michael", "friendly everyday man"), ("am_puck", "playful quirky man"), ("am_onyx", "deep grumpy man"),
    ("am_fenrir", "gruff tough man"), ("am_adam", "plain young man"), ("am_santa", "jolly old man"), ("bm_lewis", "British man"), ("bm_daniel", "calm British man"),
];
/// Caption pill colours for characters (the narrator is yellow).
const CAPTION: &[&str] = &["#ff7ab6", "#5ef2c2", "#7cc4ff", "#ffa94d", "#b197fc", "#8ce99a"];

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Character {
    pub name: String,
    /// Fast style: a code-drawn kind from KINDS.
    pub kind: String,
    /// Slow style: how the character looks, repeated in every picture prompt.
    pub look: String,
    /// Kokoro voice.
    pub voice: String,
    /// Main colour (clothes / body) in the Fast style.
    pub color: String,
    /// Caption pill colour.
    pub caption: String,
    /// Speaking pace for the voice (1 = normal, above 1 = slower). 0 = normal.
    pub pace: f64,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Part {
    pub n: u32,
    pub job_id: String,
    pub title: String,
    pub summary: String,
    pub cliffhanger: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Series {
    pub id: String,
    pub title: String,
    pub premise: String,
    /// pixel (8-bit pixel art) | fast (drawn with code) | slow (AI pictures)
    pub style: String,
    /// Slow and 8-bit styles: the art style added to every picture prompt.
    pub art: String,
    /// The narrator's voice: the hero's own voice when the story is told in the first person. Empty = Settings.
    pub narrator_voice: String,
    /// The narrator's pace (1 = normal, above 1 = slower: suspense).
    pub narrator_pace: f64,
    pub characters: Vec<Character>,
    pub parts: Vec<Part>,
    /// How many parts the story is planned to take (the last one resolves it).
    pub planned: u32,
    pub created: u64,
}

impl Series {
    pub fn finished(&self) -> bool {
        self.parts.len() as u32 >= self.planned.max(1)
    }
}

fn dir() -> PathBuf {
    channel_dir().join("stories")
}

fn valid_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("invalid series id");
    }
    Ok(())
}

/// This channel's series, newest first.
pub fn list() -> Vec<Series> {
    let mut all: Vec<Series> = fs::read_dir(dir())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("series-"))
        .filter_map(|e| fs::read_to_string(e.path()).ok().and_then(|x| serde_json::from_str(&x).ok()))
        .collect();
    all.sort_by_key(|s| std::cmp::Reverse(s.created));
    all
}

pub fn load(id: &str) -> Result<Series> {
    valid_id(id)?;
    Ok(serde_json::from_str(&fs::read_to_string(dir().join(format!("{id}.json"))).context("that story series no longer exists")?)?)
}

fn save(se: &Series) -> Result<()> {
    write_atomic(&dir().join(format!("{}.json", se.id)), &serde_json::to_string_pretty(se)?)
}

/// Deletes the series record (its parts stay in the Library).
pub fn delete(id: &str) -> Result<()> {
    valid_id(id)?;
    let _ = fs::remove_file(dir().join(format!("{id}.json")));
    Ok(())
}

pub fn last_research() -> Option<Value> {
    fs::read_to_string(dir().join("research.json")).ok().and_then(|x| serde_json::from_str(&x).ok())
}

/// The niche of the channel the app is on: the saved one, else worked out once from the channel's YouTube
/// description and uploads, so channels that existed before the app work too. Remembered for the channel.
fn channel_niche(s: &Settings, log: &dyn Fn(&str)) -> Result<String> {
    if !s.niche_query.trim().is_empty() {
        return Ok(s.niche_query.trim().to_string());
    }
    let mut about = s.channel_about.trim().to_string();
    if youtube::is_connected() {
        log("Reading your channel to work out its niche…");
        if let Ok((title, description)) = youtube::channel_profile(s) {
            about.push_str(&format!("\nChannel: {title}\n{description}"));
        }
        let titles = youtube::my_titles(s).unwrap_or_default();
        if !titles.is_empty() {
            about.push_str(&format!("\nRecent uploads:\n{}", titles.into_iter().take(15).collect::<Vec<_>>().join("\n")));
        }
    }
    if about.trim().is_empty() {
        bail!("type the story niche you want (e.g. home energy comedy), or link this channel to YouTube so the AI can read it");
    }
    let v = llm::generate_json(s, "research", &format!(
        "This describes a YouTube channel:\n{about}\nReturn ONLY JSON {{\"query\": a 2-4 word YouTube search query for the channel's niche, e.g. \"home energy saving\"}}"
    ))?;
    let q = v["query"].as_str().unwrap_or("").trim().to_string();
    if q.is_empty() {
        bail!("could not work out this channel's niche; type the story niche you want");
    }
    let mut saved = Settings::load();
    saved.niche_query = q.clone();
    saved.save()?;
    Ok(q)
}

/// What is working for story Shorts in a niche right now, and original series ideas inspired by it.
/// `niche` = typed by the user; "" = this channel's own niche.
pub fn research(s: &Settings, niche: &str, log: &dyn Fn(&str)) -> Result<Value> {
    let niche = if niche.trim().is_empty() { channel_niche(s, log)? } else { niche.trim().to_string() };
    let mut videos: Vec<Value> = vec![];
    if youtube::is_connected() {
        for q in [format!("{niche} story"), format!("{niche} animation"), niche.clone()] {
            log(&format!("Reading the most-viewed Shorts for “{q}”…"));
            match youtube::trending(s, &q) {
                Ok((v, _)) => videos.extend(v),
                Err(e) => log(&format!("  skipped: {e:#}")),
            }
        }
        let mut seen = std::collections::HashSet::new();
        videos.retain(|v| seen.insert(v["title"].as_str().unwrap_or("").to_lowercase()));
        videos.sort_by_key(|v| std::cmp::Reverse(v["views"].as_u64().unwrap_or(0)));
        videos.truncate(30);
    } else {
        log("YouTube is not linked: researching from the AI's own knowledge");
    }
    let top: Vec<String> = videos.iter().map(|v| format!("- {} ({} views, {})", v["title"].as_str().unwrap_or(""), v["views"], v["channel"].as_str().unwrap_or(""))).collect();
    let made: Vec<String> = list().into_iter().map(|x| x.title).collect();
    log("AI is studying what makes these stories work…");
    let mut v = llm::generate_json(s, "research", &format!(
        "You are a YouTube Shorts strategist for animated STORY channels. Think carefully.\nChannel: {}\nNiche: {niche}\n\
         The most-viewed story Shorts in this niche this month:\n{}\n\
         First work out what makes them work: premise types, hooks, twists, characters, how parts and cliffhangers are used, length.\n\
         Then propose 6 ORIGINAL story series for this channel. Inspired by what works, but never a retelling of any of these videos: \
         no copied plots, characters or titles, no copyrighted characters or brands. Family friendly, but made for teens and adults (not kids).\n\
         Series this channel already has (do not repeat): {}\n\
         Return ONLY JSON: {{\"insights\": [5 short insights about what works], \"ideas\": [{{\"title\": series title, \"premise\": 2 sentences, \
         \"hook\": the first line of part 1, \"why\": one sentence citing the data, \"parts\": 3-6}}]}}",
        if s.channel_about.trim().is_empty() { niche.as_str() } else { s.channel_about.as_str() }, if top.is_empty() { "(no live data; use your knowledge)".to_string() } else { top.join("\n") },
        if made.is_empty() { "(none)".to_string() } else { made.join("; ") }
    ))?;
    v["top_videos"] = videos.into_iter().take(12).collect::<Vec<_>>().into();
    v["niche"] = niche.into();
    write_atomic(&dir().join("research.json"), &serde_json::to_string_pretty(&v)?)?;
    Ok(v)
}

/// Turns an idea into a series: title, premise, cast with fixed looks and voices.
pub fn new_series(s: &Settings, idea: &str, style: &str) -> Result<Series> {
    let style = match style {
        "pixel" | "slow" => style,
        _ if fast_ready() => "fast",
        _ => "slow",
    };
    let fast = style == "fast";
    let parts = s.story.parts.clamp(1, 50);
    let cast_rule = if style == "pixel" {
        "The story is shown as retro 8-bit pixel-art pictures. Give each character a one-sentence \"look\" (age, hair, clothes, \
         colours) that stays identical in every picture, and an \"art\" sentence for the pixel-art look of the whole series \
         (palette, lighting, mood). Pick a \"narrator_voice\": if the story is told in the first person by its hero, use the hero's \
         own voice; otherwise a fitting storyteller voice. Give \"narrator_pace\" 0.95-1.12 (above 1 = slower: suspense and horror \
         about 1.07) and each character a \"pace\" 0.95-1.08."
            .to_string()
    } else if fast {
        format!(
            "Every character is drawn by code and must use one of these kinds: {}. People use person_man, person_woman, person_kid or person_old; \
             talking objects and animals use their own kind. Give each a vivid main \"color\" (#RRGGBB: clothes for people, body for objects and animals).",
            KINDS.join(", ")
        )
    } else {
        "Give each character a one-sentence \"look\" for an illustrator (age, hair, clothes, colours) that stays identical in every picture, \
         and an \"art\" style for the whole series (e.g. \"flat vector cartoon illustration, bold black outlines, vibrant colors\")."
            .to_string()
    };
    let voices: Vec<String> = VOICES.iter().filter(|v| v.0 != s.story.narrator_voice).map(|(v, d)| format!("{v} ({d})")).collect();
    let mut v = llm::generate_json(s, "script", &format!(
        "Create an original animated story series for YouTube Shorts, told in {parts} parts of about {} seconds.\nIdea: {idea}\nChannel: {}\n\
         2-4 characters plus a narrator (the narrator is not a character). {cast_rule}\n\
         Give every character a different voice from: {}.\nNo copyrighted characters or brands.\n\
         Return ONLY JSON: {{\"title\": series title, \"premise\": 3 sentences covering the whole arc and how it ends, \"art\": art style, \
         \"narrator_voice\": a voice from the list, \"narrator_pace\": number, \
         \"characters\": [{{\"name\", \"kind\", \"look\", \"voice\", \"color\", \"pace\"}}]}}\n\
         {}",
        s.story.seconds, s.channel_about, voices.join(", "),
        if parts > 8 { "This is a long-running series: plan an arc with room for many escalating parts, new twists and recurring characters." } else { "" }
    ))?;
    let mut used_voices = vec![s.story.narrator_voice.clone()];
    let mut characters: Vec<Character> = vec![];
    for (i, c) in v["characters"].as_array_mut().into_iter().flatten().enumerate() {
        let name = c["name"].as_str().unwrap_or("").trim().to_string();
        if name.is_empty() || name.eq_ignore_ascii_case("narrator") || characters.iter().any(|x| x.name.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let kind = c["kind"].as_str().unwrap_or("").to_string();
        let kind = if KINDS.contains(&kind.as_str()) { kind } else { "person_man".into() };
        let voice = c["voice"].as_str().unwrap_or("").to_string();
        let voice = if VOICES.iter().any(|v| v.0 == voice) && !used_voices.contains(&voice) {
            voice
        } else {
            VOICES.iter().map(|v| v.0.to_string()).find(|v| !used_voices.contains(v)).unwrap_or_else(|| "am_michael".into())
        };
        used_voices.push(voice.clone());
        let color = c["color"].as_str().filter(|x| x.len() == 7 && x.starts_with('#')).unwrap_or("#f4b400").to_string();
        let pace = c["pace"].as_f64().unwrap_or(1.0).clamp(0.9, 1.15);
        characters.push(Character { name, kind, look: c["look"].as_str().unwrap_or("").into(), voice, color, caption: CAPTION[i % CAPTION.len()].into(), pace });
    }
    if characters.is_empty() {
        bail!("the AI did not create any characters; try again");
    }
    let title = v["title"].as_str().filter(|t| !t.trim().is_empty()).unwrap_or(idea).trim().to_string();
    let now = youtube::now_secs();
    let se = Series {
        id: format!("series-{}-{:x}", slug(&title), now % 0xff_ffff),
        title,
        premise: v["premise"].as_str().unwrap_or(idea).into(),
        style: style.into(),
        art: v["art"].as_str().filter(|a| !a.trim().is_empty()).unwrap_or("flat vector cartoon illustration, bold black outlines, vibrant colors, comic style").into(),
        narrator_voice: v["narrator_voice"].as_str().filter(|x| crate::tools::KOKORO_VOICES.contains(x)).unwrap_or("").into(),
        narrator_pace: v["narrator_pace"].as_f64().unwrap_or(1.0).clamp(0.9, 1.15),
        characters,
        parts: vec![],
        planned: parts,
        created: now,
    };
    save(&se)?;
    Ok(se)
}

/// Writes the script of part `n`, continuing the storyline from the earlier parts' summaries.
fn write_part(s: &Settings, se: &Series, n: u32) -> Result<Value> {
    if se.style == "pixel" {
        return write_pixel_part(s, se, n);
    }
    let fast = se.style == "fast";
    let secs = s.story.seconds.clamp(20, 90);
    let words = secs * 23 / 10;
    let scenes = (secs / 6).clamp(4, 9);
    let cast: Vec<String> = se.characters.iter().map(|c| if fast { format!("- {} ({})", c.name, c.kind.replace('_', " ")) } else { format!("- {}: {}", c.name, c.look) }).collect();
    let so_far: Vec<String> = se.parts.iter().map(|p| format!("Part {}: {} (ended on: {})", p.n, p.summary, p.cliffhanger)).collect();
    let ending = if n >= se.planned {
        "This is the FINAL part: resolve the story with a satisfying, funny or surprising payoff.".to_string()
    } else {
        format!("End on a cliffhanger that makes viewers need part {} (do not say \"part {}\" out loud; just leave the tension).", n + 1, n + 1)
    };
    let opening = if n == 1 { "Open with a scroll-stopping hook line." } else { "Open with a one-sentence recap that also hooks new viewers, then continue exactly where the last part ended." };
    let scene_fields = if fast {
        format!(
            "\"bg\": one of {}, \"cast\": [1-3 characters on screen: {{\"name\", \"at\": \"left\" | \"center\" | \"right\"}}], \
             \"action\": {{\"who\": a character on screen, \"move\": one of {}}} or null,",
            BACKGROUNDS.join(" | "), MOVES.join(" | ")
        )
    } else {
        "\"image\": what the picture shows at this moment: who is in it, what they do, their expressions and the place (no text, no speech bubbles),".to_string()
    };
    let prompt = format!(
        "Write part {n} of {} of the animated story series \"{}\" for YouTube Shorts.\nPremise: {}\nCharacters (plus a narrator):\n{}\nStory so far:\n{}\n\
         About {words} spoken words in {scenes} scenes (~{secs} seconds). {opening} Continue the storyline exactly and keep every character consistent. \
         Mostly character dialogue, with the narrator linking scenes; short lines; genuinely funny or gripping; every scene moves the story. {ending}\n\
         Return ONLY JSON: {{\"title\": max 60 characters, ending with \" (Part {n})\", \"description\": 2 sentences then 3-5 hashtags including #shorts, \
         \"tags\": [10-15 search tags], \"hook\": on-screen headline, max 6 words, \"thumbnail_text\": 2-4 words in ALL CAPS, \
         \"mood\": music mood: epic | mysterious | tense | uplifting | calm | playful, \"comment\": a first comment asking viewers what happens next, \
         \"summary\": 2 sentences of what happened in this part, \"cliffhanger\": the open question it ends on, \
         \"scenes\": [{{ {scene_fields} \"camera\": one of {}, \"sfx\": one of {}, \
         \"lines\": [{{\"speaker\": \"narrator\" or a character name, \"text\": spoken words, \"mood\": the speaker's emotion: one of {} \
         (\"dead\" ONLY when that character is knocked out, broken or switched off for good; a dry or deadpan line is \"neutral\" or \"smug\")}}] }}]}}",
        se.planned, se.title, se.premise, cast.join("\n"), if so_far.is_empty() { "(this is the first part)".to_string() } else { so_far.join("\n") },
        CAMERAS.join(" | "), SFX.join(" | "), MOODS.join(" | ")
    );
    let mut script = llm::generate_json(s, "script", &prompt)?;
    clean_script(&mut script, se)?;
    script["story"] = json!({"series": se.id, "series_title": se.title, "part": n, "style": se.style, "art": se.art});
    script["cast"] = serde_json::to_value(&se.characters)?;
    script["cta"] = false.into();
    Ok(script)
}

/// 8-bit style: a narrated story told over pixel-art pictures (the prompt that wrote the approved samples).
fn write_pixel_part(s: &Settings, se: &Series, n: u32) -> Result<Value> {
    let secs = s.story.seconds.clamp(25, 90);
    let words = secs * 23 / 10;
    let cast: Vec<String> = se.characters.iter().map(|c| format!("- {}: {}", c.name, c.look)).collect();
    let so_far: Vec<String> = se.parts.iter().map(|p| format!("Part {}: {} (ended on: {})", p.n, p.summary, p.cliffhanger)).collect();
    let (ending, ask) = if n >= se.planned {
        ("This is the FINAL part: resolve the story with a satisfying, surprising payoff.".to_string(), "Subscribe for the next story.".to_string())
    } else {
        ("The narrator's last story line is a cliffhanger that makes viewers need the next part.".to_string(), format!("Subscribe to see part {}.", words_for(n + 1)))
    };
    let opening = if n == 1 { "Hook the viewer in the first line." } else { "Open with a one-line recap that also hooks new viewers, then continue exactly where the last part ended." };
    let prompt = format!(
        "You write viral YouTube Shorts told as a retro 8-bit pixel-art story with a narrator. Write PART {n} of {} of the series \"{}\".\n\
         Premise: {}\nCharacters:\n{}\nStory so far:\n{}\n\
         {opening} Advertiser friendly: suspense, mystery and danger are fine, but NO gore, NO blood, NO graphic violence; for teens and adults.\n\
         Writing rules: about {words} spoken words in total (about {secs} seconds) over 6 or 7 scenes. Mostly narration (first person if the series is told by \
         its hero) plus short character lines. Short, vivid sentences; use contractions naturally (you're, we're, it's, didn't). Every scene pushes the story. \
         Keep every character consistent. {ending} Then the narrator says exactly: \"{ask}\"\n\
         Return ONLY a JSON object:\n\
         {{\"title\": YouTube title, max 60 characters, ending with \"(Part {n})\", \"hook\": on-screen headline for the first 2 seconds, max 5 words, ALL CAPS, \
         \"description\": 2 sentences then 3-5 hashtags including #shorts, \"tags\": [10-15 search tags], \"thumbnail_text\": 2-4 words ALL CAPS, \
         \"comment\": a first comment asking viewers what happens next, \"mood\": music mood: epic | mysterious | tense | uplifting | calm | playful, \
         \"summary\": 2 sentences of what happened in this part, \"cliffhanger\": the open question it ends on, \
         \"pronounce\": {{a name or unusual word the voice might misread: a simple phonetic spelling, e.g. \"Eli\": \"Ee-lie\"}} (only names that need it), \
         \"scenes\": [{{\"image\": what the picture shows at this exact moment, framed for a vertical phone screen: setting, who is in it, pose, expression, \
         lighting; no text, no speech bubbles, \"camera\": one of {}, \"sfx\": one of {}, \
         \"lines\": [{{\"speaker\": \"narrator\" or a character name, \"text\": spoken words, \"mood\": one of {}}}]}}]}}",
        se.planned, se.title, se.premise, cast.join("\n"), if so_far.is_empty() { "(this is the first part)".to_string() } else { so_far.join("\n") },
        CAMERAS.join(" | "), PIXEL_SFX.join(" | "), PIXEL_MOODS.join(" | ")
    );
    let mut script = llm::generate_json(s, "script", &prompt)?;
    clean_script(&mut script, se)?;
    // Pronunciation help for the voice only (captions keep the real spelling): short, plain respellings.
    let mut say = serde_json::Map::new();
    for (k, v) in script["pronounce"].as_object().into_iter().flatten().take(12) {
        let ok = |x: &str, max: usize| x.chars().count() <= max && !x.trim().is_empty() && x.chars().all(|c| c.is_alphabetic() || c == '-' || c == ' ' || c == '\'');
        if let Some(v) = v.as_str().filter(|v| ok(v, 40)) {
            if ok(k, 30) {
                say.insert(k.trim().to_string(), Value::from(v.trim()));
            }
        }
    }
    script["pronounce"] = Value::Object(say);
    script["story"] = json!({"series": se.id, "series_title": se.title, "part": n, "style": "pixel", "art": se.art,
                             "narrator_voice": se.narrator_voice, "narrator_pace": se.narrator_pace});
    script["cast"] = serde_json::to_value(&se.characters)?;
    script["cta"] = false.into();
    Ok(script)
}

/// "two", "three"… for the spoken ask (the voice reads words more naturally than digits).
fn words_for(n: u32) -> String {
    const W: [&str; 21] = ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen",
        "fifteen", "sixteen", "seventeen", "eighteen", "nineteen", "twenty"];
    W.get(n as usize).map(|w| w.to_string()).unwrap_or_else(|| n.to_string())
}

/// Keeps the AI's script inside what the renderers support (unknown names become the narrator, unknown choices a default).
fn clean_script(script: &mut Value, se: &Series) -> Result<()> {
    let pick = |v: &Value, allowed: &[&str], default: &str| -> Value {
        let x = v.as_str().unwrap_or("");
        Value::from(if allowed.contains(&x) { x } else { default })
    };
    let names: Vec<&str> = se.characters.iter().map(|c| c.name.as_str()).collect();
    let speaker = |x: &str| names.iter().find(|n| n.eq_ignore_ascii_case(x.trim())).map(|n| n.to_string()).unwrap_or_else(|| "narrator".into());
    let mut narration = vec![];
    let scenes = script["scenes"].as_array_mut().context("the AI returned no scenes")?;
    scenes.retain(|sc| sc["lines"].as_array().is_some_and(|l| l.iter().any(|x| x["text"].as_str().is_some_and(|t| !t.trim().is_empty()))));
    if scenes.is_empty() {
        bail!("the AI returned no spoken lines");
    }
    let pixel = se.style == "pixel";
    let (sfx_ok, mood_ok, mood_default) = if pixel { (PIXEL_SFX, PIXEL_MOODS, "calm") } else { (SFX, MOODS, "neutral") };
    for sc in scenes.iter_mut() {
        sc["camera"] = pick(&sc["camera"], CAMERAS, "push_in");
        sc["sfx"] = pick(&sc["sfx"], sfx_ok, "none");
        sc["bg"] = pick(&sc["bg"], BACKGROUNDS, "living_room");
        let mut cast: Vec<Value> = vec![];
        for c in sc["cast"].as_array().into_iter().flatten() {
            let n = speaker(c["name"].as_str().unwrap_or(""));
            if n != "narrator" && !cast.iter().any(|x| x["name"] == n.as_str()) && cast.len() < 3 {
                cast.push(json!({"name": n, "at": pick(&c["at"], &["left", "center", "right"], "center")}));
            }
        }
        sc["cast"] = cast.into();
        if let Some(a) = sc["action"].as_object() {
            let who = speaker(a.get("who").and_then(|w| w.as_str()).unwrap_or(""));
            sc["action"] = if who == "narrator" { Value::Null } else { json!({"who": who, "move": pick(a.get("move").unwrap_or(&Value::Null), MOVES, "bounce")}) };
        } else {
            sc["action"] = Value::Null;
        }
        let lines: Vec<Value> = sc["lines"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| {
                let text = l["text"].as_str()?.split_whitespace().collect::<Vec<_>>().join(" ");
                (!text.is_empty()).then(|| {
                    narration.push(text.clone());
                    json!({"speaker": speaker(l["speaker"].as_str().unwrap_or("")), "text": text, "mood": pick(&l["mood"], mood_ok, mood_default)})
                })
            })
            .collect();
        sc["lines"] = lines.into();
    }
    script["narration"] = narration.join(" ").into();
    Ok(())
}

/// Writes the next `count` parts of a series and queues each as a job. Returns the job ids (fewer if the story ends).
pub fn queue_parts(s: &Settings, series_id: &str, count: u32, log: &dyn Fn(&str)) -> Result<Vec<String>> {
    let mut se = load(series_id)?;
    let mut ids = vec![];
    for _ in 0..count {
        if se.finished() {
            break;
        }
        let n = se.parts.len() as u32 + 1;
        log(&format!("Writing “{}” part {n} of {}…", se.title, se.planned));
        let script = write_part(s, &se, n)?;
        let title = script["title"].as_str().filter(|t| !t.trim().is_empty()).map(String::from).unwrap_or_else(|| format!("{} (Part {n})", se.title));
        let id = pipeline::create_job(&title, &[])?;
        fs::write(pipeline::job_dir(&id)?.join("script.json"), serde_json::to_string_pretty(&script)?)?;
        se.parts.push(Part { n, job_id: id.clone(), title, summary: script["summary"].as_str().unwrap_or("").into(), cliffhanger: script["cliffhanger"].as_str().unwrap_or("").into() });
        save(&se)?;
        ids.push(id);
    }
    Ok(ids)
}

/// Story parts for Autopilot / ⚡ Make N: continue the newest unfinished series; start new ones from the research
/// ideas when they run out.
pub fn next_parts(s: &Settings, n: usize, log: &dyn Fn(&str)) -> Result<Vec<String>> {
    let mut ids: Vec<String> = vec![];
    let mut fresh_ideas = 0;
    while ids.len() < n {
        let open = list().into_iter().find(|x| !x.finished());
        let se = match open {
            Some(se) => se,
            None => {
                if fresh_ideas >= 2 {
                    break; // never loop on an AI that keeps failing
                }
                fresh_ideas += 1;
                let found = match last_research() {
                    Some(r) => r,
                    None => research(s, "", log)?,
                };
                let taken: Vec<String> = list().into_iter().map(|x| x.title.to_lowercase()).collect();
                let idea = found["ideas"].as_array().into_iter().flatten().find(|i| !taken.contains(&i["title"].as_str().unwrap_or("").to_lowercase())).cloned();
                let idea = match idea {
                    Some(i) => format!("{}: {}", i["title"].as_str().unwrap_or(""), i["premise"].as_str().unwrap_or("")),
                    None => research(s, "", log)?["ideas"][0]["premise"].as_str().unwrap_or("").to_string(),
                };
                if idea.trim().is_empty() {
                    bail!("no story ideas yet: open Stories and press Find popular stories");
                }
                log("Starting a new story series…");
                new_series(s, &idea, &s.story.style)?
            }
        };
        let more = queue_parts(s, &se.id, (n - ids.len()) as u32, log)?;
        if more.is_empty() {
            break;
        }
        ids.extend(more);
    }
    Ok(ids)
}

/// True when a job is a story part.
pub fn is_story(dir: &Path) -> bool {
    fs::read_to_string(dir.join("script.json")).ok().and_then(|x| serde_json::from_str::<Value>(&x).ok()).is_some_and(|v| v["story"].is_object())
}

/// Builds a story part: voices (checked by ear with Whisper), pictures (Slow and 8-bit), the animated page, the
/// render and the thumbnail. Reuses finished files, so Retry after a failure or Cancel continues where it stopped.
pub fn build(s: &Settings, id: &str, log: &dyn Fn(&str)) -> Result<()> {
    let dir = pipeline::job_dir(id)?;
    let script: Value = serde_json::from_str(&fs::read_to_string(dir.join("script.json"))?)?;
    let style = script["story"]["style"].as_str().unwrap_or("fast").to_string();
    let (fast, pixel) = (style == "fast", style == "pixel");
    let cast: Vec<Character> = serde_json::from_value(script["cast"].clone()).unwrap_or_default();
    let scenes = script["scenes"].as_array().context("story has no scenes")?.clone();
    let lines: Vec<(usize, String, String, String)> = scenes
        .iter()
        .enumerate()
        .flat_map(|(k, sc)| sc["lines"].as_array().cloned().unwrap_or_default().into_iter().map(move |l| (k, l)))
        .map(|(k, l)| (k, l["speaker"].as_str().unwrap_or("narrator").to_string(), l["text"].as_str().unwrap_or("").to_string(), l["mood"].as_str().unwrap_or("neutral").to_string()))
        .collect();
    let steps = if fast { 4 } else { 5 };

    // 1) Voices: one Kokoro voice (and pace) per character, four lines at a time.
    pipeline::stage(id, 1, steps, &format!("Recording {} voice lines…", lines.len()), log);
    let audio = dir.join("audio");
    fs::create_dir_all(&audio)?;
    // Download the voice engine and model once, before the workers start (they would race to unpack the same archive).
    tools::tts_engine(log, false)?;
    tools::kokoro_model(log)?;
    let narrator = (
        script["story"]["narrator_voice"].as_str().filter(|v| !v.is_empty()).unwrap_or(&s.story.narrator_voice).to_string(),
        script["story"]["narrator_pace"].as_f64().filter(|p| *p > 0.0).unwrap_or(1.0),
    );
    let voice_of = |speaker: &str| -> (String, f64) {
        cast.iter().find(|c| c.name == speaker).map(|c| (c.voice.clone(), if c.pace > 0.0 { c.pace } else { 1.0 })).unwrap_or_else(|| narrator.clone())
    };
    let pronounce: Vec<(String, String)> = script["pronounce"].as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect();
    let spoken = |text: &str| -> String { pronounce.iter().fold(media::speakable(text), |t, (w, say)| replace_word(&t, w, say)) };
    let make = |i: usize, pace_scale: f64, overwrite: bool| -> Result<()> {
        let (voice, pace) = voice_of(&lines[i].1);
        voice_line(s, &audio, i + 1, &voice, &spoken(&lines[i].2), pace * pace_scale, overwrite)
    };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);
    std::thread::scope(|sc| {
        for _ in 0..4 {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= lines.len() || failed.lock().unwrap().is_some() {
                    break;
                }
                if let Err(e) = make(i, 1.0, false) {
                    *failed.lock().unwrap() = Some(e);
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap() {
        return Err(e);
    }

    // 2) The listening check: Whisper writes down what it hears and every line is compared with the script, word by
    // word. A line that does not sound right is re-recorded (a little slower, then a little faster); the best take stays.
    pipeline::stage(id, 2, steps, "Listening to every line…", log);
    let all: Vec<usize> = (0..lines.len()).collect();
    match listen(&dir, &lines, &all, log) {
        Ok(mut scores) => {
            for (round, scale) in [1.06, 0.96].into_iter().enumerate() {
                let bad: Vec<usize> = all.iter().copied().filter(|i| scores[*i] < 0.9).collect();
                if bad.is_empty() {
                    break;
                }
                llm::check_cancel()?;
                log(&format!("  re-recording {} line(s) that did not sound right (try {})…", bad.len(), round + 1));
                for &i in &bad {
                    fs::copy(audio.join(format!("l{}.wav", i + 1)), audio.join(format!("best{}.wav", i + 1)))?;
                    make(i, scale, true)?;
                }
                let again = listen(&dir, &lines, &bad, log)?;
                for &i in &bad {
                    let best = audio.join(format!("best{}.wav", i + 1));
                    if again[i] >= scores[i] {
                        scores[i] = again[i];
                        let _ = fs::remove_file(&best);
                    } else {
                        fs::rename(&best, audio.join(format!("l{}.wav", i + 1)))?;
                    }
                }
            }
            let weak = scores.iter().filter(|x| **x < 0.9).count();
            log(&format!("  voice check: {} of {} lines clear{}", lines.len() - weak, lines.len(), if weak > 0 { " (the rest kept as their best take)" } else { "" }));
            let _ = fs::write(audio.join("check.json"), serde_json::to_string(&scores)?);
        }
        Err(e) => log(&format!("  voice check skipped ({e:#})")),
    }
    let durs: Vec<f64> = (1..=lines.len()).map(|n| media::duration(&audio.join(format!("l{n}.wav")))).collect::<Result<_>>()?;

    // 3) Pictures (Slow and 8-bit): one AI picture per scene, with the series art style and the looks of who is in it.
    if !fast {
        let art = script["story"]["art"].as_str().unwrap_or("flat vector cartoon illustration, bold black outlines, vibrant colors");
        for (k, sc) in scenes.iter().enumerate() {
            let out = dir.join(format!("scene_{}.png", k + 1));
            if out.exists() {
                continue;
            }
            llm::check_cancel()?;
            pipeline::stage(id, 3, steps, &format!("Painting picture {}/{}…", k + 1, scenes.len()), log);
            let what = sc["image"].as_str().unwrap_or("");
            let speakers: Vec<&str> = sc["lines"].as_array().into_iter().flatten().filter_map(|l| l["speaker"].as_str()).collect();
            let looks: Vec<String> = cast.iter().filter(|c| what.contains(&c.name) || speakers.contains(&c.name.as_str())).map(|c| format!("{}: {}", c.name, c.look)).collect();
            let (prompt, negative, seed) = if pixel {
                (format!("pixel art, 16-bit retro video game cutscene, crisp chunky pixels, limited color palette, {art}. One single full-screen scene, ONE camera shot, vertical composition. {}. {what}", looks.join(". ")),
                 Some(PIXEL_NEGATIVE), 4200 + k as u64)
            } else {
                (format!("{art}, one single full-frame illustration of one moment, vertical composition. {} {what}. No text, no letters, no speech bubbles, no panels.", looks.join(". ")), None, 1000 + k as u64)
            };
            let target = if pixel { dir.join(format!("raw_{}.png", k + 1)) } else { out.clone() };
            let r = if s.story.images == "gemini" { media::gemini_image_ratio(s, &prompt, "9:16", &target) } else { tools::comfy_image(&prompt, negative, seed, &target, log) };
            if let Err(e) = r {
                if s.story.images == "gemini" && tools::comfy_ready() {
                    log(&format!("  Gemini picture failed ({e:#}); painting it locally"));
                    tools::comfy_image(&prompt, negative, seed, &target, log)?;
                } else {
                    return Err(e);
                }
            }
            if pixel {
                // True pixel art: down to a 216x384 grid, a 32-colour palette without dithering, then 5x nearest-neighbour
                // to exactly 1080x1920 (crisp pixels, no blur).
                let raw = format!("raw_{}.png", k + 1);
                media::ffmpeg(&dir, &["-i", &raw, "-vf", "scale=216:384:force_original_aspect_ratio=increase:flags=area,crop=216:384,split[a][b];[a]palettegen=max_colors=32:stats_mode=full[p];[b][p]paletteuse=dither=none,scale=1080:1920:flags=neighbor", &format!("scene_{}.png", k + 1)])?;
                let _ = fs::remove_file(dir.join(raw));
            }
        }
    }

    // 4) The animated page.
    pipeline::stage(id, steps - 1, steps, "Animating…", log);
    let (data, starts) = if pixel { timeline_pixel(&script, &scenes, &lines, &durs, &cast) } else { timeline(&script, &scenes, &lines, &durs, &cast, fast) };
    let total = data["total"].as_f64().unwrap_or(30.0);
    let mood = script["mood"].as_str().unwrap_or("playful");
    let mut tags = vec![];
    if let Some((track, credit)) = media::pick_music(s, mood) {
        let ext = track.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "mp3".into());
        fs::copy(&track, dir.join(format!("music.{ext}")))?;
        tags.push(format!("<audio src=\"music.{ext}\" data-volume=\"0.22\" data-duck=\"0.25\" data-fade-in=\"1\" data-fade-out=\"1.5\" loop></audio>"));
        if credit.is_empty() { drop(fs::remove_file(dir.join("music_credit.txt"))) } else { fs::write(dir.join("music_credit.txt"), credit)? }
    }
    // Lines are loudness-matched when recorded; characters get a small lift over the narrator.
    for (n, start) in starts.iter().enumerate() {
        let vol = if lines[n].1 == "narrator" { "1.0" } else { "1.25" };
        tags.push(format!("<audio id=\"a{}\" src=\"audio/l{}.wav\" data-start=\"{start:.3}\" data-volume=\"{vol}\"></audio>", n + 1, n + 1));
    }
    // Effects hit in the pause before a scene, so they never sit on top of its first words.
    let sfx_dir = dir.join("sfx");
    for sc in data["scenes"].as_array().into_iter().flatten() {
        let fx = sc["sfx"].as_str().unwrap_or("none");
        if fx != "none" {
            make_sfx(&sfx_dir, fx)?;
            tags.push(format!("<audio src=\"sfx/{fx}.wav\" data-start=\"{:.3}\" data-volume=\"0.38\"></audio>", (sc["start"].as_f64().unwrap_or(0.0) - 0.38).max(0.0)));
        }
    }
    let shot_list: Vec<Value> = data["scenes"].as_array().cloned().unwrap_or_default();
    let shots: String = if fast {
        String::new()
    } else {
        shot_list
            .iter()
            .enumerate()
            .map(|(k, sc)| {
                let (a, b) = (sc["start"].as_f64().unwrap_or(0.0), sc["end"].as_f64().unwrap_or(0.0));
                // The last picture stays to the very end (the subscribe card is shown over it).
                let d = if pixel && k + 1 == shot_list.len() { total - a } else { b - a + 0.35 };
                format!("<section class=\"shot\" id=\"s{k}\" data-start=\"{a:.3}\" data-duration=\"{d:.3}\"><div class=\"wrap\"><img src=\"scene_{}.png\" alt=\"\"></div></section>\n", k + 1)
            })
            .collect()
    };
    if pixel {
        fs::create_dir_all(dir.join("fonts"))?;
        for (name, bytes) in PIXEL_FONTS {
            fs::write(dir.join("fonts").join(name), bytes)?;
        }
    }
    // The data goes into a <script>: keep "</script>" from ever closing it early.
    let data_js = serde_json::to_string(&data)?.replace("</", "<\\/");
    let page = if pixel { PIXEL_PAGE } else if fast { FAST_PAGE } else { SLOW_PAGE }
        .replace("{{TOTAL}}", &format!("{total:.2}"))
        .replace("{{AUDIO}}", &tags.join("\n"))
        .replace("{{SHOTS}}", &shots)
        .replace("{{DATA}}", &data_js);
    fs::write(dir.join("story.html"), page)?;

    // 5) Render, compress (the animator's own file is ~5x bigger than needed), then the thumbnail.
    llm::check_cancel()?;
    pipeline::stage(id, steps, steps, &format!("Rendering the {total:.0}s video…"), log);
    let started = std::time::Instant::now();
    let _ = fs::remove_file(dir.join("short.mp4"));
    tools::givemotion_render(&dir, "story.html", "render.mp4")?;
    media::encode(s, &dir, vec!["-i".into(), "render.mp4".into()], &["-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", "short.mp4"])?;
    let _ = fs::remove_file(dir.join("render.mp4"));
    log(&format!("  rendered in {:.0}s", started.elapsed().as_secs_f64()));
    if fast {
        let t = starts.get(1).copied().unwrap_or(3.5).max(3.2);
        tools::givemotion_still(&dir, "story.html", t, "scene_1.png")?;
    }
    media::thumbnail(&dir, script["thumbnail_text"].as_str().unwrap_or(""))?;
    log("Story part ready ✔");
    Ok(())
}

/// `word` replaced by `with` wherever it stands as a whole word (so "Eli" changes but "Elizabeth" does not).
fn replace_word(text: &str, word: &str, with: &str) -> String {
    if word.is_empty() {
        return text.to_string();
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(pos) = rest.find(word) {
        let before = rest[..pos].chars().next_back();
        let after = rest[pos + word.len()..].chars().next();
        let whole = !before.is_some_and(|c| c.is_alphanumeric()) && !after.is_some_and(|c| c.is_alphanumeric());
        out.push_str(&rest[..pos]);
        out.push_str(if whole { with } else { word });
        rest = &rest[pos + word.len()..];
    }
    out.push_str(rest);
    out
}

/// One voice line: Kokoro (CPU, so several run side by side) into audio/l{n}.wav, silence trimmed at both ends and the
/// loudness matched (so every line sits at the same level).
fn voice_line(s: &Settings, audio: &Path, n: usize, voice: &str, text: &str, pace: f64, overwrite: bool) -> Result<()> {
    let out = audio.join(format!("l{n}.wav"));
    if out.exists() && !overwrite {
        return Ok(());
    }
    let tmp = audio.join(format!("tmp{n}"));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    fs::write(tmp.join("narration.txt"), text)?;
    let mut s2 = s.clone();
    (s2.voice, s2.kokoro_voice, s2.kokoro_device, s2.kokoro_pace) = ("kokoro".into(), voice.into(), "cpu".into(), pace);
    media::voice(&s2, &tmp, &|_| {})?;
    media::ffmpeg(&tmp, &["-i", "voice.wav", "-af", "areverse,silenceremove=start_periods=1:start_threshold=-50dB:start_silence=0.08,areverse,loudnorm=I=-16:TP=-1.5:LRA=7", "-ar", "44100", "line.wav"])?;
    fs::rename(tmp.join("line.wav"), &out)?;
    let _ = fs::remove_dir_all(&tmp);
    Ok(())
}

/// Whisper listens to the chosen lines in one pass (joined with gaps, so the model loads once) and every line gets a
/// score: the share of the script's words it heard, in order. Returns a score for every line (1.0 for lines not checked).
fn listen(dir: &Path, lines: &[(usize, String, String, String)], which: &[usize], log: &dyn Fn(&str)) -> Result<Vec<f64>> {
    const PAD: f64 = 1.2;
    let mut scores = vec![1.0; lines.len()];
    if which.is_empty() {
        return Ok(scores);
    }
    let mut args: Vec<String> = vec![];
    let mut filter = String::new();
    let mut spans = vec![];
    let mut at = 0.0;
    for (j, &i) in which.iter().enumerate() {
        let f = format!("audio/l{}.wav", i + 1);
        let d = media::duration(&dir.join(&f))?;
        spans.push((at, at + d));
        at += d + PAD;
        args.extend(["-i".into(), f]);
        filter.push_str(&format!("[{j}:a]aformat=sample_rates=16000:channel_layouts=mono,apad=pad_dur={PAD}[a{j}];"));
    }
    filter.push_str(&(0..which.len()).map(|j| format!("[a{j}]")).collect::<String>());
    filter.push_str(&format!("concat=n={}:v=0:a=1[out]", which.len()));
    args.extend(["-filter_complex".into(), filter, "-map".into(), "[out]".into(), "audio/check.wav".into()]);
    media::ffmpeg(dir, &args.iter().map(String::as_str).collect::<Vec<_>>())?;
    let heard = tools::transcribe_chunked(dir, "audio/check.wav", "audio/check.srt", "../.aiyt-tools/ggml-base.bin", "en", 20, log)?;
    let _ = fs::remove_file(dir.join("audio").join("check.wav"));
    for (j, &i) in which.iter().enumerate() {
        let (a, b) = spans[j];
        let text: Vec<&str> = heard.iter().filter(|(s0, e0, _)| (s0 + e0) / 2.0 >= a - 0.3 && (s0 + e0) / 2.0 < b + PAD * 0.75).map(|(_, _, t)| t.as_str()).collect();
        let got = text.join(" ");
        // A pass counts as at least 0.9 (the retake threshold); a fail keeps its real score.
        scores[i] = if line_ok(&lines[i].2, &got) { similar(&lines[i].2, &got).max(0.9) } else { similar(&lines[i].2, &got).min(0.89) };
    }
    Ok(scores)
}

/// Words as heard: lower case, no punctuation, contractions spelled out, and the spellings Whisper picks for words that
/// sound the same (your/you're, were/we're, their/they're, C/see, digits) made equal.
fn heard_words(text: &str) -> Vec<String> {
    let mut t = media::speakable(text).to_lowercase();
    for (a, b) in [("you're", "you are"), ("we're", "we are"), ("they're", "they are"), ("i'm", "i am"), ("it's", "it is"), ("don't", "do not"), ("didn't", "did not"),
        ("doesn't", "does not"), ("won't", "will not"), ("can't", "cannot"), ("i'd", "i would"), ("it'll", "it will"), ("i've", "i have"), ("that's", "that is")] {
        t = t.replace(a, b);
    }
    let t: String = t.chars().map(|c| if c.is_ascii_alphanumeric() || c == '\'' { c } else { ' ' }).collect();
    let alike = |w: &str| -> String {
        match w {
            "your" => "you are", "were" => "we are", "their" | "there" => "they are", "c" => "see", "0" => "zero", "1" => "one", "2" => "two", "3" => "three",
            "4" => "four", "5" => "five", "6" => "six", "7" => "seven", "8" => "eight", "9" => "nine", "10" => "ten",
            other => other,
        }
        .to_string()
    };
    const NUMBERS: &[&str] = &["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen",
        "fifteen", "sixteen", "seventeen", "eighteen", "nineteen", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety", "hundred", "thousand"];
    t.split_whitespace()
        .flat_map(|w| alike(w).split(' ').map(String::from).collect::<Vec<_>>())
        .flat_map(|w| w.split('-').map(String::from).collect::<Vec<_>>())
        .filter(|w| !w.is_empty() && !w.chars().all(|c| c.is_ascii_digit()) && !NUMBERS.contains(&w.as_str()))
        .collect()
}

/// A line sounds right when it misses at most ~15% of its words (and always allows one miss: one misheard name in a
/// short line is Whisper's spelling, not a bad take).
fn line_ok(want: &str, got: &str) -> bool {
    let n = heard_words(want).len();
    if n == 0 {
        return true;
    }
    let missed = n as f64 - (similar(want, got) * n as f64).round();
    missed <= (n as f64 * 0.15).ceil().max(1.0)
}

/// Share of the wanted words that were heard, in order (longest common subsequence).
fn similar(want: &str, got: &str) -> f64 {
    let (w, g) = (heard_words(want), heard_words(got));
    if w.is_empty() {
        return 1.0;
    }
    let mut dp = vec![vec![0usize; g.len() + 1]; w.len() + 1];
    for i in 1..=w.len() {
        for k in 1..=g.len() {
            dp[i][k] = if w[i - 1] == g[k - 1] { dp[i - 1][k - 1] + 1 } else { dp[i - 1][k].max(dp[i][k - 1]) };
        }
    }
    dp[w.len()][g.len()] as f64 / w.len() as f64
}

/// 8-bit timeline: scene, line and dialogue-box page times, plus when the subscribe card appears.
fn timeline_pixel(script: &Value, scenes: &[Value], lines: &[(usize, String, String, String)], durs: &[f64], cast: &[Character]) -> (Value, Vec<f64>) {
    const LEAD: f64 = 0.35;
    const GAP: f64 = 0.2;
    const TAIL: f64 = 0.45;
    const HOLD: f64 = 1.6;
    let r3 = |x: f64| (x * 1000.0).round() / 1000.0;
    let (mut t, mut out_scenes, mut out_lines, mut pages, mut starts) = (LEAD, vec![], vec![], vec![], vec![]);
    let mut cta = Value::Null;
    for (k, sc) in scenes.iter().enumerate() {
        let start = if k == 0 { 0.0 } else { t };
        for (i, (_, speaker, text, _)) in lines.iter().enumerate().filter(|(_, l)| l.0 == k) {
            let d = durs[i];
            if speaker == "narrator" && text.to_lowercase().contains("subscribe") {
                cta = r3(t - 0.1).into();
            }
            starts.push(r3(t));
            out_lines.push(json!({"n": i + 1, "speaker": speaker, "start": r3(t), "dur": r3(d)}));
            // Words timed by length; a long line is split into pages of about 50 characters (at a comma or full stop when
            // possible) so every page fits three rows of the dialogue box.
            let ws: Vec<&str> = text.split_whitespace().collect();
            let weight: f64 = ws.iter().map(|w| w.chars().count() as f64 + 2.0).sum();
            let mut wt = t;
            let mut cur: Vec<Value> = vec![];
            let mut len = 0usize;
            for (j, w) in ws.iter().enumerate() {
                let wl = d * (w.chars().count() as f64 + 2.0) / weight;
                cur.push(json!({"text": w, "start": r3(wt), "end": r3(wt + wl)}));
                wt += wl;
                len += w.chars().count() + 1;
                let rest: usize = ws[j + 1..].iter().map(|x| x.chars().count() + 1).sum();
                let stop = w.ends_with([',', '.', ';', ':', '!', '?']);
                if len >= 50 || (len >= 28 && stop && rest > 12) || j + 1 == ws.len() {
                    pages.push(json!({"line": i + 1, "speaker": speaker, "start": cur[0]["start"], "end": cur[cur.len() - 1]["end"], "words": cur}));
                    cur = vec![];
                    len = 0;
                }
            }
            t += d + GAP;
        }
        t += TAIL - GAP;
        out_scenes.push(json!({"start": r3(start), "end": r3(t), "camera": sc["camera"], "sfx": sc["sfx"]}));
    }
    let cast_map: serde_json::Map<String, Value> = cast.iter().map(|c| (c.name.clone(), json!({"caption": c.caption}))).collect();
    let data = json!({
        "hook": script["hook"].as_str().unwrap_or(""), "total": r3(t + HOLD), "cta": cta, "cast": cast_map,
        "scenes": out_scenes, "lines": out_lines, "pages": pages,
    });
    (data, starts)
}

/// Scene, line and word times for the page. Returns (data for the template, start time of every line).
fn timeline(script: &Value, scenes: &[Value], lines: &[(usize, String, String, String)], durs: &[f64], cast: &[Character], fast: bool) -> (Value, Vec<f64>) {
    const LEAD: f64 = 0.35;
    const GAP: f64 = 0.18;
    const TAIL: f64 = 0.4;
    let r3 = |x: f64| (x * 1000.0).round() / 1000.0;
    let (mut t, mut out_scenes, mut out_lines, mut words, mut starts) = (LEAD, vec![], vec![], vec![], vec![]);
    for (k, sc) in scenes.iter().enumerate() {
        let start = if k == 0 { 0.0 } else { t };
        let mut action_at = None;
        for (i, (_, speaker, text, mood)) in lines.iter().enumerate().filter(|(_, l)| l.0 == k) {
            let d = durs[i];
            if sc["action"]["who"].as_str() == Some(speaker.as_str()) && action_at.is_none() {
                action_at = Some(t);
            }
            starts.push(r3(t));
            out_lines.push(json!({"n": i + 1, "speaker": speaker, "start": r3(t), "dur": r3(d), "mood": mood}));
            let ws: Vec<&str> = text.split_whitespace().collect();
            let weight: f64 = ws.iter().map(|w| w.chars().count() as f64 + 2.0).sum();
            let mut wt = t;
            for w in &ws {
                let len = d * (w.chars().count() as f64 + 2.0) / weight;
                words.push(json!({"text": w, "start": r3(wt), "end": r3(wt + len), "speaker": speaker}));
                wt += len;
            }
            t += d + GAP;
        }
        t += TAIL - GAP;
        let mut scene = json!({"start": r3(start), "end": r3(t), "camera": sc["camera"], "sfx": sc["sfx"]});
        if fast {
            scene["bg"] = sc["bg"].clone();
            scene["cast"] = sc["cast"].clone();
            scene["action"] = match sc["action"].as_object() {
                Some(a) => json!({"who": a["who"], "move": a["move"], "at": r3(action_at.unwrap_or(start + 0.5))}),
                None => Value::Null,
            };
        } else {
            scene["img"] = format!("scene_{}.png", k + 1).into();
        }
        out_scenes.push(scene);
    }
    let cast_map: serde_json::Map<String, Value> = cast.iter().map(|c| (c.name.clone(), json!({"kind": c.kind, "color": c.color, "caption": c.caption}))).collect();
    let data = json!({
        "hook": script["hook"].as_str().unwrap_or(""),
        "total": r3(t + 0.3),
        "cast": cast_map,
        "scenes": out_scenes,
        "lines": out_lines,
        "words": words,
    });
    (data, starts)
}

/// Sound effects are synthesised with ffmpeg: nothing to download and no licences to track.
fn make_sfx(dir: &Path, name: &str) -> Result<()> {
    let out = dir.join(format!("{name}.wav"));
    if out.exists() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    let (expr, shape, secs) = match name {
        "whoosh" => ("0.9*(random(0)-0.5)*sin(PI*t/0.55)", "bandpass=f=900:w=700", 0.55),
        "boing" => ("0.5*sin(2*PI*(180+90*sin(2*PI*11*t))*t)*exp(-3.5*t)", "lowpass=f=9000", 0.7),
        "thud" => ("0.9*sin(2*PI*(75-35*t)*t)*exp(-9*t)+0.2*(random(0)-0.5)*exp(-30*t)", "lowpass=f=9000", 0.7),
        "ding" => ("0.35*sin(2*PI*1568*t)*exp(-4*t)+0.18*sin(2*PI*2352*t)*exp(-5*t)", "lowpass=f=9000", 0.7),
        "heartbeat" => ("0.9*sin(2*PI*52*t)*(exp(-16*t)+gte(t,0.27)*exp(-16*(t-0.27)))", "lowpass=f=300", 1.0),
        "creak" => ("0.35*sin(2*PI*(170+70*sin(2*PI*2.5*t))*t)*(0.55+0.45*sin(2*PI*29*t))*exp(-1.3*t)", "lowpass=f=2500", 1.4),
        "knock" => ("0.9*sin(2*PI*110*t)*(exp(-30*t)+gte(t,0.22)*exp(-30*(t-0.22))+gte(t,0.44)*exp(-30*(t-0.44)))", "lowpass=f=600", 0.9),
        "thunder" => ("1.4*(random(0)-0.5)*exp(-1.4*t)*(1-exp(-30*t))", "lowpass=f=380", 2.4),
        "static" => ("0.45*(random(0)-0.5)*lt(t,0.55)", "highpass=f=1800", 0.6),
        "sting" => ("0.22*(sin(2*PI*220*t)+sin(2*PI*233*t)+sin(2*PI*311*t)+sin(2*PI*466*t))*exp(-2.2*t)", "lowpass=f=4000", 1.6),
        "wind" => ("0.7*(random(0)-0.5)*(0.45+0.55*sin(2*PI*0.5*t))*sin(PI*t/2.2)", "lowpass=f=700", 2.2),
        "splash" => ("1.1*(random(0)-0.5)*exp(-5*t)*(1-exp(-60*t))", "bandpass=f=1400:w=1800", 0.9),
        "hammer" => ("0.8*(sin(2*PI*820*t)+0.6*(random(0)-0.5))*(exp(-45*t)+gte(t,0.32)*exp(-45*(t-0.32)))", "highpass=f=200", 0.7),
        _ => ("0.45*sin(2*PI*(260+520*sin(2*PI*6*t))*t)*(0.6+0.4*random(0))*exp(-2.5*t)", "lowpass=f=9000", 0.7),
    };
    // The formula is quoted: its commas would otherwise split the filter.
    let fade = format!("{shape},afade=t=out:st={:.2}:d={:.2}", secs * 0.75, secs * 0.25);
    media::ffmpeg(dir, &["-f", "lavfi", "-i", &format!("aevalsrc='{expr}':s=44100:d={secs}"), "-af", &fade, &format!("{name}.wav")])
}

/// More parts for a story (it can run as long as you like). Returns the updated series.
pub fn extend(id: &str, more: u32) -> Result<Series> {
    let mut se = load(id)?;
    se.planned = (se.planned + more.clamp(1, 50)).min(200);
    save(&se)?;
    Ok(se)
}

fn slug(s: &str) -> String {
    let s: String = s.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let s: String = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-").chars().take(32).collect();
    if s.is_empty() { "story".into() } else { s.trim_end_matches('-').into() }
}

/// A real two-part story with your saved AI writer: `cargo test story_live -- --ignored --nocapture`.
/// Uses the real Videos\AIYT folder (so the installed tools are reused) with a separate test config, so the parts
/// belong to no channel and never show up in the app. STORY_STYLE = fast | slow.
#[cfg(test)]
#[test]
#[ignore]
fn story_live() {
    let saved = PathBuf::from(std::env::var("APPDATA").unwrap()).join("com.aiyt.studio/settings.json");
    let mut s: Settings = serde_json::from_str(&fs::read_to_string(saved).unwrap()).unwrap();
    s.active_channel.clear();
    let _ = crate::settings::OUT_DIR.set(PathBuf::from(std::env::var("USERPROFILE").unwrap()).join("Videos").join("AIYT"));
    let _ = crate::settings::CONFIG_DIR.set(std::env::temp_dir().join("aiyt-story-live-cfg"));
    s.music_dir = PathBuf::from(std::env::var("USERPROFILE").unwrap()).join("Videos/AIYT/_music").to_string_lossy().into();
    s.channel_about = "Talking-object comedy: everyday things in the house have opinions about their owners.".into();
    s.story.style = std::env::var("STORY_STYLE").unwrap_or_else(|_| "slow".into());
    (s.story.seconds, s.story.parts, s.story.images) = (30, 2, "local".into());
    let log = |m: &str| println!("{m}");
    tools::setup_givemotion(&log).unwrap();
    let t0 = std::time::Instant::now();
    let idea = std::env::var("STORY_IDEA").unwrap_or_else(|_| "A smart fridge that judges everything its owner eats".into());
    let se = new_series(&s, &idea, &s.story.style).unwrap();
    println!("SERIES {} | {}\n{}", se.title, se.premise, serde_json::to_string(&se.characters).unwrap());
    let ids = queue_parts(&s, &se.id, 2, &log).unwrap();
    println!("SCRIPTS {:.0}s", t0.elapsed().as_secs_f64());
    for id in &ids {
        let t = std::time::Instant::now();
        pipeline::build_job(&s, id, &log).unwrap();
        let dir = pipeline::job_dir(id).unwrap();
        assert!(dir.join("short.mp4").exists() && dir.join("thumbnail.jpg").exists());
        println!("BUILT {id} in {:.0}s -> {}", t.elapsed().as_secs_f64(), dir.display());
    }
    for p in &load(&se.id).unwrap().parts {
        println!("PART {}: {} | {} | ends on: {}", p.n, p.title, p.summary, p.cliffhanger);
    }
    tools::comfy_stop();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series() -> Series {
        Series {
            id: "series-t".into(),
            title: "T".into(),
            style: "fast".into(),
            planned: 3,
            characters: vec![Character { name: "Toaster".into(), kind: "toaster".into(), voice: "am_onyx".into(), color: "#cccccc".into(), caption: "#5ef2c2".into(), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn scripts_are_kept_inside_what_the_renderer_supports() {
        let mut v = json!({"scenes": [
            {"bg": "moon base", "camera": "drone", "sfx": "laser", "cast": [{"name": "toaster", "at": "up"}, {"name": "Ghost"}],
             "action": {"who": "Ghost", "move": "fly"},
             "lines": [{"speaker": "TOASTER", "text": "  Crumbs   are overdue. ", "mood": "furious"}, {"speaker": "Bob", "text": "Who?"}]},
            {"lines": [{"speaker": "narrator", "text": "   "}]}
        ]});
        clean_script(&mut v, &series()).unwrap();
        let sc = &v["scenes"][0];
        assert_eq!(v["scenes"].as_array().unwrap().len(), 1, "scenes with nothing to say are dropped");
        assert_eq!((sc["bg"].as_str(), sc["camera"].as_str(), sc["sfx"].as_str()), (Some("living_room"), Some("push_in"), Some("none")));
        assert_eq!(sc["cast"], json!([{"name": "Toaster", "at": "center"}]));
        assert!(sc["action"].is_null(), "an action by someone not in the cast is dropped");
        assert_eq!(sc["lines"][0], json!({"speaker": "Toaster", "text": "Crumbs are overdue.", "mood": "neutral"}));
        assert_eq!(sc["lines"][1]["speaker"], "narrator");
        assert_eq!(v["narration"], "Crumbs are overdue. Who?");
    }

    #[test]
    fn timeline_follows_the_voice_lines() {
        let scenes = vec![json!({"camera": "push_in", "sfx": "none", "bg": "kitchen", "cast": [], "action": {"who": "Toaster", "move": "shake"}}), json!({"camera": "shake", "sfx": "thud", "bg": "shop", "cast": [], "action": null})];
        let lines = vec![(0, "narrator".to_string(), "One two".to_string(), "neutral".to_string()), (0, "Toaster".to_string(), "Three".to_string(), "angry".to_string()), (1, "narrator".to_string(), "Four five six".to_string(), "happy".to_string())];
        let (d, starts) = timeline(&json!({"hook": "H"}), &scenes, &lines, &[1.0, 0.5, 2.0], &series().characters, true);
        assert_eq!(starts, vec![0.35, 1.53, 2.43]);
        assert_eq!(d["scenes"][0]["start"], 0.0);
        assert_eq!(d["scenes"][1]["start"], 2.43);
        assert_eq!(d["scenes"][0]["action"]["at"], 1.53, "the action lands when that character starts talking");
        assert_eq!(d["words"].as_array().unwrap().len(), 6);
        assert_eq!(d["words"][0]["start"], 0.35);
        assert_eq!(d["total"], 5.13);
        assert_eq!(d["cast"]["Toaster"]["kind"], "toaster");
    }
    #[test]
    fn names_are_respelled_only_as_whole_words() {
        assert_eq!(replace_word("Eli, run! Elizabeth saw Eli.", "Eli", "Ee-lie"), "Ee-lie, run! Elizabeth saw Ee-lie.");
        assert_eq!(media::speakable("You\u{2019}re home\u{2014}now..."), "You're home, now,");
        assert_eq!(words_for(2), "two");
    }

    #[test]
    fn the_listening_check_ignores_spelling_differences_but_not_wrong_words() {
        // What Whisper writes for a correctly spoken line.
        assert_eq!(similar("You\u{2019}re home early this time.", "Your home early this time."), 1.0);
        assert_eq!(similar("Subscribe to see part two.", "Subscribe to C-Part 2."), 1.0);
        assert_eq!(similar("We're late.", "were late"), 1.0);
        // What it wrote for the broken curly-apostrophe voice.
        assert!(similar("You're home early.", "Yawia Half-Free Home Early.") < 0.9);
        // Real transcripts from a generated part (Whisper with 20-second chunks).
        assert!(line_ok("Milo, move!", "My low move."));
        assert!(line_ok("Rattle flashed eleven fifty-nine. My voice whispered,", "Rattle flashed 1159, my voice whispered."));
        assert!(line_ok("On graveyard shift, Rattle crackled in my voice:", "On graveyard shift, rattle crackled in my voice."));
        assert!(!line_ok("You're home early this time.", "Yawia Half-Free Home Early."));
        assert!(!line_ok("A cleaning cart rocketed at me. I dropped; it flew overhead.", "A cleaning car."));
    }

    #[test]
    fn pixel_timeline_pages_long_lines_and_finds_the_ask() {
        let scenes = vec![json!({"camera": "push_in", "sfx": "creak"}), json!({"camera": "shake", "sfx": "none"})];
        let long = "My smart trick was trapping air inside trash bags, then stuffing them into pillowcases.";
        let lines = vec![(0, "narrator".to_string(), long.to_string(), "calm".to_string()), (1, "narrator".to_string(), "Subscribe to see part two.".to_string(), "calm".to_string())];
        let (d, starts) = timeline_pixel(&json!({"hook": "H"}), &scenes, &lines, &[4.0, 1.5], &series().characters);
        assert_eq!(starts, vec![0.35, 4.8]);
        let pages = d["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 3, "the long line is split, the short one is one page: {pages:?}");
        assert!(pages.iter().all(|p| p["words"].as_array().unwrap().iter().map(|w| w["text"].as_str().unwrap().len() + 1).sum::<usize>() <= 60));
        assert_eq!(d["cta"], 4.7);
        assert_eq!(d["total"], 4.8 + 1.5 + 0.45 + 1.6);
    }
}