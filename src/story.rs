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
    /// fast | slow
    pub style: String,
    /// Slow style: the art style added to every picture prompt.
    pub art: String,
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

/// What is working for story Shorts in this channel's niche right now, and original series ideas inspired by it.
pub fn research(s: &Settings, log: &dyn Fn(&str)) -> Result<Value> {
    let niche = if !s.niche_query.trim().is_empty() { s.niche_query.trim().to_string() } else { s.channel_about.chars().take(80).collect() };
    if niche.trim().is_empty() {
        bail!("set up this channel first (Channel setup) so the AI knows its story niche");
    }
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
        s.channel_about, if top.is_empty() { "(no live data; use your knowledge)".to_string() } else { top.join("\n") },
        if made.is_empty() { "(none)".to_string() } else { made.join("; ") }
    ))?;
    v["top_videos"] = videos.into_iter().take(12).collect::<Vec<_>>().into();
    v["niche"] = niche.into();
    write_atomic(&dir().join("research.json"), &serde_json::to_string_pretty(&v)?)?;
    Ok(v)
}

/// Turns an idea into a series: title, premise, cast with fixed looks and voices.
pub fn new_series(s: &Settings, idea: &str, style: &str) -> Result<Series> {
    let fast = style != "slow" && fast_ready();
    let parts = s.story.parts.clamp(1, 12);
    let cast_rule = if fast {
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
         \"characters\": [{{\"name\", \"kind\", \"look\", \"voice\", \"color\"}}]}}",
        s.story.seconds, s.channel_about, voices.join(", ")
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
        characters.push(Character { name, kind, look: c["look"].as_str().unwrap_or("").into(), voice, color, caption: CAPTION[i % CAPTION.len()].into() });
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
        style: if fast { "fast" } else { "slow" }.into(),
        art: v["art"].as_str().filter(|a| !a.trim().is_empty()).unwrap_or("flat vector cartoon illustration, bold black outlines, vibrant colors, comic style").into(),
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
         \"lines\": [{{\"speaker\": \"narrator\" or a character name, \"text\": spoken words, \"mood\": one of {}}}] }}]}}",
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
    for sc in scenes.iter_mut() {
        sc["camera"] = pick(&sc["camera"], CAMERAS, "push_in");
        sc["sfx"] = pick(&sc["sfx"], SFX, "none");
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
                    json!({"speaker": speaker(l["speaker"].as_str().unwrap_or("")), "text": text, "mood": pick(&l["mood"], MOODS, "neutral")})
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
                    None => research(s, log)?,
                };
                let taken: Vec<String> = list().into_iter().map(|x| x.title.to_lowercase()).collect();
                let idea = found["ideas"].as_array().into_iter().flatten().find(|i| !taken.contains(&i["title"].as_str().unwrap_or("").to_lowercase())).cloned();
                let idea = match idea {
                    Some(i) => format!("{}: {}", i["title"].as_str().unwrap_or(""), i["premise"].as_str().unwrap_or("")),
                    None => research(s, log)?["ideas"][0]["premise"].as_str().unwrap_or("").to_string(),
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

/// Builds a story part: voices, pictures (Slow), the animated page, the render and the thumbnail. Reuses finished
/// files, so Retry after a failure or Cancel continues where it stopped.
pub fn build(s: &Settings, id: &str, log: &dyn Fn(&str)) -> Result<()> {
    let dir = pipeline::job_dir(id)?;
    let script: Value = serde_json::from_str(&fs::read_to_string(dir.join("script.json"))?)?;
    let fast = script["story"]["style"] != "slow";
    let cast: Vec<Character> = serde_json::from_value(script["cast"].clone()).unwrap_or_default();
    let scenes = script["scenes"].as_array().context("story has no scenes")?.clone();
    let lines: Vec<(usize, String, String, String)> = scenes
        .iter()
        .enumerate()
        .flat_map(|(k, sc)| sc["lines"].as_array().cloned().unwrap_or_default().into_iter().map(move |l| (k, l)))
        .map(|(k, l)| (k, l["speaker"].as_str().unwrap_or("narrator").to_string(), l["text"].as_str().unwrap_or("").to_string(), l["mood"].as_str().unwrap_or("neutral").to_string()))
        .collect();
    let steps = if fast { 3 } else { 4 };

    // 1) Voices: one Kokoro voice per character, four lines at a time.
    pipeline::stage(id, 1, steps, &format!("Recording {} voice lines…", lines.len()), log);
    let audio = dir.join("audio");
    fs::create_dir_all(&audio)?;
    // Download the voice engine and model once, before the workers start (they would race to unpack the same archive).
    tools::tts_engine(log, false)?;
    tools::kokoro_model(log)?;
    let voice_of = |speaker: &str| cast.iter().find(|c| c.name == speaker).map(|c| c.voice.clone()).unwrap_or_else(|| s.story.narrator_voice.clone());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);
    std::thread::scope(|sc| {
        for _ in 0..4 {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= lines.len() || failed.lock().unwrap().is_some() {
                    break;
                }
                if let Err(e) = voice_line(s, &audio, i + 1, &voice_of(&lines[i].1), &lines[i].2) {
                    *failed.lock().unwrap() = Some(e);
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap() {
        return Err(e);
    }
    let durs: Vec<f64> = (1..=lines.len()).map(|n| media::duration(&audio.join(format!("l{n}.wav")))).collect::<Result<_>>()?;

    // 2) Slow style: one AI picture per scene, with the series art style and the looks of who is in it.
    if !fast {
        let art = script["story"]["art"].as_str().unwrap_or("flat vector cartoon illustration, bold black outlines, vibrant colors");
        for (k, sc) in scenes.iter().enumerate() {
            let out = dir.join(format!("scene_{}.png", k + 1));
            if out.exists() {
                continue;
            }
            llm::check_cancel()?;
            pipeline::stage(id, 2, steps, &format!("Painting picture {}/{}…", k + 1, scenes.len()), log);
            let what = sc["image"].as_str().unwrap_or("");
            let speakers: Vec<&str> = sc["lines"].as_array().into_iter().flatten().filter_map(|l| l["speaker"].as_str()).collect();
            let looks: Vec<String> = cast.iter().filter(|c| what.contains(&c.name) || speakers.contains(&c.name.as_str())).map(|c| format!("{}: {}", c.name, c.look)).collect();
            let prompt = format!("{art}, one single full-frame illustration of one moment, vertical composition. {} {what}. No text, no letters, no speech bubbles, no panels.", looks.join(". "));
            let seed = 1000 + k as u64;
            let r = if s.story.images == "gemini" { media::gemini_image_ratio(s, &prompt, "9:16", &out) } else { tools::comfy_image(&prompt, seed, &out, log) };
            if let Err(e) = r {
                if s.story.images == "gemini" && tools::comfy_ready() {
                    log(&format!("  Gemini picture failed ({e:#}); painting it locally"));
                    tools::comfy_image(&prompt, seed, &out, log)?;
                } else {
                    return Err(e);
                }
            }
        }
    }

    // 3) The animated page.
    pipeline::stage(id, steps - 1, steps, "Animating…", log);
    let (data, starts) = timeline(&script, &scenes, &lines, &durs, &cast, fast);
    let total = data["total"].as_f64().unwrap_or(30.0);
    let mood = script["mood"].as_str().unwrap_or("playful");
    let mut tags = vec![];
    if let Some((track, credit)) = media::pick_music(s, mood) {
        let ext = track.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "mp3".into());
        fs::copy(&track, dir.join(format!("music.{ext}")))?;
        tags.push(format!("<audio src=\"music.{ext}\" data-volume=\"0.28\" data-duck=\"0.35\" data-fade-out=\"1.5\" loop></audio>"));
        if credit.is_empty() { drop(fs::remove_file(dir.join("music_credit.txt"))) } else { fs::write(dir.join("music_credit.txt"), credit)? }
    }
    for (n, start) in starts.iter().enumerate() {
        tags.push(format!("<audio id=\"a{}\" src=\"audio/l{}.wav\" data-start=\"{start:.3}\" data-volume=\"1.9\"></audio>", n + 1, n + 1));
    }
    let sfx_dir = dir.join("sfx");
    for sc in data["scenes"].as_array().into_iter().flatten() {
        let fx = sc["sfx"].as_str().unwrap_or("none");
        if fx != "none" {
            make_sfx(&sfx_dir, fx)?;
            tags.push(format!("<audio src=\"sfx/{fx}.wav\" data-start=\"{:.3}\" data-volume=\"0.55\"></audio>", sc["start"].as_f64().unwrap_or(0.0)));
        }
    }
    let shots: String = if fast {
        String::new()
    } else {
        data["scenes"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(k, sc)| {
                let (a, b) = (sc["start"].as_f64().unwrap_or(0.0), sc["end"].as_f64().unwrap_or(0.0));
                format!("<section class=\"shot\" id=\"s{k}\" data-start=\"{a:.3}\" data-duration=\"{:.3}\"><div class=\"wrap\"><img src=\"scene_{}.png\" alt=\"\"></div></section>\n", b - a + 0.35, k + 1)
            })
            .collect()
    };
    // The data goes into a <script>: keep "</script>" from ever closing it early.
    let data_js = serde_json::to_string(&data)?.replace("</", "<\\/");
    let page = if fast { FAST_PAGE } else { SLOW_PAGE }
        .replace("{{TOTAL}}", &format!("{total:.2}"))
        .replace("{{AUDIO}}", &tags.join("\n"))
        .replace("{{SHOTS}}", &shots)
        .replace("{{DATA}}", &data_js);
    fs::write(dir.join("story.html"), page)?;

    // 4) Render, then a thumbnail from a frame after the hook (Fast) or the first picture (Slow).
    llm::check_cancel()?;
    pipeline::stage(id, steps, steps, &format!("Rendering the {total:.0}s video…"), log);
    let started = std::time::Instant::now();
    let _ = fs::remove_file(dir.join("short.mp4"));
    tools::givemotion_render(&dir, "story.html", "short.mp4")?;
    log(&format!("  rendered in {:.0}s", started.elapsed().as_secs_f64()));
    if fast {
        let t = starts.get(1).copied().unwrap_or(3.5).max(3.2);
        tools::givemotion_still(&dir, "story.html", t, "scene_1.png")?;
    }
    media::thumbnail(&dir, script["thumbnail_text"].as_str().unwrap_or(""))?;
    log("Story part ready ✔");
    Ok(())
}

/// One voice line: Kokoro (CPU, so several run side by side) into audio/l{n}.wav with the silence at both ends cut.
fn voice_line(s: &Settings, audio: &Path, n: usize, voice: &str, text: &str) -> Result<()> {
    let out = audio.join(format!("l{n}.wav"));
    if out.exists() {
        return Ok(());
    }
    let tmp = audio.join(format!("tmp{n}"));
    fs::create_dir_all(&tmp)?;
    fs::write(tmp.join("narration.txt"), text)?;
    let mut s2 = s.clone();
    (s2.voice, s2.kokoro_voice, s2.kokoro_device) = ("kokoro".into(), voice.into(), "cpu".into());
    media::voice(&s2, &tmp, &|_| {})?;
    media::ffmpeg(&tmp, &["-i", "voice.wav", "-af", "areverse,silenceremove=start_periods=1:start_threshold=-50dB:start_silence=0.05,areverse", "-ar", "44100", "line.wav"])?;
    fs::rename(tmp.join("line.wav"), &out)?;
    let _ = fs::remove_dir_all(&tmp);
    Ok(())
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
    let expr = match name {
        "whoosh" => "0.6*(random(0)-0.5)*sin(PI*t/0.45)",
        "boing" => "0.5*sin(2*PI*(180+90*sin(2*PI*11*t))*t)*exp(-3.5*t)",
        "thud" => "0.9*sin(2*PI*(75-35*t)*t)*exp(-9*t)+0.2*(random(0)-0.5)*exp(-30*t)",
        "ding" => "0.35*sin(2*PI*1568*t)*exp(-4*t)+0.18*sin(2*PI*2352*t)*exp(-5*t)",
        _ => "0.45*sin(2*PI*(260+520*sin(2*PI*6*t))*t)*(0.6+0.4*random(0))*exp(-2.5*t)",
    };
    media::ffmpeg(dir, &["-f", "lavfi", "-i", &format!("aevalsrc={expr}:s=44100:d=0.7"), "-af", "lowpass=f=9000", &format!("{name}.wav")])
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
    let se = new_series(&s, "A smart fridge that judges everything its owner eats", &s.story.style).unwrap();
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
}
