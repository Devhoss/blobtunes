# Blobtunes

<p align="center">
  <img src="assets/blob.svg" alt="The Blobtunes mascot" width="220">
</p>

Audio-only YouTube player for Windows. Paste a link (or search), hear music —
no browser, no video, nothing written to disk.

![MIT](https://img.shields.io/badge/license-MIT-green)
![mascot: CC BY-NC-ND](https://img.shields.io/badge/mascot-CC%20BY--NC--ND%204.0-blue)

> The blob is the app's face and stays that way: the **code is MIT**, but the
> **mascot character is not** — use, run and share the app freely; don't sell
> or remix the character itself. See [BLOB-LICENSE.txt](BLOB-LICENSE.txt).

## What it is

Tauri v2 + React shell around a Rust-owned **libmpv** instance. YouTube
resolution comes from the user's own **yt-dlp** (never bundled); live HLS
plays natively; VOD uses a two-tier URL strategy (best dash first, android
progressive fallback on failure). The playback story lives in the Rust module
docs (`src-tauri/src/*.rs`), including the range-gated-edge findings that
shaped this design.

## The player

The UI is a port of the **Blobtunes** design: one 390x600 card (the window
itself, decorations off) with three views — **NOW** (the blob), **RESULTS**
and **QUEUE** — the blob shrinking into a dense header when a list is open.
Every track tints the whole card with its own hue (FNV-1a of the video id),
and mini mode shrinks the real window to 390x206.

The design artifact is the spec, and a harness keeps the port honest:

```bash
npm run dev                       # app on :1420
node tools/visual-check.cjs       # artifact vs app, computed-style diff
```

`tools/visual-check.cjs` renders the artifact and the app side by side in
Chromium, compares every element the design pins down, and exercises the
centring/scroll/fit invariants that regress silently. `tools/window-probe.ps1`
reads the *real* window's client rect (DPI-aware, so 125% displays don't lie).
The artifact itself lives outside the repo (gitignored) — point
`BLOBTUNES_BASELINE` at it if you move it.

## Prerequisites

```powershell
winget install yt-dlp.yt-dlp
```

> If playback breaks after working before, update yt-dlp first:
> `winget upgrade yt-dlp.yt-dlp`. YouTube changes their side regularly;
> yt-dlp absorbs it within days.

Search inside the app is optional and needs a **YouTube Data API v3 key**
([create one here](https://console.cloud.google.com/apis/library/youtube.googleapis.com),
paste it in Blobtunes Settings). Without a key, paste-URL mode works fully.
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

| State | Blobtunes (MB) | Browser YouTube tab (MB) |
|---|---|---|
| idle, nothing playing | ~40 (main process; + shared WebView2 runtime) | — |
| VOD playing | _tbd_ | _tbd_ |
| live playing | _tbd_ | — |

Target claim only: substantially lower than a browser tab rendering video.
Fill this table in during release testing.

## Windows media session (SMTC)

The player publishes a real **System Media Transport Controls** session, so
Windows' own media flyout, the hardware media keys and shell media widgets
(Venu's Media slide included) can see what is playing. WebView2 cannot do this
on its own: Chromium's `navigator.mediaSession` never reaches WinRT from a
WebView2 host.

- Implementation: `src-tauri/src/smtc.rs` — a thin bridge over
  [`playwire`](https://crates.io/crates/playwire) (SMTC for Windows behind one
  cross-platform API). One extra thread blocked on a channel; no polling.
- Published: title, channel (artist), source ("YouTube"/"Live"), the existing
  YouTube thumbnail (handed to Windows as a URL that IT fetches and caches),
  duration, position, playing/paused state, and play/pause/stop/⏮/⏭/seek. The
  session is released while nothing is playing, so no stale entry lingers  in Windows or Venu when Blobtunes is stopped or idle.
- Cadence: metadata and transport state go out the moment they change; position
  is republished every ~2s while playing (Windows interpolates in between) and
  immediately on any seek. Unchanged state publishes nothing.
- Commands from media keys/Windows come back through the same command channel
  the UI uses; ⏮/⏭ are the queue's own actions and stay disabled when the queue
  has no target. Nothing is faked for live streams (no duration, no seek).
- Failure is never fatal: every SMTC error is logged to `player-debug.log` and
  playback carries on without a session.

## Known limits

- No private / age-gated / region-blocked videos anonymously (clear error, no crash).
- First play resolves in ~6–8s for VOD (one yt-dlp dump; the progressive
  fallback resolves only if the primary URL fails) and ~15–20s for live
  (default + android dumps — see below).
- **Live streams with server-side ad inserts stutter.** Some YouTube live
  broadcasts (notably 24/7 music streams) splice ad segments into the HLS
  timeline without discontinuity markers. ffmpeg's demuxer cannot step past
  these splices: audio stops with no error while the player still shows
  "playing". Blobtunes detects the frozen position and reloads a fresh
  playlist automatically (bounded: 5 attempts, then it surfaces an error or
  advances the queue), so playback resumes — but during heavy ad periods you
  will hear gaps. This is a limitation of the HLS/ffmpeg stack, not a bug in
  Blobtunes's control layer (verified: a minimal libmpv harness stalls
  identically on the same URLs). VOD playback is unaffected.
- Search needs an API key; quota is real (~100 searches/day free).
- Settings → *Media-key fallback (SMTC shim)* is obsolete: the player now
  publishes a native Windows media session (see above), so leave the shim off.
- Search titles arrive as clean UTF-8 (a byte-vs-character bug used to mangle
  emoji and non-ASCII titles; fixed with a regression test).
- Tray Quit is the only full exit; window ✕ hides to tray by design.
- The app identifier (`com.hoss.wavesurf`) and the `wavesurf.json` key file
  are intentionally not renamed: they key the saved API key and the WebView2
  profile, and renaming them would silently drop both.
- Debug thread names and log paths still say `wavesurf` (threads in
  `player.rs`/`smtc.rs`, `E:/dev/wavesurf/*.log`): cosmetic or
  machine-local, deliberately left alone.

## License

Code: MIT — see [LICENSE](LICENSE).
The mascot character: [CC BY-NC-ND 4.0](BLOB-LICENSE.txt) — free to use and
share as part of the app, not to sell or modify on its own.
