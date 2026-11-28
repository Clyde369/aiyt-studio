//! Helpers fetched on first use: yt-dlp (downloads source clips) and a small Whisper model (local AI subtitles).

use crate::llm::{command, http_err, run};
use crate::settings::out_dir;
use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

const YT_DLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const WHISPER_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin";

/// Lives inside the output folder so ffmpeg (run from a job folder) can reach the model as `../.aiyt-tools/…`
/// without Windows drive-letter escaping in filter arguments.
fn tools_dir() -> PathBuf {
    out_dir().join(".aiyt-tools")
}

fn download(url: &str, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest.parent().unwrap())?;
    let part = dest.with_extension("part");
    let mut reader = ureq::get(url).call().map_err(http_err)?.into_reader();
    std::io::copy(&mut reader, &mut File::create(&part)?)?;
    fs::rename(part, dest)?;
    Ok(())
}

/// Like `download`, but logs every 10% (and the speed) so a big download visibly moves.
fn download_progress(url: &str, dest: &Path, log: &dyn Fn(&str)) -> Result<()> {
    use std::io::{Read, Write};
    fs::create_dir_all(dest.parent().unwrap())?;
    let part = dest.with_extension("part");
    let resp = ureq::get(url).call().map_err(http_err)?;
    let total: u64 = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let (mut reader, mut file) = (resp.into_reader(), File::create(&part)?);
    let (mut buf, mut done, mut next) = (vec![0u8; 1 << 20], 0u64, 10u64);
    let started = std::time::Instant::now();
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        if total > 0 && done * 100 / total >= next {
            let mbps = done as f64 / 1e6 / started.elapsed().as_secs_f64().max(0.1);
            log(&format!("  {next}% of {:.0} MB ({mbps:.1} MB/s)", total as f64 / 1e6));
            next += 10;
        }
    }
    drop(file);
    fs::rename(part, dest)?;
    Ok(())
}

/// Only clip sites we support, over https. Blocks local files and option-looking strings.
pub fn check_clip_url(url: &str) -> Result<&'static str> {
    let rest = url.strip_prefix("https://").context("clip links must start with https://")?;
    let host = rest.split(['/', '?']).next().unwrap_or("").trim_start_matches("www.").trim_start_matches("m.");
    Ok(match host {
        "youtube.com" | "youtu.be" => "youtube",
        "twitch.tv" | "clips.twitch.tv" => "twitch",
        "kick.com" => "kick",
        _ => bail!("only YouTube, Twitch and Kick links are supported"),
    })
}

fn yt_dlp(log: &dyn Fn(&str)) -> Result<String> {
    if crate::on_path("yt-dlp") {
        return Ok("yt-dlp".into());
    }
    if !cfg!(windows) {
        bail!("install yt-dlp first (https://github.com/yt-dlp/yt-dlp)");
    }
    let exe = tools_dir().join("yt-dlp.exe");
    if !exe.exists() {
        log("Downloading yt-dlp (one time)…");
        download(YT_DLP_URL, &exe)?;
    }
    Ok(exe.to_string_lossy().into())
}

/// Downloads the clip as `source.mp4` (≤1080p) plus any subtitles YouTube has, into `dir`.
pub fn download_video(url: &str, dir: &Path, log: &dyn Fn(&str)) -> Result<()> {
    check_clip_url(url)?;
    let prog = yt_dlp(log)?;
    let format = "bv*[height<=1080][ext=mp4]+ba[ext=m4a]/b[ext=mp4][height<=1080]/bv*[height<=1080]+ba/b";
    let args = [
        "--no-playlist", "--ignore-errors", "-f", format, "--merge-output-format", "mp4", "-o", "source.%(ext)s",
        "--write-info-json", "--write-subs", "--write-auto-subs", "--sub-langs", "en.*,en", "--convert-subs", "srt", "--", url,
    ];
    let r = run(command(&prog).current_dir(dir).args(args), "yt-dlp");
    if !dir.join("source.mp4").exists() {
        r?;
        bail!("yt-dlp did not produce a video (if this keeps happening, delete {} so a fresh yt-dlp is downloaded)", tools_dir().display());
    }
    Ok(())
}

// Pinned to the last 1.x release: sherpa-onnx 2.x dropped the built-in phonemizer Piper voices need.
const SHERPA: &str = "sherpa-onnx-v1.13.8-win-x64-shared-MT-Release";
/// NVIDIA build: bundles the CUDA 12 + cuDNN 9 runtime, so only a normal NVIDIA driver is needed (~600 MB).
const SHERPA_GPU: &str = "sherpa-onnx-v1.13.8-cuda-12.x-cudnn-9.x-onnxruntime1.28.2-win-x64-cuda";
const SHERPA_RELEASE: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.8";
const KOKORO: &str = "kokoro-int8-multi-lang-v1_0";

/// English Kokoro voices and their speaker ids in the v1.0 model (read from the model's own metadata).
pub const KOKORO_VOICES: [&str; 28] = [
    "af_alloy", "af_aoede", "af_bella", "af_heart", "af_jessica", "af_kore", "af_nicole", "af_nova", "af_river", "af_sarah", "af_sky",
    "am_adam", "am_echo", "am_eric", "am_fenrir", "am_liam", "am_michael", "am_onyx", "am_puck", "am_santa",
    "bf_alice", "bf_emma", "bf_isabella", "bf_lily", "bm_daniel", "bm_fable", "bm_george", "bm_lewis",
];

/// The GPU engine needs NVIDIA's CUDA 12 runtime + cuDNN 9 (not bundled). Found on PATH or in
/// `.aiyt-tools/cuda-runtime`? Returns the folder to add to PATH (None = not installed).
pub fn cuda_runtime() -> Option<PathBuf> {
    let local = tools_dir().join("cuda-runtime");
    let has = |d: &Path| ["cublas64_12.dll", "cudart64_12.dll", "cudnn64_9.dll"].iter().all(|f| d.join(f).is_file());
    if has(&local) {
        return Some(local);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    // cuDNN often lives in a different folder from the CUDA toolkit; each file just has to be somewhere on PATH.
    ["cublas64_12.dll", "cudart64_12.dll", "cudnn64_9.dll"].iter().all(|f| dirs.iter().any(|d| d.join(f).is_file())).then(PathBuf::new)
}

/// Piper voices (MIT): very light (~65 MB each) and many times faster than real time, even on a CPU.
pub const PIPER_VOICES: [&str; 10] = [
    "en_US-lessac-medium", "en_US-ryan-medium", "en_US-amy-medium", "en_US-joe-medium", "en_US-john-medium",
    "en_US-kristin-medium", "en_US-hfc_female-medium", "en_US-hfc_male-medium", "en_GB-alan-medium", "en_GB-cori-medium",
];

const TTS_MODELS: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models";

/// Download a .tar.bz2 into the tools folder and unpack it there (Windows 10+ ships bsdtar).
fn fetch_archive(name: &str, url: &str) -> Result<()> {
    let dir = tools_dir();
    let archive = dir.join(format!("{name}.tar.bz2"));
    download(url, &archive)?;
    let r = run(command(&tar_exe()).current_dir(&dir).args(["-xjf", &format!("{name}.tar.bz2")]), "tar");
    let _ = fs::remove_file(&archive);
    r.map(|_| ())
}

fn tar_exe() -> String {
    let tar = Path::new(r"C:\Windows\System32\tar.exe");
    if tar.exists() { tar.to_string_lossy().to_string() } else { "tar".into() }
}

/// The sherpa-onnx TTS program (CPU build, or the CUDA build for NVIDIA GPUs). Downloads once.
pub fn tts_engine(log: &dyn Fn(&str), gpu: bool) -> Result<PathBuf> {
    if !cfg!(windows) {
        bail!("local voices are set up automatically on Windows only");
    }
    let engine = if gpu { SHERPA_GPU } else { SHERPA };
    let exe = tools_dir().join(engine).join("bin").join("sherpa-onnx-offline-tts.exe");
    if !exe.exists() && gpu {
        log("Downloading the GPU voice engine (~600 MB, one time)…");
        fetch_archive(SHERPA_GPU, &format!("{SHERPA_RELEASE}/{SHERPA_GPU}.tar.bz2"))?;
    } else if !exe.exists() {
        log("Downloading the local voice engine (~32 MB, one time)…");
        fetch_archive(SHERPA, &format!("{SHERPA_RELEASE}/{SHERPA}.tar.bz2"))?;
    }
    if !exe.exists() {
        bail!("the voice engine did not install correctly; delete {} and try again", tools_dir().display());
    }
    // Engines from sherpa-onnx 2.x cannot run Piper and are no longer used; free their space (the GPU one is ~1.1 GB).
    for e in fs::read_dir(tools_dir()).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("sherpa-onnx-v2.") {
            let _ = fs::remove_dir_all(e.path());
        }
    }
    Ok(exe)
}

/// Kokoro (Apache 2.0) model folder, ~130 MB once.
pub fn kokoro_model(log: &dyn Fn(&str)) -> Result<PathBuf> {
    let model = tools_dir().join(KOKORO);
    if !model.join("voices.bin").exists() {
        log("Downloading the Kokoro voices (~130 MB, one time)…");
        fetch_archive(KOKORO, &format!("{TTS_MODELS}/{KOKORO}.tar.bz2"))?;
    }
    Ok(model)
}

/// One Piper voice: (model .onnx, voice folder). ~65 MB once per voice.
pub fn piper_model(log: &dyn Fn(&str), voice: &str) -> Result<(PathBuf, PathBuf)> {
    let voice = if PIPER_VOICES.contains(&voice) { voice } else { PIPER_VOICES[0] };
    let name = format!("vits-piper-{voice}");
    let dir = tools_dir().join(&name);
    if !dir.join("tokens.txt").exists() {
        log(&format!("Downloading the Piper voice {voice} (~65 MB, one time)…"));
        fetch_archive(&name, &format!("{TTS_MODELS}/{name}.tar.bz2"))?;
    }
    let onnx = fs::read_dir(&dir)?.filter_map(|e| e.ok().map(|e| e.path())).find(|p| p.extension().is_some_and(|x| x == "onnx"));
    Ok((onnx.with_context(|| format!("Piper voice {voice} is missing its model; delete {} and try again", dir.display()))?, dir))
}

/// NVIDIA's official redistributable CUDA 12 + cuDNN 9 runtime files, unpacked into `.aiyt-tools/cuda-runtime`
/// so the GPU voices work without installing the CUDA toolkit. ~2.6 GB of downloads, only on request.
pub fn setup_cuda(log: &dyn Fn(&str)) -> Result<()> {
    const CUDA: &str = "https://developer.download.nvidia.com/compute/cuda/redist";
    const CUDNN: &str = "https://developer.download.nvidia.com/compute/cudnn/redist";
    let packs = [
        ("CUDA runtime", format!("{CUDA}/cuda_cudart/windows-x86_64/cuda_cudart-windows-x86_64-12.9.79-archive.zip")),
        ("cuBLAS (~550 MB)", format!("{CUDA}/libcublas/windows-x86_64/libcublas-windows-x86_64-12.9.2.10-archive.zip")),
        ("cuFFT (~200 MB)", format!("{CUDA}/libcufft/windows-x86_64/libcufft-windows-x86_64-11.4.1.4-archive.zip")),
        ("cuDNN (~1.9 GB)", format!("{CUDNN}/cudnn/windows-x86_64/cudnn-windows-x86_64-9.28.1.4_cuda12-archive.zip")),
    ];
    let runtime = tools_dir().join("cuda-runtime");
    let work = tools_dir().join("cuda-download");
    fs::create_dir_all(&runtime)?;
    for (i, (label, url)) in packs.iter().enumerate() {
        log(&format!("GPU pack {}/{}: downloading {label}…", i + 1, packs.len()));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work)?;
        let zip = work.join("pack.zip");
        download_progress(url, &zip, log)?;
        log("  unpacking…");
        // Not tied to the Cancel button (that stops short-making), so cancelling a short never breaks this download.
        let out = command(&tar_exe()).current_dir(&work).args(["-xf", "pack.zip"]).output()?;
        if !out.status.success() {
            bail!("could not unpack the NVIDIA files: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        // Only the DLLs are needed; flatten them into one folder.
        let mut stack = vec![work.clone()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d)?.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")) {
                    fs::copy(&p, runtime.join(p.file_name().unwrap()))?;
                }
            }
        }
    }
    let _ = fs::remove_dir_all(&work);
    if cuda_runtime().is_none() {
        bail!("the NVIDIA files did not unpack as expected");
    }
    log("GPU pack ready ✔ Local voices will now use your NVIDIA GPU.");
    Ok(())
}

/// Local Whisper transcription through ffmpeg's whisper filter. Returns (start, end, text) segments.
pub fn transcribe(dir: &Path, input: &str, out_srt: &str, lang: &str, log: &dyn Fn(&str)) -> Result<Vec<(f64, f64, String)>> {
    transcribe_with(dir, input, out_srt, "../.aiyt-tools/ggml-base.bin", lang, log)
}

/// `model_rel` is the Whisper model's path relative to `dir` (relative, so the filter needs no drive-letter escaping).
pub fn transcribe_with(dir: &Path, input: &str, out_srt: &str, model_rel: &str, lang: &str, log: &dyn Fn(&str)) -> Result<Vec<(f64, f64, String)>> {
    transcribe_chunked(dir, input, out_srt, model_rel, lang, 3, log)
}

/// `queue` = seconds of audio Whisper hears at a time: 3 gives finely timed subtitles; ~20 hears whole sentences
/// (better for checking what was said: short chunks cut words in half).
pub fn transcribe_chunked(dir: &Path, input: &str, out_srt: &str, model_rel: &str, lang: &str, queue: u32, log: &dyn Fn(&str)) -> Result<Vec<(f64, f64, String)>> {
    let model = tools_dir().join("ggml-base.bin");
    if !model.exists() {
        log("Downloading the Whisper subtitle model (~150 MB, one time)…");
        download(WHISPER_URL, &model)?;
    }
    // A fixed language: auto-detect misfires on short or music-heavy audio (captions came out in French/Tamil).
    let lang: String = lang.chars().filter(|c| c.is_ascii_alphabetic()).take(8).collect();
    let lang = if lang.is_empty() { "auto".to_string() } else { lang };
    let af = format!("whisper=model={model_rel}:language={lang}:queue={queue}:destination={out_srt}:format=srt");
    crate::media::ffmpeg(dir, &["-i", input, "-vn", "-af", &af, "-f", "null", "-"])?;
    Ok(parse_srt(&fs::read_to_string(dir.join(out_srt)).unwrap_or_default()))
}

/// For analysis only: first 10 minutes at up to 720p, plus its info (title, views, likes). Any site yt-dlp supports.
pub fn download_for_analysis(url: &str, dir: &Path, log: &dyn Fn(&str)) -> Result<()> {
    crate::watch::check_watch_url(url)?;
    let prog = yt_dlp(log)?;
    let args = [
        "--no-playlist", "-f", "bv*[height<=720]+ba/b[height<=720]/b", "--merge-output-format", "mp4", "-o", "source.%(ext)s",
        "--write-info-json", "--download-sections", "*0-600", "--", url,
    ];
    let r = run(command(&prog).current_dir(dir).args(args), "yt-dlp");
    if !dir.join("source.mp4").exists() {
        r?;
        bail!("could not download that video (private, age-restricted, or the site changed; try again later)");
    }
    Ok(())
}

/// SRT (or yt-dlp's converted auto-captions) -> segments, with markup and rolling duplicates removed.
pub fn parse_srt(s: &str) -> Vec<(f64, f64, String)> {
    let ts = |t: &str| -> Option<f64> {
        let t = t.trim().replace(',', ".");
        let p: Vec<&str> = t.split(':').collect();
        let (h, m, sec) = match p.as_slice() {
            [h, m, s] => (h.parse::<f64>().ok()?, m.parse::<f64>().ok()?, s.parse::<f64>().ok()?),
            [m, s] => (0.0, m.parse::<f64>().ok()?, s.parse::<f64>().ok()?),
            _ => return None,
        };
        Some(h * 3600.0 + m * 60.0 + sec)
    };
    let mut out: Vec<(f64, f64, String)> = vec![];
    for block in s.replace("\r\n", "\n").split("\n\n") {
        let mut lines = block.lines().skip_while(|l| !l.contains("-->"));
        let Some(times) = lines.next() else { continue };
        let Some((a, b)) = times.split_once("-->") else { continue };
        let (Some(a), Some(b)) = (ts(a), ts(b.split_whitespace().next().unwrap_or(""))) else { continue };
        let mut text = String::new();
        let mut in_tag = false;
        for c in lines.collect::<Vec<_>>().join(" ").chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => text.push(c),
                _ => {}
            }
        }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() || out.last().is_some_and(|l| l.2 == text) {
            continue;
        }
        out.push((a, b, text));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_and_urls() {
        let srt = "1\r\n00:00:01,000 --> 00:00:02,500\r\nHello <c>world</c>\r\n\r\n2\r\n00:00:02,500 --> 00:00:04,000 align:start\r\nHello world\r\n\r\n3\r\n00:01:02.250 --> 00:01:03.000\r\nNext line\r\n";
        let segs = parse_srt(srt);
        assert_eq!(segs.len(), 2, "{segs:?}");
        assert_eq!(segs[0], (1.0, 2.5, "Hello world".into()));
        assert_eq!(segs[1].0, 62.25);
        assert_eq!(check_clip_url("https://www.youtube.com/watch?v=x").unwrap(), "youtube");
        assert_eq!(check_clip_url("https://clips.twitch.tv/Abc").unwrap(), "twitch");
        assert_eq!(check_clip_url("https://kick.com/user?clip=1").unwrap(), "kick");
        assert!(check_clip_url("http://youtube.com/x").is_err());
        assert!(check_clip_url("https://evil.com/youtube.com").is_err());
        assert!(check_clip_url("--exec calc").is_err());
    }
}

// ---------- Local FLUX images (stable-diffusion.cpp, Vulkan: runs on any modern GPU without CUDA) ----------
const SD_ZIP: &str = "https://github.com/leejet/stable-diffusion.cpp/releases/download/master-1141-e637c7b/sd-master-e637c7b-bin-win-vulkan-x64.zip";
const FLUX_REPO: &str = "https://huggingface.co/second-state/FLUX.1-schnell-GGUF/resolve/main"; // Apache 2.0, ungated
const FLUX_FILES: [(&str, &str); 4] = [
    ("flux1-schnell-Q4_0.gguf", "FLUX.1-schnell model (~6.7 GB)"),
    ("t5xxl-Q4_0.gguf", "T5 text encoder (~2.8 GB)"),
    ("clip_l.safetensors", "CLIP text encoder (~250 MB)"),
    ("ae.safetensors", "image decoder (~335 MB)"),
];

fn flux_dir() -> PathBuf {
    tools_dir().join("flux")
}

/// The stable-diffusion.cpp program, wherever the zip put it.
pub fn flux_exe() -> Option<PathBuf> {
    let mut stack = vec![flux_dir().join("engine")];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == "sd.exe" || n == "sd-cli.exe") {
                return Some(p);
            }
        }
    }
    None
}

pub fn flux_ready() -> bool {
    flux_exe().is_some() && FLUX_FILES.iter().all(|(f, _)| flux_dir().join(f).is_file())
}

/// One-time download of the engine and FLUX.1-schnell (~10 GB, with progress). Safe to run again: skips what is there.
pub fn setup_flux(log: &dyn Fn(&str)) -> Result<()> {
    let dir = flux_dir();
    fs::create_dir_all(dir.join("engine"))?;
    if flux_exe().is_none() {
        log("FLUX 1/5: downloading the image engine (~31 MB)…");
        download_progress(SD_ZIP, &dir.join("engine").join("sd.zip"), log)?;
        let out = command(&tar_exe()).current_dir(dir.join("engine")).args(["-xf", "sd.zip"]).output()?;
        let _ = fs::remove_file(dir.join("engine").join("sd.zip"));
        if !out.status.success() || flux_exe().is_none() {
            bail!("could not unpack the image engine");
        }
    }
    for (i, (file, label)) in FLUX_FILES.iter().enumerate() {
        if dir.join(file).is_file() {
            continue;
        }
        log(&format!("FLUX {}/5: downloading the {label}…", i + 2));
        download_progress(&format!("{FLUX_REPO}/{file}"), &dir.join(file), log)?;
    }
    log("FLUX ready ✔ Images are now made on your GPU, free and unlimited.");
    Ok(())
}

/// Paths for one FLUX run: (exe, [model args]).
pub fn flux_args() -> Result<(PathBuf, Vec<String>)> {
    let exe = flux_exe().context("local FLUX is not set up yet: Settings → Images → Set up FLUX")?;
    let p = |f: &str| flux_dir().join(f).to_string_lossy().to_string();
    if !flux_ready() {
        bail!("local FLUX is not fully downloaded yet: Settings → Images → Set up FLUX");
    }
    Ok((exe, vec![
        "--diffusion-model".into(), p("flux1-schnell-Q4_0.gguf"), "--vae".into(), p("ae.safetensors"),
        "--clip_l".into(), p("clip_l.safetensors"), "--t5xxl".into(), p("t5xxl-Q4_0.gguf"),
    ]))
}

// ---------- Story tools: ComfyUI + SDXL-Lightning (Slow-style pictures) and GiveMotion (the animator) ----------

const COMFY_7Z: &str = "https://github.com/Comfy-Org/ComfyUI/releases/download/v0.46.0/ComfyUI_windows_portable_nvidia.7z";
/// ByteDance SDXL-Lightning, 4 steps: ~9 s a picture on an 8 GB card. Licence: OpenRAIL++ (commercial use allowed).
const SDXL_URL: &str = "https://huggingface.co/ByteDance/SDXL-Lightning/resolve/main/sdxl_lightning_4step.safetensors";
const SDXL_FILE: &str = "sdxl_lightning_4step.safetensors";
/// Not ComfyUI's default 8188, so a ComfyUI the user runs themselves is never touched.
const COMFY_PORT: u16 = 8189;
/// Pinned so a new upstream release can never change (or break) rendering by surprise.
const GIVEMOTION: &str = "givemotion@0.1.6";

static COMFY: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);
static COMFY_USED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn comfy_root() -> PathBuf {
    tools_dir().join("comfyui")
}
fn comfy_base() -> PathBuf {
    comfy_root().join("ComfyUI_windows_portable")
}
fn comfy_python() -> PathBuf {
    comfy_base().join("python_embeded").join("python.exe")
}
fn sdxl_file() -> PathBuf {
    comfy_root().join("models").join("checkpoints").join(SDXL_FILE)
}

/// Local pictures for Slow-style stories are installed.
pub fn comfy_ready() -> bool {
    comfy_python().exists() && sdxl_file().is_file()
}

/// One-time download: ComfyUI (~2 GB) + SDXL-Lightning (~6.5 GB). Safe to run again: skips what is there.
pub fn setup_comfy(log: &dyn Fn(&str)) -> Result<()> {
    if !cfg!(windows) {
        bail!("local story pictures are set up automatically on Windows only");
    }
    fs::create_dir_all(comfy_root())?;
    if !comfy_python().exists() {
        log("Downloading ComfyUI, the local picture engine (~2 GB, one time)…");
        let archive = comfy_root().join("comfyui.7z");
        download_progress(COMFY_7Z, &archive, log)?;
        log("Unpacking ComfyUI…");
        let r = run(command(&tar_exe()).current_dir(comfy_root()).args(["-xf", "comfyui.7z"]), "tar");
        let _ = fs::remove_file(&archive);
        r?;
    }
    if !sdxl_file().is_file() {
        log("Downloading SDXL-Lightning, the fast picture model (~6.5 GB, one time)…");
        download_progress(SDXL_URL, &sdxl_file(), log)?;
    }
    comfy_paths()?;
    log("Story pictures ready ✔ About 9 seconds a picture on your GPU, free.");
    Ok(())
}

/// Tells ComfyUI where the models are (they live outside its folder so an upgrade keeps them).
fn comfy_paths() -> Result<()> {
    let models = comfy_root().join("models").to_string_lossy().replace('\\', "/");
    fs::write(comfy_base().join("ComfyUI").join("extra_model_paths.yaml"), format!("aiyt:\n  base_path: \"{models}\"\n  checkpoints: checkpoints\n"))?;
    Ok(())
}

fn comfy_url(path: &str) -> String {
    format!("http://127.0.0.1:{COMFY_PORT}{path}")
}

fn comfy_up() -> bool {
    ureq::get(&comfy_url("/system_stats")).timeout(std::time::Duration::from_secs(2)).call().is_ok()
}

fn touch_comfy() {
    COMFY_USED.store(crate::youtube::now_secs(), std::sync::atomic::Ordering::SeqCst);
}

/// Starts the hidden ComfyUI server if it is not running. It stops itself after 10 idle minutes (and when the app
/// closes), so it never keeps the GPU's memory while you do other things.
fn comfy_start(log: &dyn Fn(&str)) -> Result<()> {
    if comfy_up() {
        return Ok(());
    }
    if !comfy_ready() {
        bail!("local story pictures are not set up yet: Stories → Set up (or choose Fast style / Gemini pictures)");
    }
    comfy_paths()?;
    let err_log = File::create(comfy_root().join("comfyui.log"))?;
    let child = command(&comfy_python().to_string_lossy())
        .current_dir(comfy_base())
        .args(["-s", "ComfyUI/main.py", "--listen", "127.0.0.1", "--port", &COMFY_PORT.to_string(), "--disable-auto-launch"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(err_log)
        .spawn()
        .context("could not start ComfyUI")?;
    *COMFY.lock().unwrap() = Some(child);
    touch_comfy();
    log("Starting the local picture engine (about a minute)…");
    let started = std::time::Instant::now();
    while !comfy_up() {
        if crate::llm::cancelled() {
            comfy_stop();
            bail!("cancelled");
        }
        let exited = COMFY.lock().unwrap().as_mut().is_none_or(|c| c.try_wait().ok().flatten().is_some());
        if exited || started.elapsed().as_secs() > 300 {
            comfy_stop();
            let tail: String = fs::read_to_string(comfy_root().join("comfyui.log")).unwrap_or_default().chars().rev().take(800).collect::<Vec<_>>().into_iter().rev().collect();
            bail!("the local picture engine did not start: {tail}");
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    static WATCH: std::sync::Once = std::sync::Once::new();
    WATCH.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
            let idle = crate::youtube::now_secs().saturating_sub(COMFY_USED.load(std::sync::atomic::Ordering::SeqCst));
            if idle > 600 && COMFY.lock().unwrap().is_some() {
                comfy_stop();
            }
        });
    });
    Ok(())
}

/// Stops the ComfyUI server this app started (no-op otherwise).
pub fn comfy_stop() {
    if let Some(mut c) = COMFY.lock().unwrap().take() {
        if cfg!(windows) {
            let _ = command("taskkill").args(["/T", "/F", "/PID", &c.id().to_string()]).status();
        }
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// One 768x1344 picture from SDXL-Lightning through ComfyUI's API, saved as `out` (PNG).
pub fn comfy_image(prompt: &str, negative: Option<&str>, seed: u64, out: &Path, log: &dyn Fn(&str)) -> Result<()> {
    comfy_start(log)?;
    touch_comfy();
    let negative = negative.unwrap_or("comic strip, comic page, panels, grid, collage, split screen, multiple scenes, text, letters, words, caption, speech bubble, watermark, signature, logo, blurry, deformed, extra fingers, extra limbs, ugly, photo, realistic");
    let wf = serde_json::json!({
        "1": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": SDXL_FILE}},
        "2": {"class_type": "CLIPTextEncode", "inputs": {"text": prompt, "clip": ["1", 1]}},
        "3": {"class_type": "CLIPTextEncode", "inputs": {"text": negative, "clip": ["1", 1]}},
        "4": {"class_type": "EmptyLatentImage", "inputs": {"width": 768, "height": 1344, "batch_size": 1}},
        "5": {"class_type": "KSampler", "inputs": {"model": ["1", 0], "seed": seed, "steps": 4, "cfg": 1.0, "sampler_name": "euler",
              "scheduler": "sgm_uniform", "positive": ["2", 0], "negative": ["3", 0], "latent_image": ["4", 0], "denoise": 1.0}},
        "6": {"class_type": "VAEDecode", "inputs": {"samples": ["5", 0], "vae": ["1", 2]}},
        "7": {"class_type": "SaveImage", "inputs": {"images": ["6", 0], "filename_prefix": "aiyt"}}
    });
    let r: serde_json::Value = ureq::post(&comfy_url("/prompt")).send_json(serde_json::json!({"prompt": wf})).map_err(http_err)?.into_json()?;
    let id = r["prompt_id"].as_str().context("ComfyUI did not accept the picture")?.to_string();
    let started = std::time::Instant::now();
    loop {
        if crate::llm::cancelled() {
            let _ = ureq::post(&comfy_url("/interrupt")).call();
            bail!("cancelled");
        }
        let h: serde_json::Value = ureq::get(&comfy_url(&format!("/history/{id}"))).call().map_err(http_err)?.into_json()?;
        let job = &h[&id];
        if !job.is_null() {
            if job["status"]["status_str"] == "error" {
                bail!("the picture failed: {}", job["status"]["messages"].to_string().chars().take(600).collect::<String>());
            }
            let img = &job["outputs"]["7"]["images"][0];
            let file = img["filename"].as_str().context("ComfyUI made no picture")?;
            let resp = ureq::get(&comfy_url("/view"))
                .query("filename", file)
                .query("subfolder", img["subfolder"].as_str().unwrap_or(""))
                .query("type", img["type"].as_str().unwrap_or("output"))
                .call()
                .map_err(http_err)?;
            let mut bytes = vec![];
            std::io::Read::read_to_end(&mut resp.into_reader(), &mut bytes)?;
            fs::write(out, bytes)?;
            let _ = fs::remove_file(comfy_base().join("ComfyUI").join("output").join(img["subfolder"].as_str().unwrap_or("")).join(file));
            touch_comfy();
            return Ok(());
        }
        if started.elapsed().as_secs() > 600 {
            bail!("the picture took more than 10 minutes");
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}

fn gm_dir() -> PathBuf {
    tools_dir().join("givemotion")
}
fn gm_cli() -> PathBuf {
    gm_dir().join("node_modules").join("givemotion").join("src").join("cli.js")
}

/// The story animator (GiveMotion, MIT) is installed. It needs Node.js.
pub fn givemotion_ready() -> bool {
    gm_cli().exists() && crate::on_path("node")
}

/// Installs the pinned GiveMotion into the tools folder and the headless browser it records frames with.
pub fn setup_givemotion(log: &dyn Fn(&str)) -> Result<()> {
    if !crate::on_path("node") {
        bail!("story animation needs Node.js: install it (winget install OpenJS.NodeJS.LTS), then restart AIYT Studio");
    }
    if !gm_cli().exists() {
        log("Installing GiveMotion, the story animator…");
        fs::create_dir_all(gm_dir())?;
        let prefix = gm_dir().to_string_lossy().to_string();
        run(command("cmd").args(["/C", "npm", "install", "--prefix", &prefix, GIVEMOTION, "--no-audit", "--no-fund", "--loglevel=error"]), "npm")?;
    }
    log("Checking the animator's browser (downloads once if missing)…");
    run(command("node").arg(gm_cli()).arg("install"), "GiveMotion")?;
    log("Story animator ready ✔");
    Ok(())
}

/// Renders `page` (an HTML composition in `dir`) to `out` (MP4) with GiveMotion.
pub fn givemotion_render(dir: &Path, page: &str, out: &str) -> Result<()> {
    if !givemotion_ready() {
        bail!("the story animator is not set up yet: Stories → Set up");
    }
    run(command("node").current_dir(dir).arg(gm_cli()).args(["render", page, "-o", out]), "GiveMotion")?;
    Ok(())
}

/// One frame of `page` at `t` seconds, as a PNG.
pub fn givemotion_still(dir: &Path, page: &str, t: f64, out: &str) -> Result<()> {
    run(command("node").current_dir(dir).arg(gm_cli()).args(["still", page, "-t", &format!("{t:.2}"), "-o", out]), "GiveMotion")?;
    Ok(())
}
