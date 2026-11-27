//! Text AI: Gemini API, your Claude / ChatGPT / OpenCode subscriptions (via their CLIs), or a local Ollama model.

use crate::settings::Settings;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

/// Ask the AI chosen for this job type ("script", "research", "clips", "replies") for a JSON object.
pub fn generate_json(s: &Settings, task: &str, prompt: &str) -> Result<Value> {
    let raw = generate(s, task, prompt)?;
    let v = extract_json(&raw).with_context(|| format!("the AI did not return valid JSON:\n{}", raw.chars().take(1500).collect::<String>()))?;
    // Check the raw text AND every decoded field (JSON escapes like A only show after parsing).
    if looks_like_secret(&raw) || any_string(&v, &looks_like_secret) {
        bail!("the AI's answer contained something that looks like a key or token, so it was discarded");
    }
    Ok(v)
}

pub fn generate(s: &Settings, task: &str, prompt: &str) -> Result<String> {
    let (writer, model) = s.pick(task);
    let model = model.as_str();
    let record = |m: &str, i: u64, o: u64| crate::usage::record(&writer, m, task, i, o);
    let with_model = |mut args: Vec<String>, flag: &str| {
        if !model.is_empty() {
            args.extend([flag.to_string(), model.to_string()]);
        }
        args
    };
    match writer.as_str() {
        "gemini" => {
            let model = if model.is_empty() { s.gemini_text_model.as_str() } else { model };
            let body = json!({
                "contents": [{"parts": [{"text": prompt}]}],
                "generationConfig": {"responseMimeType": "application/json"}
            });
            const FALLBACK: &str = "gemini-flash-lite-latest"; // usually has capacity when Flash is overloaded
            let mut used = model;
            let resp = gemini(s, model, &body).or_else(|e| match e.to_string().starts_with("HTTP 503") && model != FALLBACK {
                true => {
                    used = FALLBACK;
                    gemini(s, FALLBACK, &body)
                }
                false => Err(e),
            })?;
            let u = &resp["usageMetadata"];
            record(used, u["promptTokenCount"].as_u64().unwrap_or(0), u["candidatesTokenCount"].as_u64().unwrap_or(0));
            let parts = resp["candidates"][0]["content"]["parts"].as_array().context("Gemini returned no text")?;
            Ok(parts.iter().filter_map(|p| p["text"].as_str()).collect())
        }
        // Each CLI uses whatever account it is logged into, so your subscription pays, not an API key.
        "claude" => {
            // Lean call: no Claude Code settings, plugins, MCP servers or tools are loaded (~1K tokens instead of ~100K
            // per call), and `--tools ""` means instructions hidden in YouTube titles/comments cannot run anything.
            let model = if model.is_empty() { "sonnet" } else { model };
            let args: Vec<String> = [
                "-p", "--output-format", "json", "--tools", "", "--setting-sources", "", "--strict-mcp-config", "--disable-slash-commands",
                "--no-session-persistence", "--system-prompt", "You are a precise writing assistant. Follow the requested format exactly.", "--model", model,
            ]
            .map(String::from)
            .to_vec();
            let raw = cli("claude", &args, Some(prompt))?;
            let v: Value = serde_json::from_str(raw.trim()).with_context(|| format!("unexpected Claude output: {}", raw.chars().take(500).collect::<String>()))?;
            if v["is_error"] == true {
                bail!("Claude: {}", v["result"].as_str().unwrap_or("error"));
            }
            let tok = |k: &str| v["usage"][k].as_u64().unwrap_or(0);
            record(model, tok("input_tokens") + tok("cache_creation_input_tokens") + tok("cache_read_input_tokens"), tok("output_tokens"));
            Ok(v["result"].as_str().unwrap_or_default().to_string())
        }
        "codex" => {
            let out = temp_file("codex-answer.txt");
            let args = vec!["exec".into(), "--skip-git-repo-check".into(), "-s".into(), "read-only".into(), "-o".into(), out.clone(), "-".into()];
            cli("codex", &with_model(args, "-m"), Some(prompt))?;
            let text = std::fs::read_to_string(&out).context("codex wrote no answer")?;
            let _ = std::fs::remove_file(&out);
            record(model, 0, 0); // Codex tracks its own limits; they are read from its session logs
            Ok(text)
        }
        "opencode" => {
            // Long prompts go in an attached file; the command line only carries a fixed instruction.
            let file = temp_file("opencode-prompt.md");
            std::fs::write(&file, prompt)?;
            let args = vec!["run".into(), "Follow the instructions in the attached file exactly and reply with only what they ask for.".into(), "-f".into(), file.clone()];
            let out = cli("opencode", &with_model(args, "-m"), None);
            let _ = std::fs::remove_file(&file);
            record(model, 0, 0);
            out
        }
        "ollama" => {
            let model = if model.is_empty() { "llama3.2:3b" } else { model };
            let host = s.ollama_host.trim_end_matches('/');
            let resp: Value = ureq::post(&format!("{host}/api/generate"))
                .send_json(json!({"model": model, "prompt": prompt, "stream": false, "format": "json"}))
                .map_err(|e| match e {
                    ureq::Error::Transport(_) => anyhow!("can't reach Ollama at {host}; is it running?"),
                    e => http_err(e).context(format!("try `ollama pull {model}`")),
                })?
                .into_json()?;
            record(model, resp["prompt_eval_count"].as_u64().unwrap_or(0), resp["eval_count"].as_u64().unwrap_or(0));
            Ok(resp["response"].as_str().unwrap_or_default().to_string())
        }
        w => bail!("unknown writer '{w}'"),
    }
}

/// Model names to offer in the per-job pickers.
pub fn list_models(s: &Settings, writer: &str) -> Vec<String> {
    let json_get = |url: &str, key: &str| ureq::get(url).set("x-goog-api-key", key).call().ok().and_then(|r| r.into_json::<Value>().ok()).unwrap_or_default();
    match writer {
        "claude" => ["haiku", "sonnet", "opus"].map(String::from).to_vec(),
        "gemini" if !s.gemini_key.trim().is_empty() => json_get("https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000", s.gemini_key.trim())["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["supportedGenerationMethods"].as_array().is_some_and(|a| a.iter().any(|x| x == "generateContent")))
            .filter_map(|m| m["name"].as_str().map(|n| n.trim_start_matches("models/").to_string()))
            .filter(|n| n.starts_with("gemini") && !["tts", "image", "audio", "live"].iter().any(|x| n.contains(x)))
            .collect(),
        "opencode" => {
            let exe = if cfg!(windows) { "opencode.cmd" } else { "opencode" };
            run(command(exe).arg("models"), "opencode")
                .map(|o| String::from_utf8_lossy(&o).lines().map(str::trim).filter(|l| l.contains('/') && !l.contains(' ')).map(String::from).collect())
                .unwrap_or_default()
        }
        "ollama" => json_get(&format!("{}/api/tags", s.ollama_host.trim_end_matches('/')), "")["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["name"].as_str().map(String::from))
            .collect(),
        _ => vec![],
    }
}

/// POST to the Gemini generateContent endpoint. Shared by text, speech and image generation.
pub fn gemini(s: &Settings, model: &str, body: &Value) -> Result<Value> {
    let key = if s.gemini_key.trim().is_empty() { std::env::var("GEMINI_API_KEY").unwrap_or_default() } else { s.gemini_key.trim().to_string() };
    if key.is_empty() {
        bail!("add your Gemini API key in Settings (free at aistudio.google.com/apikey)");
    }
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
    // Overload (503) and per-minute limits (429) are temporary: back off and retry. Daily "limit: 0" is not.
    for wait in [3, 10, 0] {
        check_cancel()?;
        match ureq::post(&url).set("x-goog-api-key", &key).send_json(body) {
            Ok(r) => return Ok(r.into_json()?),
            Err(e) => {
                let e = http_err(e);
                let msg = e.to_string();
                let transient = msg.starts_with("HTTP 503") || msg.starts_with("HTTP 500") || (msg.starts_with("HTTP 429") && !msg.contains("limit: 0"));
                if !transient || wait == 0 {
                    return Err(e);
                }
                for _ in 0..wait * 5 {
                    check_cancel()?;
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }
    unreachable!()
}

/// Turn ureq status errors into messages that include the API's error body.
pub fn http_err(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, r) => anyhow!("HTTP {code}: {}", r.into_string().unwrap_or_default().chars().take(800).collect::<String>()),
        e => e.into(),
    }
}

/// Models love wrapping JSON in prose or ``` fences; take the outermost {...}.
pub fn extract_json(s: &str) -> Option<Value> {
    let (start, end) = (s.find('{')?, s.rfind('}')?);
    serde_json::from_str(s.get(start..=end)?).ok()
}

/// A Command that never flashes a console window (we are a GUI app).
pub fn command(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// Set by the Cancel button. Every external program and every pipeline step checks it.
pub static CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn cancelled() -> bool {
    CANCEL.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn check_cancel() -> Result<()> {
    if cancelled() {
        bail!("cancelled");
    }
    Ok(())
}

/// Wait for a child, killing it (and anything it started) on cancel or timeout. Returns (status, stdout, stderr).
fn wait(mut child: std::process::Child, what: &str, timeout: std::time::Duration) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    use std::io::Read;
    let reader = |r: Option<Box<dyn Read + Send>>| std::thread::spawn(move || {
        let mut b = vec![];
        if let Some(mut r) = r {
            let _ = r.read_to_end(&mut b);
        }
        b
    });
    let out = reader(child.stdout.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let err = reader(child.stderr.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        let timed_out = started.elapsed() > timeout;
        if cancelled() || timed_out {
            // .cmd shims start node as a grandchild, so kill the whole tree.
            if cfg!(windows) {
                let _ = command("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).status();
            }
            let _ = child.kill();
            let _ = child.wait();
            if timed_out {
                bail!("{what} did not finish within {} minutes", timeout.as_secs() / 60);
            }
            bail!("cancelled");
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    };
    Ok((status, out.join().unwrap_or_default(), err.join().unwrap_or_default()))
}

/// Run a command; on failure, report the tail of its stderr. Stops immediately on Cancel.
pub fn run(c: &mut Command, what: &str) -> Result<Vec<u8>> {
    run_full(c, what).map(|(out, _)| out)
}

/// Like `run`, but also returns stderr (some tools report what they actually did there). Returns (stdout, stderr).
pub fn run_full(c: &mut Command, what: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    check_cancel()?;
    let child = c
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start {what}; is it installed?"))?;
    let (status, out, err) = wait(child, what, std::time::Duration::from_secs(4 * 3600))?;
    if !status.success() {
        bail!("{what} failed: {}", tail(&err));
    }
    Ok((out, err))
}

fn tail(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    let s = s.trim();
    s.char_indices().rev().nth(1200).map(|(i, _)| &s[i..]).unwrap_or(s).to_string()
}

/// Unique per call, so another process cannot pre-create it.
fn temp_file(name: &str) -> String {
    let mut r = [0u8; 8];
    let _ = getrandom::fill(&mut r);
    let id: String = r.iter().map(|b| format!("{b:02x}")).collect();
    std::env::temp_dir().join(format!("aiyt-{id}-{name}")).to_string_lossy().into()
}

const CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Run an installed AI CLI with the prompt on stdin (keeps the prompt out of shell quoting).
/// Killed after 10 minutes so a stuck CLI cannot freeze the app's task queue.
fn cli(bin: &str, args: &[String], stdin: Option<&str>) -> Result<String> {
    check_cancel()?;
    // AI CLIs only write text for us. Prompts can carry text from strangers (YouTube titles, comments), so the tools
    // run from an empty folder, and OpenCode gets a config that denies shell, file edits and web fetches.
    let sandbox = sandbox_dir()?;
    let spawn = |exe: &str| {
        command(exe)
            .current_dir(&sandbox)
            .env("OPENCODE_CONFIG", sandbox.join("opencode.json"))
            .args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    };
    // Native installs ship `bin.exe`; npm installs a `bin.cmd` shim.
    let mut child = match spawn(bin) {
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::NotFound => spawn(&format!("{bin}.cmd")),
        r => r,
    }
    .with_context(|| format!("could not run `{bin}`; install it and log in first"))?;
    if let (Some(p), Some(mut w)) = (stdin, child.stdin.take()) {
        let p = p.to_string();
        std::thread::spawn(move || drop(w.write_all(p.as_bytes()))); // a CLI that exits early just closes the pipe
    }
    let (status, out, err) = wait(child, &format!("`{bin}`"), CLI_TIMEOUT)?;
    if !status.success() {
        bail!("`{bin}` failed: {}{}", tail(&err), tail(&out));
    }
    Ok(String::from_utf8_lossy(&out).into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn extracts_fenced_json() {
        let v = super::extract_json("Sure!\n```json\n{\"narration\": \"hi {there}\"}\n```").unwrap();
        assert_eq!(v["narration"], "hi {there}");
        assert!(super::extract_json("no json").is_none());
    }
}

#[cfg(test)]
#[test]
fn cancel_kills_running_program() {
    // A 60s real-time ffmpeg job must stop within a couple of seconds of Cancel.
    let t = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(800));
        CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let started = std::time::Instant::now();
    let r = run(command("ffmpeg").args(["-hide_banner", "-re", "-f", "lavfi", "-i", "sine=duration=60", "-f", "null", "-"]), "ffmpeg");
    t.join().unwrap();
    CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(r.unwrap_err().to_string(), "cancelled");
    assert!(started.elapsed().as_secs() < 5, "took {:?}", started.elapsed());
}

/// Working folder for AI CLIs: inside the app's own per-user data folder (not the shared temp folder), holding only
/// the deny-everything OpenCode config, which is rewritten on every call so nothing else can swap it.
fn sandbox_dir() -> Result<std::path::PathBuf> {
    let dir = crate::settings::config_dir().join("ai-sandbox");
    std::fs::create_dir_all(&dir)?;
    // Anything else that appears in here is removed; the folder must stay empty apart from the config.
    for e in std::fs::read_dir(&dir)?.flatten() {
        if e.file_name() != "opencode.json" {
            let p = e.path();
            let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
        }
    }
    std::fs::write(dir.join("opencode.json"), r#"{"$schema":"https://opencode.ai/config.json","permission":{"edit":"deny","bash":"deny","webfetch":"deny","external_directory":"deny"},"tools":{"read":false,"write":false,"edit":false,"patch":false,"bash":false,"grep":false,"glob":false,"list":false,"webfetch":false,"websearch":false,"task":false,"todowrite":false,"todoread":false}}"#)?;
    Ok(dir)
}

/// Key/token shapes (Google, OpenAI, Anthropic, GitHub, OAuth refresh tokens, private keys). Last line of defence
/// before AI text is posted (comment replies) or saved (scripts, descriptions).
pub fn looks_like_secret(t: &str) -> bool {
    // Normalise first so spacing, zero-width characters or case changes cannot slip a key past the check.
    let t: String = t
        .chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{FEFF}'))
        .collect::<String>()
        .to_lowercase();
    let t = t.as_str();
    let markers = ["aiza", "sk-ant-", "sk-proj-", "ghp_", "gho_", "github_pat_", "xoxb-", "-----begin", "\"refresh_token\"", "client_secret"];
    if markers.iter().any(|m| t.contains(m)) {
        return true;
    }
    // Google OAuth refresh tokens start with "1//" followed by a long run of token characters.
    t.match_indices("1//").any(|(i, _)| t[i + 3..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').count() >= 30)
}

#[cfg(test)]
#[test]
fn secret_shapes_are_caught() {
    assert!(looks_like_secret("my key is AIzaSyD-abc123"));
    assert!(looks_like_secret("token 1//0gAbCdEfGhIjKlMnOpQrStUvWxYz0123456789"));
    assert!(!looks_like_secret("Thanks for watching! The ISS orbits every 90 minutes. 1//2 of fans agree"));
}

/// True if `f` holds for any string anywhere inside `v`.
fn any_string(v: &Value, f: &dyn Fn(&str) -> bool) -> bool {
    match v {
        Value::String(x) => f(x),
        Value::Array(a) => a.iter().any(|x| any_string(x, f)),
        Value::Object(o) => o.values().any(|x| any_string(x, f)),
        _ => false,
    }
}

#[cfg(test)]
#[test]
fn secret_filter_survives_tricks() {
    assert!(looks_like_secret("AI za Sy D-abc")); // spaced out
    assert!(looks_like_secret("A\u{200B}IzaSyD")); // zero-width split
    let v: Value = serde_json::from_str(r#"{"reply": "key: AIzaSyD-abc"}"#).unwrap();
    assert!(any_string(&v, &looks_like_secret)); // JSON escape only visible after decoding
}
