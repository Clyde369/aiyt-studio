//! Voice, AI images, captions, video and thumbnail. Heavy lifting is done by ffmpeg.

use crate::llm::{command, gemini, run};
use crate::settings::Settings;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

const W: u32 = 1080;
const H: u32 = 1920;
const FPS: u32 = 30;

/// Writes `voice.wav` in `dir` from `narration.txt`.
pub fn voice(s: &Settings, dir: &Path, log: &dyn Fn(&str)) -> Result<()> {
    let text = fs::read_to_string(dir.join("narration.txt"))?;
    match s.voice.as_str() {
        "gemini" => {
            // Only the script goes in: a style instruction here ("read this like…") sometimes gets spoken aloud.
            let body = json!({
                "contents": [{"parts": [{"text": text}]}],
                "generationConfig": {
                    "responseModalities": ["AUDIO"],
                    "speechConfig": {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": s.gemini_voice}}}
                }
            });
            let resp = gemini(s, &s.gemini_tts_model, &body)?;
            let u = &resp["usageMetadata"];
            crate::usage::record("gemini", &s.gemini_tts_model, "voice", u["promptTokenCount"].as_u64().unwrap_or(0), u["candidatesTokenCount"].as_u64().unwrap_or(0));
            let data = resp["candidates"][0]["content"]["parts"][0]["inlineData"]["data"].as_str().context("Gemini returned no audio")?;
            fs::write(dir.join("voice.pcm"), B64.decode(data)?)?;
            // Gemini TTS returns raw 16-bit little-endian PCM, 24 kHz mono.
            finish_voice(dir, &["-f", "s16le", "-ar", "24000", "-ac", "1", "-i", "voice.pcm"], "voice.pcm")?;
        }
        "local" => {
            if !cfg!(windows) {
                bail!("the offline voice uses Windows speech; pick the Gemini voice on this OS");
            }
            // Reads narration.txt from disk so no script text ever touches the command line.
            let ps = "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
                      $s.Rate = 2; $s.SetOutputToWaveFile((Join-Path $PWD 'voice_raw.wav')); \
                      $s.Speak([IO.File]::ReadAllText((Join-Path $PWD 'narration.txt'))); $s.Dispose()";
            run(command("powershell").current_dir(dir).args(["-NoProfile", "-Command", ps]), "Windows speech")?;
            finish_voice(dir, &["-i", "voice_raw.wav"], "voice_raw.wav")?;
        }
        engine @ ("kokoro" | "piper") => {
            let want_gpu = match s.kokoro_device.as_str() {
                "gpu" => {
                    if crate::tools::cuda_runtime().is_none() {
                        bail!("GPU-only voices need the NVIDIA GPU pack: Settings → Voice & sound → Set up GPU voices (or switch to Auto/CPU)");
                    }
                    true
                }
                "cpu" => false,
                // auto: the GPU only when an NVIDIA card AND its CUDA libraries are present
                _ => crate::on_path("nvidia-smi") && crate::tools::cuda_runtime().is_some(),
            };
            match local_tts(s, dir, &text, engine, want_gpu, log) {
                Ok(()) => {}
                // Auto mode: never lose a short over the GPU; GPU-only mode reports the problem instead.
                Err(e) if want_gpu && s.kokoro_device != "gpu" && !crate::llm::cancelled() => {
                    log(&format!("  GPU voice unavailable ({e:#}); using the CPU"));
                    local_tts(s, dir, &text, engine, false, log)?;
                }
                Err(e) => return Err(e),
            }
            finish_voice(dir, &["-i", "voice_raw.wav"], "voice_raw.wav")?;
        }
        v => bail!("unknown voice '{v}'"),
    }
    Ok(())
}

/// One local text-to-speech run (Kokoro or Piper through sherpa-onnx) into `voice_raw.wav`, on the GPU (CUDA) or CPU.
fn local_tts(s: &Settings, dir: &Path, text: &str, engine: &str, gpu: bool, log: &dyn Fn(&str)) -> Result<()> {
    let exe = crate::tools::tts_engine(log, gpu)?;
    let p = |path: &Path, f: &str| path.join(f).to_string_lossy().to_string();
    let mut args: Vec<String> = if engine == "kokoro" {
        let model = crate::tools::kokoro_model(log)?;
        let sid = crate::tools::KOKORO_VOICES.iter().position(|v| *v == s.kokoro_voice).unwrap_or(3); // af_heart
        let lexicon = if s.kokoro_voice.starts_with('b') { "lexicon-gb-en.txt" } else { "lexicon-us-en.txt" };
        vec![
            format!("--kokoro-model={}", p(&model, "model.int8.onnx")),
            format!("--kokoro-voices={}", p(&model, "voices.bin")),
            format!("--kokoro-tokens={}", p(&model, "tokens.txt")),
            format!("--kokoro-data-dir={}", p(&model, "espeak-ng-data")),
            format!("--kokoro-dict-dir={}", p(&model, "dict")),
            format!("--kokoro-lexicon={}", p(&model, lexicon)),
            format!("--sid={sid}"),
        ]
    } else {
        let (onnx, voice) = crate::tools::piper_model(log, &s.piper_voice)?;
        vec![
            format!("--vits-model={}", onnx.to_string_lossy()),
            format!("--vits-tokens={}", p(&voice, "tokens.txt")),
            format!("--vits-data-dir={}", p(&voice, "espeak-ng-data")),
            "--sid=0".into(),
        ]
    };
    let threads = std::thread::available_parallelism().map(|n| n.get().clamp(1, 8)).unwrap_or(4);
    args.extend([
        format!("--num-threads={threads}"),
        format!("--provider={}", if gpu { "cuda" } else { "cpu" }),
        "--output-filename=voice_raw.wav".into(),
        text.split_whitespace().collect::<Vec<_>>().join(" "), // one line of text for the CLI
    ]);
    let mut cmd = command(&exe.to_string_lossy());
    // CUDA libraries kept in the app's own tools folder are found by putting that folder first on PATH.
    if let Some(rt) = gpu.then(crate::tools::cuda_runtime).flatten().filter(|p| !p.as_os_str().is_empty()) {
        let mut dirs = vec![rt];
        dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        if let Ok(p) = std::env::join_paths(dirs) {
            cmd.env("PATH", p);
        }
    }
    let started = std::time::Instant::now();
    let (_, err) = crate::llm::run_full(cmd.current_dir(dir).args(args), "local voice")?;
    // sherpa-onnx quietly drops to the CPU when CUDA cannot start; it says so on stderr.
    let err = String::from_utf8_lossy(&err).to_lowercase();
    if gpu && (err.contains("fallback to cpu") || err.contains("falling back to cpu")) {
        bail!("CUDA could not start on this GPU (the NVIDIA driver may need an update)");
    }
    log(&format!("  {engine} voice made on the {} in {:.1}s", if gpu { "GPU" } else { "CPU" }, started.elapsed().as_secs_f64()));
    Ok(())
}

/// Raw speech -> `voice.wav` with the silence at the start cut, so the short opens on the first word.
/// voice.wav only appears once complete, so a cancelled run never leaves a half-made voice to reuse.
fn finish_voice(dir: &Path, input: &[&str], raw: &str) -> Result<()> {
    let mut args = input.to_vec();
    args.extend(["-af", "silenceremove=start_periods=1:start_threshold=-50dB:start_silence=0.05", "voice.wav"]);
    let r = ffmpeg(dir, &args);
    let _ = fs::remove_file(dir.join(raw));
    r
}

/// AI image with the image model chosen in Settings: local FLUX (free, on your GPU) or Gemini (paid key).
pub fn image(s: &Settings, prompt: &str, ratio: &str, out: &Path, log: &dyn Fn(&str)) -> Result<()> {
    if s.image_model == "gemini" {
        return gemini_image_ratio(s, prompt, ratio, out);
    }
    // FLUX.1-schnell: 4 steps, no guidance. Sizes are multiples of 64; callers scale to the final size.
    let (w, h) = match ratio {
        // ponytail: ~0.6 MP keeps FLUX inside 8 GB of VRAM and roughly twice as fast as 1 MP; the art is upscaled
        // and darkened under a crisp logo, so the softness does not show. Raise these on 12 GB+ cards.
        "16:9" => (1024, 576),
        "9:16" => (576, 1024),
        _ => (768, 768),
    };
    let (exe, mut args) = crate::tools::flux_args()?;
    args.extend([
        "-p".into(), prompt.into(), "--cfg-scale".into(), "1.0".into(), "--sampling-method".into(), "euler".into(), "--steps".into(), "4".into(),
        "-W".into(), w.to_string(), "-H".into(), h.to_string(), "-s".into(), "-1".into(),
        // Keep the text encoders on the CPU and decode in tiles so FLUX fits in 8 GB of video memory.
        "--clip-on-cpu".into(), "--vae-tiling".into(), "-o".into(), out.to_string_lossy().to_string(),
    ]);
    let started = std::time::Instant::now();
    run(command(&exe.to_string_lossy()).args(args), "FLUX")?;
    if !out.exists() {
        bail!("FLUX finished without an image");
    }
    log(&format!("  image made with FLUX in {:.0}s", started.elapsed().as_secs_f64()));
    Ok(())
}

/// A 9:16 scene for an AI-visuals short.
pub fn ai_image(s: &Settings, prompt: &str, out: &Path, log: &dyn Fn(&str)) -> Result<()> {
    image(s, &format!("Vertical 9:16 cinematic, highly detailed, dramatic lighting, absolutely no text or letters. {prompt}"), "9:16", out, log)
}

/// Any Gemini image at a given aspect ratio ("16:9", "1:1", "9:16"…), prompt used as-is.
pub fn gemini_image_ratio(s: &Settings, prompt: &str, ratio: &str, out: &Path) -> Result<()> {
    let body = json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": {"responseModalities": ["IMAGE"], "imageConfig": {"aspectRatio": ratio}}
    });
    let resp = gemini(s, &s.gemini_image_model, &body).map_err(|e| {
        if e.to_string().contains("HTTP 429") && e.to_string().contains("limit: 0") {
            anyhow::anyhow!("AI images need a Gemini key with billing enabled (the free tier allows 0 images). Use Pexels or Pixabay visuals instead")
        } else {
            e
        }
    })?;
    let u = &resp["usageMetadata"];
    crate::usage::record("gemini", &s.gemini_image_model, "image", u["promptTokenCount"].as_u64().unwrap_or(0), u["candidatesTokenCount"].as_u64().unwrap_or(0));
    let parts = resp["candidates"][0]["content"]["parts"].as_array().context("Gemini returned no image")?;
    let data = parts.iter().find_map(|p| p["inlineData"]["data"].as_str()).context("Gemini returned no image (prompt may have been blocked)")?;
    fs::write(out, B64.decode(data)?)?;
    Ok(())
}

pub fn duration(file: &Path) -> Result<f64> {
    let out = run(command("ffprobe").args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]).arg(file), "ffprobe")?;
    String::from_utf8_lossy(&out).trim().parse().context("could not read media duration")
}

/// Numbered visuals present in the job folder: scene_N.png (AI) or clip_N.mp4 (stock).
fn numbered(dir: &Path, prefix: &str, ext: &str) -> usize {
    (1..).take_while(|i| dir.join(format!("{prefix}_{i}.{ext}")).exists()).count()
}

/// Assembles `short.mp4` from whatever visuals the folder has + `voice.wav` + `captions.ass` (+ optional music).
pub fn render(s: &Settings, dir: &Path, dur: f64) -> Result<()> {
    let (scenes, clips) = (numbered(dir, "scene", "png"), numbered(dir, "clip", "mp4"));
    let mut args: Vec<String> = vec![];
    let mut filter = String::new();
    let n = scenes.max(clips).max(1);
    let seg = dur / n as f64 + 0.05;
    let fit = format!("scale={W}:{H}:force_original_aspect_ratio=increase,crop={W}:{H},setsar=1");

    if scenes > 0 {
        let frames = (seg * FPS as f64).ceil() as u32 + 1;
        let step = 0.25 / frames as f64; // ease to 1.25x zoom over each scene
        for i in 0..scenes {
            args.extend(["-i".into(), format!("scene_{}.png", i + 1)]);
            // Alternate slow push-in and pull-out (Ken Burns). Upscale first to avoid zoompan jitter.
            let z = if i % 2 == 0 { format!("min(zoom+{step:.6},1.25)") } else { format!("if(eq(on,0),1.25,max(zoom-{step:.6},1))") };
            let (bw, bh) = (W * 3 / 2, H * 3 / 2);
            filter += &format!(
                "[{i}:v]scale={bw}:{bh}:force_original_aspect_ratio=increase,crop={bw}:{bh},\
                 zoompan=z='{z}':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d={frames}:s={W}x{H}:fps={FPS},setsar=1[s{i}];"
            );
        }
    } else if clips > 0 {
        for i in 0..clips {
            // Loop short clips, cut each to an equal slice of the voiceover.
            args.extend(["-stream_loop", "-1", "-t"].map(String::from));
            args.extend([format!("{:.2}", seg + 0.2), "-i".into(), format!("clip_{}.mp4", i + 1)]);
            filter += &format!("[{i}:v]{fit},fps={FPS},trim=duration={seg:.3},setpts=PTS-STARTPTS[s{i}];");
        }
    } else {
        args.extend(gradient(dur));
        filter += &format!("[0:v]{fit},fps={FPS}[s0];");
    }
    filter += &(0..n).map(|i| format!("[s{i}]")).collect::<String>();
    filter += &format!("concat=n={n}:v=1:a=0,ass=captions.ass[v]");

    args.extend(["-i".into(), "voice.wav".into()]);
    // Music matched to the script's mood; its licence credit is saved for the YouTube description.
    let mood = fs::read_to_string(dir.join("script.json")).ok().and_then(|x| serde_json::from_str::<serde_json::Value>(&x).ok()).and_then(|v| v["mood"].as_str().map(String::from)).unwrap_or_default();
    let picked = pick_music(s, &mood);
    match picked.as_ref().map(|p| &p.1).filter(|c| !c.is_empty()) {
        Some(credit) => fs::write(dir.join("music_credit.txt"), credit)?,
        None => drop(fs::remove_file(dir.join("music_credit.txt"))),
    }
    let audio = if let Some((track, _)) = picked {
        args.extend(["-stream_loop".into(), "-1".into(), "-i".into(), track.to_string_lossy().into()]);
        let st = "aformat=sample_rates=48000:channel_layouts=stereo";
        filter += &format!(";[{n}:a]{st}[va];[{}:a]volume=0.10,{st}[ma];[va][ma]amix=inputs=2:duration=first:normalize=0[a]", n + 1);
        "[a]".to_string()
    } else {
        format!("{n}:a")
    };
    args.extend(["-filter_complex".into(), filter, "-map".into(), "[v]".into(), "-map".into(), audio]);
    args.extend(["-t".into(), format!("{:.2}", dur + 0.3)]);
    encode(s, dir, args, &["-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", "short.mp4"])
}

/// Writes `thumbnail.jpg`: the first visual with big bold text.
pub fn thumbnail(dir: &Path, text: &str) -> Result<()> {
    let style = "Style: Default,Arial Black,170,&H0000FFFF,&H000000FF,&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,12,6,5,60,60,0,1";
    let event = format!("Dialogue: 0,0:00:00.00,0:00:10.00,Default,,0,0,0,,{}", clean(text).to_uppercase());
    fs::write(dir.join("thumb.ass"), ass_doc(style, &[event]))?;
    let video = ["cut.mp4", "clip_1.mp4"].into_iter().find(|f| dir.join(f).exists());
    let (mut args, pick): (Vec<String>, &str) = if dir.join("scene_1.png").exists() {
        (vec!["-i".into(), "scene_1.png".into()], "")
    } else if let Some(v) = video {
        (vec!["-i".into(), v.into()], "thumbnail=60,") // most representative of the first ~2s of frames
    } else {
        (gradient(2.0), "")
    };
    let vf = format!("{pick}scale={W}:{H}:force_original_aspect_ratio=increase,crop={W}:{H},ass=thumb.ass,format=yuvj420p");
    args.extend(["-vf".into(), vf, "-frames:v".into(), "1".into(), "-q:v".into(), "3".into(), "thumbnail.jpg".into()]);
    ffmpeg(dir, &args.iter().map(String::as_str).collect::<Vec<_>>())?;
    if !dir.join("thumbnail.jpg").exists() {
        bail!("could not make a thumbnail");
    }
    Ok(())
}

fn gradient(dur: f64) -> Vec<String> {
    vec!["-f".into(), "lavfi".into(), "-i".into(), format!("gradients=s={W}x{H}:r={FPS}:speed=0.015:d={:.2}", dur + 1.0)]
}

/// A background track: from the music folder in Settings, or the app's own `_music` folder. Prefers tracks tagged
/// with the script's mood in the folder's `credits.json` ({"file.mp3": {"mood": "...", "credit": "..."}}).
/// Returns (track, licence credit to put in the description).
fn pick_music(s: &Settings, mood: &str) -> Option<(PathBuf, String)> {
    let folder = if s.music_dir.trim().is_empty() { crate::settings::out_dir().join("_music") } else { PathBuf::from(s.music_dir.trim()) };
    let tracks: Vec<PathBuf> = fs::read_dir(&folder)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| ["mp3", "wav", "m4a", "ogg", "aac", "flac"].contains(&e.to_lowercase().as_str())))
        .filter(|p| fs::metadata(p).map(|m| m.len() > 100_000).unwrap_or(false)) // skip partial downloads
        .collect();
    if tracks.is_empty() {
        return None;
    }
    let info: serde_json::Value = fs::read_to_string(folder.join("credits.json")).ok().and_then(|x| serde_json::from_str(&x).ok()).unwrap_or_default();
    let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let matching: Vec<&PathBuf> = tracks.iter().filter(|p| !mood.is_empty() && info[name(p)]["mood"].as_str() == Some(mood)).collect();
    let pool: Vec<&PathBuf> = if matching.is_empty() { tracks.iter().collect() } else { matching };
    let r = std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), 0u8) as usize;
    let track = pool[r % pool.len()];
    Some((std::path::absolute(track).ok()?, info[name(track)]["credit"].as_str().unwrap_or("").to_string()))
}

/// House look for every short (AI or clip): a hook banner pinned to the top and big 1-3 word captions that pop in,
/// alternating white and the accent colour. Colours come from Settings → Look.
/// ponytail: caption timing is spread over the audio by character count (fine for steady TTS);
/// clips use Whisper's real segment timing instead.
pub fn captions_ass(s: &Settings, narration: &str, hook: &str, cta_line: &str, dur: f64) -> String {
    house_ass(s, hook, &[(0.0, dur, narration.to_string())], dur, cta_window(narration, cta_line, dur))
}

/// When the subscribe line is spoken: its position in the narration mapped onto the audio by character count
/// (the same approximation the captions use).
fn cta_window(narration: &str, line: &str, dur: f64) -> Option<(f64, f64)> {
    let norm = |x: &str| x.to_lowercase().chars().filter(|c| c.is_alphanumeric() || *c == ' ').collect::<String>();
    let (n, l) = (norm(narration), norm(line));
    let l = l.trim();
    if l.len() < 8 {
        return None;
    }
    let start = n.find(l)?;
    let total = n.len().max(1) as f64;
    Some((start as f64 / total * dur, (start + l.len()) as f64 / total * dur))
}

/// Animated subscribe button above the captions while the call to action is spoken: pops in, gets "tapped",
/// turns grey "SUBSCRIBED", fades out. Drawn by the caption renderer, so it always matches the house colours.
fn subscribe_events(window: (f64, f64), banner: &str) -> Vec<String> {
    let t0 = window.0.max(0.0);
    let pos = r"\pos(540,1185)";
    vec![
        format!(
            r"Dialogue: 2,{},{},Cta,,0,0,0,,{{{pos}\3c{banner}&\fscx0\fscy0\t(0,180,\fscx115\fscy115)\t(180,320,\fscx100\fscy100)\t(850,950,\fscx90\fscy90)\t(950,1050,\fscx100\fscy100)}}SUBSCRIBE",
            ts(t0),
            ts(t0 + 1.05)
        ),
        format!(r"Dialogue: 2,{},{},Cta,,0,0,0,,{{{pos}\3c&H00505050&\fad(0,350)}}SUBSCRIBED", ts(t0 + 1.05), ts((t0 + 2.6).max(window.1))),
    ]
}

/// "#RRGGBB" -> ASS "&H00BBGGRR" (falls back to `default` for anything else).
fn ass_color(hex: &str, default: &str) -> String {
    let h = hex.trim().trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return default.into();
    }
    format!("&H00{}{}{}", &h[4..6], &h[2..4], &h[0..2]).to_uppercase()
}

fn house_ass(s: &Settings, hook: &str, segments: &[(f64, f64, String)], dur: f64, cta: Option<(f64, f64)>) -> String {
    let accent = ass_color(&s.style_accent, "&H0000D4FF");
    let banner = ass_color(&s.style_hook_color, "&H00552DFF");
    let caption = "Style: Default,Arial Black,92,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,-1,0,0,0,100,100,0,0,1,7,3,2,60,60,560,1";
    // White text on a solid brand-coloured box (BorderStyle 3), pinned to the top.
    let hook_style = format!("Style: Hook,Arial Black,74,&H00FFFFFF,&H000000FF,{banner},&H00000000,-1,0,0,0,100,100,0,0,3,22,0,8,80,80,230,1");
    let mut events = caption_events(segments, &accent);
    if let Some(w) = cta {
        events.extend(subscribe_events(w, &banner));
    }
    if s.style_hook && !hook.trim().is_empty() {
        events.push(format!("Dialogue: 1,{},{},Hook,,0,0,0,,{}", ts(0.0), ts(dur), wrap_hook(&clean(hook).to_uppercase())));
    }
    // Subscribe button: white text on a solid box (BorderStyle 3) whose colour is set per event.
    let cta_style = "Style: Cta,Arial Black,62,&H00FFFFFF,&H000000FF,&H000000FF,&H00000000,-1,0,0,0,100,100,1,0,3,20,0,5,0,0,0,1";
    ass_doc(&format!("{caption}\n{hook_style}\n{cta_style}"), &events)
}

/// Break a long hook into balanced lines (max ~22 characters) so the banner stays a tidy block.
fn wrap_hook(h: &str) -> String {
    let mut lines: Vec<String> = vec![String::new()];
    for w in h.split_whitespace() {
        let last = lines.last_mut().unwrap();
        if !last.is_empty() && last.len() + w.len() + 1 > 22 {
            lines.push(w.to_string());
        } else {
            if !last.is_empty() {
                last.push(' ');
            }
            last.push_str(w);
        }
    }
    lines.join(r"\N")
}

/// Splits each (start, end, text) segment into 1-3 word captions spread over the segment by character count.
fn caption_events(segments: &[(f64, f64, String)], accent: &str) -> Vec<String> {
    let mut events = vec![];
    for (start, end, text) in segments {
        let mut chunks: Vec<(String, f64)> = vec![]; // (text, weight)
        let mut cur: Vec<String> = vec![];
        let words: Vec<String> = text.split_whitespace().map(clean).filter(|w| !w.is_empty()).collect();
        for (i, w) in words.iter().enumerate() {
            cur.push(w.clone());
            let len: usize = cur.iter().map(|w| w.len() + 1).sum();
            let pause = if w.ends_with(['.', '!', '?']) { 4.0 } else if w.ends_with([',', ';', ':']) { 2.0 } else { 0.0 };
            if cur.len() == 3 || len > 14 || pause > 0.0 || i == words.len() - 1 {
                chunks.push((cur.join(" ").to_uppercase(), len as f64 + pause));
                cur.clear();
            }
        }
        let total: f64 = chunks.iter().map(|c| c.1).sum();
        let mut t = *start;
        for (text, wgt) in chunks {
            let e = t + wgt / total * (end - start);
            let color = if events.len() % 2 == 1 { format!(r"\c{accent}&") } else { String::new() };
            events.push(format!(r"Dialogue: 0,{},{},Default,,0,0,0,,{{{color}\fscx70\fscy70\t(0,90,\fscx100\fscy100)}}{text}", ts(t), ts(e)));
            t = e;
        }
    }
    events
}

/// Clip captions: the AI intro line while it is spoken, then the original speech, in the house look.
pub fn clip_ass(s: &Settings, hook: &str, intro: &str, intro_dur: f64, speech: &[(f64, f64, String)], dur: f64) -> String {
    let mut segs: Vec<(f64, f64, String)> = vec![];
    if intro_dur > 0.0 && !intro.trim().is_empty() {
        segs.push((0.0, intro_dur, intro.to_string()));
    }
    let cue = |t: &str| (t.starts_with('(') && t.ends_with(')')) || (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('*') && t.ends_with('*')) || t.starts_with('♪'); // "(music)", "[applause]", "*bang*"
    segs.extend(speech.iter().filter(|s| s.0 >= intro_dur - 0.2 && s.0 < dur && !cue(s.2.trim())).map(|(a, b, t)| (*a, b.min(dur), t.clone())));
    house_ass(s, hook, &segs, dur, None)
}

/// `short.mp4` from `cut.mp4`: blurred full-screen backdrop, original framed in the middle, captions on top,
/// and the AI intro (`voice.wav`, if any) over the start with the original audio ducked underneath.
pub fn render_clip(s: &Settings, dir: &Path, intro_dur: f64, dur: f64) -> Result<()> {
    let mut args: Vec<String> = vec!["-i".into(), "cut.mp4".into()];
    let mut filter = format!(
        "[0:v]split[a][b];[a]scale={W}:{H}:force_original_aspect_ratio=increase,crop={W}:{H},boxblur=24:2,eq=brightness=-0.12[bg];\
         [b]scale={W}:{H}:force_original_aspect_ratio=decrease[fg];[bg][fg]overlay=(W-w)/2:(H-h)/2,setsar=1,fps={FPS},ass=captions.ass[v]"
    );
    let st = "aformat=sample_rates=48000:channel_layouts=stereo";
    let audio = if intro_dur > 0.0 && dir.join("voice.wav").exists() {
        args.extend(["-i".into(), "voice.wav".into()]);
        filter += &format!(";[0:a]volume='if(lt(t,{intro_dur:.2}),0.22,1)':eval=frame,{st}[o];[1:a]{st}[i];[o][i]amix=inputs=2:duration=first:normalize=0[a]");
        "[a]".to_string()
    } else {
        "0:a?".to_string()
    };
    args.extend(["-filter_complex".into(), filter, "-map".into(), "[v]".into(), "-map".into(), audio, "-t".into(), format!("{dur:.2}")]);
    encode(s, dir, args, &["-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", "short.mp4"])
}

/// Channel art: the logo (channel name + tagline on the banner, initials on the avatar) set in crisp type over
/// the AI artwork when there is one (banner_raw.png / avatar_raw.png), else over a brand-colour gradient.
/// Text is never left to the image model: it misspells names.
pub fn brand_images(dir: &Path, name: &str, tagline: &str, initials: &str, c0: &str, c1: &str) -> Result<()> {
    let hex = |c: &str, d: &str| {
        let c = c.trim().trim_start_matches('#');
        if c.len() == 6 && c.chars().all(|x| x.is_ascii_hexdigit()) { c.to_string() } else { d.to_string() }
    };
    let (c0, c1) = (hex(c0, "ff2d55"), hex(c1, "7c3aed"));
    let glow = ass_color(&format!("#{c0}"), "&H00552DFF");
    let name = clean(name);
    // Fit the name inside YouTube's banner safe area (~1546 px wide on every device).
    let banner_size = (1400.0 / (name.chars().count().max(4) as f64 * 0.56)).clamp(90.0, 230.0).round() as u32;
    let doc = |w: u32, h: u32, size: u32, events: &[String]| {
        format!(
            "[Script Info]\nScriptType: v4.00+\nPlayResX: {w}\nPlayResY: {h}\nWrapStyle: 2\n\n[V4+ Styles]\n\
             Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
             Style: Big,Arial Black,{size},&H00FFFFFF,&H000000FF,&H00101010,&H96000000,-1,-1,0,0,100,100,0,0,1,8,9,5,0,0,0,1\n\
             Style: Small,Segoe UI Semibold,{},&H00FFFFFF,&H000000FF,&H00101010,&H96000000,-1,0,0,0,100,100,3,0,1,3,4,5,0,0,0,1\n\n\
             [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n{}\n",
            (size as f64 * 0.36).max(40.0).round(),
            events.join("\n")
        )
    };
    // A soft brand-colour glow behind the lettering (layer 0), the crisp outlined lettering on top (layer 1).
    let logo = |x: u32, y: u32, text: &str| {
        vec![
            format!("Dialogue: 0,0:00:00.00,0:00:05.00,Big,,0,0,0,,{{\\pos({x},{y})\\bord26\\shad0\\blur18\\3c{glow}&\\1a&HFF&}}{text}"),
            format!("Dialogue: 1,0:00:00.00,0:00:05.00,Big,,0,0,0,,{{\\pos({x},{y})}}{text}"),
        ]
    };
    let mut banner = logo(1280, 700, &name.to_uppercase());
    if !tagline.trim().is_empty() {
        banner.push(format!("Dialogue: 1,0:00:00.00,0:00:05.00,Small,,0,0,0,,{{\\pos(1280,{})}}{}", 700 + banner_size * 3 / 4, clean(tagline)));
    }
    let ini: String = initials.chars().filter(|c| !c.is_whitespace()).take(3).collect::<String>().to_uppercase();
    fs::write(dir.join("banner.ass"), doc(2560, 1440, banner_size, &banner))?;
    fs::write(dir.join("avatar.ass"), doc(800, 800, if ini.chars().count() > 2 { 250 } else { 330 }, &logo(400, 410, &ini)))?;
    // Background: the AI artwork (slightly darkened so the logo pops) or a gradient in the brand colours.
    let fill = |w: u32, h: u32| format!("scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},eq=brightness=-0.06:saturation=1.1,vignette=PI/5");
    let source = |raw: &str, w: u32, h: u32| -> Vec<String> {
        if dir.join(raw).exists() {
            vec!["-i".into(), raw.into()]
        } else {
            vec!["-f".into(), "lavfi".into(), "-i".into(), format!("gradients=s={w}x{h}:c0=0x{c0}:c1=0x{c1}:x0=0:y0=0:x1={w}:y1={h}:n=2:d=1")]
        }
    };
    let mut a = source("banner_raw.png", 2560, 1440);
    a.extend(["-vf".into(), format!("{},ass=banner.ass,format=yuvj420p", fill(2560, 1440)), "-frames:v".into(), "1".into(), "-q:v".into(), "2".into(), "banner.jpg".into()]);
    ffmpeg(dir, &a.iter().map(String::as_str).collect::<Vec<_>>())?;
    let mut a = source("avatar_raw.png", 800, 800);
    a.extend(["-vf".into(), format!("{},ass=avatar.ass", fill(800, 800)), "-frames:v".into(), "1".into(), "avatar.png".into()]);
    ffmpeg(dir, &a.iter().map(String::as_str).collect::<Vec<_>>())
}

fn ass_doc(style: &str, events: &[String]) -> String {
    format!(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: {W}\nPlayResY: {H}\nWrapStyle: 0\n\n\
         [V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n{style}\n\n\
         [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n{}\n",
        events.join("\n")
    )
}

/// Strip characters that are ASS override syntax.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '{' | '}' | '\\')).collect()
}

fn ts(t: f64) -> String {
    let cs = (t * 100.0).round() as u64;
    format!("{}:{:02}:{:02}.{:02}", cs / 360000, cs / 6000 % 60, cs / 100 % 60, cs % 100)
}

/// True when ffmpeg can encode on an NVIDIA GPU (checked once).
fn nvenc_available() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| {
        crate::on_path("nvidia-smi")
            && run(command("ffmpeg").args(["-hide_banner", "-encoders"]), "ffmpeg").map(|o| String::from_utf8_lossy(&o).contains("h264_nvenc")).unwrap_or(false)
    })
}

/// Encode video: on the NVIDIA GPU (NVENC) when available and allowed in Settings, else the CPU (x264).
/// A GPU failure (busy, old driver) retries on the CPU, so a short is never lost.
pub fn encode(s: &Settings, dir: &Path, pre: Vec<String>, post: &[&str]) -> Result<()> {
    let cpu = ["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"];
    let gpu = ["-c:v", "h264_nvenc", "-preset", "p5", "-rc", "vbr", "-cq", "21", "-b:v", "0"];
    let attempt = |codec: &[&str]| {
        let mut a: Vec<&str> = pre.iter().map(String::as_str).collect();
        a.extend_from_slice(codec);
        a.extend_from_slice(post);
        ffmpeg(dir, &a)
    };
    if s.gpu_encode != "off" && nvenc_available() {
        match attempt(&gpu) {
            Ok(()) => return Ok(()),
            Err(_) if crate::llm::cancelled() => crate::llm::check_cancel()?,
            Err(_) => {} // fall through to the CPU
        }
    }
    attempt(&cpu)
}

pub(crate) fn ffmpeg(dir: &Path, args: &[&str]) -> Result<()> {
    run(command("ffmpeg").current_dir(dir).args(["-y", "-hide_banner", "-loglevel", "error"]).args(args), "ffmpeg").map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captions_cover_the_audio() {
        let ass = captions_ass(&Settings::default(), "Octopuses have THREE hearts. {Two} pump blood to the gills, one to the body!", "", "", 10.0);
        let lines: Vec<&str> = ass.lines().filter(|l| l.starts_with("Dialogue:")).collect();
        assert!(lines.len() >= 5, "{ass}");
        assert!(lines[0].starts_with("Dialogue: 0,0:00:00.00,"));
        assert!(lines.last().unwrap().contains(",0:00:10.00,Default"));
        assert!(lines[0].ends_with("OCTOPUSES HAVE"));
        assert!(!ass.contains("{TWO}") && ass.contains("TWO PUMP"));
        assert_eq!(ts(3725.5), "1:02:05.50");
    }
}

/// Live check against the real Gemini API using the key saved by the app: `cargo test gemini_live -- --ignored`
#[cfg(test)]
#[test]
#[ignore]
fn gemini_live() {
    let path = std::path::PathBuf::from(std::env::var("APPDATA").unwrap()).join("com.aiyt.studio/settings.json");
    let mut s: Settings = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    (s.voice, s.writer, s.writer_model) = ("gemini".into(), "gemini".into(), String::new());
    let script = crate::llm::generate_json(&s, "script", "Return ONLY JSON {\"title\": a 5 word title about octopuses}").unwrap();
    assert!(script["title"].as_str().is_some_and(|t| !t.is_empty()), "{script}");
    let dir = std::env::temp_dir().join("aiyt-gemini-live");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("narration.txt"), "Octopuses have three hearts, and blue blood.").unwrap();
    voice(&s, &dir, &|m| println!("{m}")).unwrap();
    assert!(duration(&dir.join("voice.wav")).unwrap() > 1.0);
    // Image models need a billed key; on a free key this must fail with the friendly message.
    match gemini_image_ratio(&s, "an octopus in deep blue water", "9:16", &dir.join("scene_1.png")) {
        Ok(()) => assert!(fs::metadata(dir.join("scene_1.png")).unwrap().len() > 10_000),
        Err(e) => assert!(format!("{e:#}").contains("billing"), "{e:#}"),
    }
}

/// Render check for stock clips (landscape + too-short clips) with background music: `cargo test render_clips -- --ignored`
#[cfg(test)]
#[test]
#[ignore]
fn render_clips() {
    let dir = std::env::temp_dir().join("aiyt-render-clips");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("music")).unwrap();
    let mk = |args: &str| ffmpeg(&dir, &args.split(' ').collect::<Vec<_>>()).unwrap();
    mk("-f lavfi -i sine=frequency=300:duration=6 voice.wav");
    mk("-f lavfi -i sine=frequency=500:duration=3 music/track.mp3");
    mk("-f lavfi -i testsrc2=s=1920x1080:d=1 clip_1.mp4"); // landscape, shorter than its slot: must loop
    mk("-f lavfi -i testsrc2=s=720x1280:d=5 clip_2.mp4");
    fs::write(dir.join("captions.ass"), captions_ass(&Settings::default(), "Hello there, this is a test. Follow for part two of this test.", "Test hook here", "Follow for part two of this test.", 6.0)).unwrap();
    let s = Settings { music_dir: dir.join("music").to_string_lossy().into(), ..Default::default() };
    render(&s, &dir, 6.0).unwrap();
    let d = duration(&dir.join("short.mp4")).unwrap();
    assert!((6.0..7.0).contains(&d), "duration {d}");
    thumbnail(&dir, "TEST").unwrap();
    assert!(dir.join("thumbnail.jpg").exists());
}

/// The Gemini voice must start on the script's first word, with no spoken instructions or long lead-in:
/// `cargo test voice_starts_clean -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn voice_starts_clean() {
    let path = std::path::PathBuf::from(std::env::var("APPDATA").unwrap()).join("com.aiyt.studio/settings.json");
    let mut s: Settings = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    s.voice = "gemini".into();
    let _ = crate::settings::OUT_DIR.set(std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap()).join("Videos/AIYT"));
    let dir = crate::settings::out_dir().join("voice-test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("narration.txt"), "Octopuses have three hearts. Two pump blood to the gills, and one pumps it to the rest of the body.").unwrap();
    voice(&s, &dir, &|m| println!("{m}")).unwrap();
    let words = crate::tools::transcribe(&dir, "voice.wav", "voice.srt", "en", &|m| println!("{m}")).unwrap();
    println!("{words:?}");
    let first = words.first().expect("no speech");
    let _ = fs::remove_dir_all(&dir);
    assert!(first.2.to_lowercase().starts_with("octopus"), "starts with: {}", first.2);
    assert!(first.0 < 0.5, "speech starts at {}s", first.0);
}

/// Channel art with the image model: FLUX/Gemini paint the artwork only (no text: they misspell names), then the
/// app sets the logo on top. banner.jpg is 2560x1440 with the logo in YouTube's centre safe area; avatar.png is 800x800.
pub fn brand_art_ai(s: &Settings, dir: &Path, k: &BrandKit, log: &dyn Fn(&str)) -> Result<()> {
    let style = if k.vibe.is_empty() { "bold, cinematic, high contrast".to_string() } else { k.vibe.clone() };
    let no_text = "Absolutely no text, letters, numbers, words, logos or watermarks anywhere in the image.";
    // Old raw art must not be reused if a step below fails.
    let _ = fs::remove_file(dir.join("banner_raw.png"));
    let _ = fs::remove_file(dir.join("avatar_raw.png"));
    log("  painting the banner artwork…");
    image(
        s,
        &format!(
            "Wide cinematic YouTube channel banner artwork for a channel about {}. Style: {style}. Rich, detailed, eye-catching scene that \
             instantly says what the channel is about, with the main subject and strongest detail around the edges and a calmer, slightly darker \
             open area across the middle where a logo will go. {no_text}",
            k.niche
        ),
        "16:9",
        &dir.join("banner_raw.png"),
        log,
    )?;
    log("  painting the profile picture artwork…");
    image(
        s,
        &format!(
            "Bold, simple emblem artwork for a YouTube profile picture for a channel about {}. Style: {style}. One iconic subject, centred, \
             strong colours, clean background that fills the square, readable as a tiny circle. {no_text}",
            k.niche
        ),
        "1:1",
        &dir.join("avatar_raw.png"),
        log,
    )?;
    brand_images(dir, &k.name, &k.tagline, &k.initials, &k.primary, &k.secondary)
}

/// Everything the art needs to know about the channel.
pub struct BrandKit {
    pub name: String,
    pub tagline: String,
    pub initials: String,
    pub primary: String,
    pub secondary: String,
    pub niche: String,
    pub vibe: String,
}

/// Visual check of the channel-art logo over real artwork: `cargo test brand_compose -- --ignored`
#[cfg(test)]
#[test]
#[ignore]
fn brand_compose() {
    let dir = std::env::temp_dir().join("aiyt-brand-test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let src = std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap()).join("Videos/AIYT/_brand/UCQTeUQ-lWXDDMJ4g11y2ajA/banner_raw.png");
    if src.exists() {
        fs::copy(&src, dir.join("banner_raw.png")).unwrap();
    }
    brand_images(&dir, "WattWise Home", "Lower bills. Smarter homes.", "WW", "#F5B700", "#0B3D2E").unwrap();
    assert!(dir.join("banner.jpg").exists() && dir.join("avatar.png").exists());
}
