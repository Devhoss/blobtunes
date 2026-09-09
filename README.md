# Wavesurf

Audio-only YouTube player for Windows. Paste a link (or search), hear music —
no browser, no video, nothing written to disk.

![MIT](https://img.shields.io/badge/license-MIT-green)

## What it is

Tauri v2 + React shell around a Rust-owned **libmpv** instance. YouTube
resolution comes from the user's own **yt-dlp** (never bundled); live HLS
plays natively; VOD uses a two-tier URL strategy (best dash first, android
progressive fallback on failure). See the rev-2 plan for the full story,
including the range-gated-edge findings that shaped this design.

## Prerequisites

```powershell
winget install yt-dlp.yt-dlp
```

> If playback breaks after working before, update yt-dlp first:
> `winget upgrade yt-dlp.yt-dlp`. YouTube changes their side regularly;
> yt-dlp absorbs it within days.

Search inside the app is optional and needs a **YouTube Data API v3 key**
([create one here](https://console.cloud.google.com/apis/library/youtube.googleapis.com),
paste it in Wavesurf Settings). Without a key, paste-URL mode works fully.
Search costs ~100 quota units per query; the app debounces (400ms) and caches
(10 min) to stay well inside the free 10,000/day.

## Run / build

```bash
cd E:\dev\wavesurf
npm run tauri dev      # hot-reload dev
npm test               # vitest (frontend)
cd src-tauri && cargo test          # rust units
cargo test -- --ignored             # + live network tests (yt-dlp, API key for search)
npm run tauri build    # installer (needs src-tauri/bin/libmpv-2.dll — see tools/)
```

First-time setup on a fresh machine:

```powershell
powershell -ExecutionPolicy Bypass -File tools/fetch-mpv.ps1  # libmpv artifacts
bash tools/copy-dll.sh                                        # stage libmpv-2.dll
```

## Resource use

Measured on this machine (Windows 11, idle desktop):

| State | Wavesurf (MB) | Browser YouTube tab (MB) |
|---|---|---|
| idle, nothing playing | ~40 (main process; + shared WebView2 runtime) | — |
| VOD playing | _tbd_ | _tbd_ |
| live playing | _tbd_ | — |

Target claim only: substantially lower than a browser tab rendering video.
Fill this table in during release testing (`Task 10`).

## Known limits

- No private / age-gated / region-blocked videos anonymously (clear error, no crash).
- First play resolves in ~6–8s for VOD (one yt-dlp dump; the progressive
  fallback resolves only if the primary URL fails) and ~15–20s for live
  (default + android dumps — see below).
- **Live streams with server-side ad inserts stutter.** Some YouTube live
  broadcasts (notably 24/7 music streams) splice ad segments into the HLS
  timeline without discontinuity markers. ffmpeg's demuxer cannot step past
  these splices: audio stops with no error while the player still shows
  "playing". Wavesurf detects the frozen position and reloads a fresh
  playlist automatically (bounded: 5 attempts, then it surfaces an error or
  advances the queue), so playback resumes — but during heavy ad periods you
  will hear gaps. This is a limitation of the HLS/ffmpeg stack, not a bug in
  Wavesurf's control layer (verified: a minimal libmpv harness stalls
  identically on the same URLs). VOD playback is unaffected.
- Search needs an API key; quota is real (~100 searches/day free).
- Tray Quit is the only full exit; window ✕ hides to tray by design.

## License

MIT — see [LICENSE](LICENSE).
