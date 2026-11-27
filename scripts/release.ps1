<#
.SYNOPSIS
  Publish a new AIYT Studio version to GitHub (code + installer + portable exe) without touching the running app.

.EXAMPLE
  .\scripts\release.ps1 -Version 0.1.1 -Notes "Faster FLUX art, clip fixes"

.NOTES
  Needs: GitHub CLI logged in to the account that owns the repo (gh auth login), Rust + tauri-cli.
  Builds into .\target-dist so the copy you have open (target\release or the installed one) is never locked or replaced.
#>
param(
  [Parameter(Mandatory = $true)][string]$Version,
  [string]$Notes = "",
  [string]$Repo = "clyde369/aiyt-studio",
  # Also refresh the installed copy in %LOCALAPPDATA%\AIYT Studio (skipped automatically while it is running).
  [switch]$UpdateLocal
)
# Native tools (cargo, git) write progress to stderr; Windows PowerShell 5.1 would treat that as fatal under "Stop",
# so failures are detected from exit codes instead.
$ErrorActionPreference = "Continue"
Set-Location (Split-Path $PSScriptRoot -Parent)

if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Version must look like 1.2.3" }
if (git status --porcelain --untracked-files=no | Select-String '.') { Write-Host "Committing your pending changes as part of the release." }

# 1. Version in both places Tauri and Cargo read it.
# (.NET WriteAllText = UTF-8 without BOM; a BOM would break Cargo.toml)
[IO.File]::WriteAllText((Resolve-Path Cargo.toml).Path, ((Get-Content Cargo.toml -Raw) -replace '(?m)^version = "[^"]+"', "version = `"$Version`""))
[IO.File]::WriteAllText((Resolve-Path tauri.conf.json).Path, ((Get-Content tauri.conf.json -Raw) -replace '"version":\s*"[^"]+"', "`"version`": `"$Version`""))

# 2. Tests, then a release build in its own folder.
cargo test
if ($LASTEXITCODE -ne 0) { throw "tests failed; nothing was published" }
$env:CARGO_TARGET_DIR = "target-dist"
cargo tauri build
if ($LASTEXITCODE -ne 0) { throw "build failed; nothing was published" }
Remove-Item Env:CARGO_TARGET_DIR

$installer = Get-ChildItem "target-dist\release\bundle\nsis\*_${Version}_x64-setup.exe" | Select-Object -First 1
if (-not $installer) { throw "installer not found" }
$portable = "target-dist\AIYT-Studio-$Version-portable.exe"
Copy-Item "target-dist\release\aiyt.exe" $portable -Force

# 3. Code + tag to GitHub, then the release with both downloads.
git add -A
git commit -m "Release v$Version$(if ($Notes) { ": $Notes" })" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git tag "v$Version"
git push origin HEAD
if ($LASTEXITCODE -ne 0) { throw "git push failed" }
git push origin "v$Version"
if ($LASTEXITCODE -ne 0) { throw "pushing the tag failed" }
$body = if ($Notes) { $Notes } else { "AIYT Studio v$Version" }
$body += "`n`n**Install:** download the setup exe below (or the portable exe). Needs FFmpeg: ``winget install Gyan.FFmpeg``. Windows may warn that the app is unsigned: More info → Run anyway."
gh release create "v$Version" $installer.FullName $portable --repo $Repo --title "AIYT Studio v$Version" --notes $body
if ($LASTEXITCODE -ne 0) { throw "GitHub release failed (code and tag are pushed; rerun: gh release create v$Version ...)" }

# 4. Optionally refresh your installed copy (never while it is open).
if ($UpdateLocal) {
  $dest = Join-Path $env:LOCALAPPDATA "AIYT Studio\AIYT Studio.exe"
  $running = Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $dest }
  if ($running) { Write-Host "Installed copy is open: close AIYT Studio and rerun with -UpdateLocal (or copy $portable over it)." }
  else { Copy-Item "target-dist\release\aiyt.exe" $dest -Force; Write-Host "Updated $dest" }
}
Write-Host "Published v$Version → https://github.com/$Repo/releases/tag/v$Version"
