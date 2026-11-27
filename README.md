# AIYT Studio

Desktop app (Rust + Tauri) that makes and posts YouTube Shorts: AI scripts, voiceover, captions, stock or AI visuals, thumbnails, niche research, scheduling, clips and comment replies.

## Download

Get the latest **setup exe** (or the portable exe) from [Releases](https://github.com/clyde369/aiyt-studio/releases/latest). The app tells you when a newer version is out.

- Windows 10/11. Install FFmpeg once: `winget install Gyan.FFmpeg`.
- The exe is not code-signed, so Windows may say "Windows protected your PC": click **More info → Run anyway**.
- Everything else (yt-dlp, Whisper, local voices, FLUX) downloads itself inside the app when first used.
- Bring your own keys: a Google OAuth client for YouTube, and optionally Gemini / Pexels / Pixabay / Twitch keys or your Claude / ChatGPT / OpenCode logins. Nothing is shared; keys and logins stay in `%APPDATA%\com.aiyt.studio` on your PC.

## Build from source

```
cargo install tauri-cli --version "^2" --locked
cargo tauri build
```

Publishing a new version (builds in `target-dist`, so a running copy of the app is never touched):

```
.\scripts\release.ps1 -Version 0.1.1 -Notes "What changed" [-UpdateLocal]
```

Requires **FFmpeg** on PATH (`winget install Gyan.FFmpeg`). yt-dlp and the Whisper subtitle model download themselves the first time you make a clip.

## Set up (Settings page)

| What | Why | Where |
|---|---|---|
| AI writer | Writes scripts, research and replies | Gemini key, or a subscription you already have: **Claude** (`claude` CLI), **ChatGPT** (`codex` CLI), **OpenCode** (`opencode` CLI), or local **Ollama** |
| Gemini key | AI voice (and AI images on a paid key) | aistudio.google.com/apikey |
| Pexels / Pixabay keys | Free stock video that is OK to monetize | pexels.com/api, pixabay.com/api/docs |
| Google OAuth client (Desktop app) | Upload, schedule, research, comments, rebrand | Steps are in the app under Settings → YouTube |
| Twitch app (optional) | Find a streamer's top clips | dev.twitch.tv/console |

## What it does

- **Create:** one topic per line → finished 9:16 shorts.
- **Autopilot:** niche → live research → N shorts → scheduled on YouTube (YouTube publishes them; your PC can be off).
- **Niche research:** this month's top Shorts + the tags they use, analysed by your AI.
- **Clips:** YouTube Creative Commons videos (by topic or matched to your channel), Twitch top clips, or pasted Twitch/Kick links → AI picks the best moment and adds a hook, AI voice intro, Whisper subtitles, reframing and a thumbnail.
- **Rebrand channel:** AI redesign for a target niche: names, description, keywords, pillars, first videos, banner and avatar. Only applies to YouTube when you click Apply twice.
- **Library:** preview, edit, upload now or schedule N per day.
- **Comments:** AI drafts replies; nothing posts without your click.

Shorts are saved in `Videos\AIYT`. Keys and your YouTube login stay in `%APPDATA%\com.aiyt.studio`.

## Know before you post

- **API audit for public uploads:** YouTube locks uploads from new, unaudited Google API projects to private. Request the free API audit to post public videos.
- **Upload quota:** the default YouTube API quota (10,000 units/day) covers only a handful of uploads per day. Request more in Google Cloud if you need it.
- **Clips and monetization:** CC BY videos may be reused commercially with credit, which the app adds automatically. YouTube's *reused content* rule still requires real changes before reused footage can earn. Twitch/Kick clips need the streamer's permission. Downloading from YouTube with yt-dlp is not allowed by YouTube's Terms of Service, even for CC videos.
- **AI disclosure:** uploads are marked as containing synthetic media, as YouTube requires for realistic AI content.

## Tests

```
cargo test                                   # unit tests
cargo test -- --ignored gemini_live          # live Gemini (uses your saved key)
cargo test -- --ignored render_clips         # ffmpeg render of stock clips + music
cargo test -- --ignored clip_live --nocapture  # full clip pipeline on a CC BY video (needs the claude CLI)
```
