use anyhow::{anyhow, Result};
use libmpv2::{events::Event, events::PropertyData, mpv_end_file_reason, Format, Mpv};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use tauri::{AppHandle, Emitter};

use crate::smtc::{CommandSink, MediaCommand, NowPlaying, Smtc};
use crate::ytdlp::TrackMeta;

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct PlayerState {
    pub playing: bool, // a source is loaded
    pub paused: bool,
    pub loading: bool,         // resolving/buffering (StartFile->FileLoaded)
    pub position: Option<f64>, // seconds; None until known
    pub duration: Option<f64>, // None = unknown or LIVE (never guess)
    pub seekable: bool,
    pub volume: u8, // synced to mpv volume; default 80 so slider doesn't jump to 0
    pub title: Option<String>,
    pub ended: bool, // one-shot: natural EOF consumed by frontend
    pub error: Option<String>,
}

/// What the frontend knows about the item that is playing right now, for the OS
/// media session (Windows SMTC) only: the queue owns title/channel/artwork, and
/// it is the only side that knows whether a next/previous item exists.
///
/// WHY from the frontend: it is correct the instant a track is selected, while
/// the yt-dlp resolve that fills the same fields in Rust takes ~6-8s — without
/// this the OS widget (and Venu) would show an unnamed session for those
/// seconds. Display only: nothing here can change what or how mpv plays, and a
/// missing hint just means the resolved metadata is used instead.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct MediaHint {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub artwork: Option<String>,
    #[serde(default)]
    pub is_live: bool,
    #[serde(default)]
    pub can_next: bool,
    #[serde(default)]
    pub can_prev: bool,
}

#[derive(Debug)]
pub enum PlayerCmd {
    Load(String), // any public YouTube URL; resolved off-thread, then played
    /// Internal: resolver thread → owner thread. Carries the load
    /// generation so a stale resolve (superseded by a newer Load) can be
    /// discarded instead of clobbering whatever is currently loading.
    /// NEVER sent from the UI.
    LoadResolved(u64, crate::ytdlp::ResolvedTrack),
    /// Internal: resolver thread → owner thread on yt-dlp failure. Tagged
    /// with the same generation, for the same reason.
    ResolveFailed(u64, String),
    Play,
    Pause,
    Stop,
    Seek(f64),     // absolute seconds (VOD only; mpv rejects on live)
    SetVolume(u8), // 0-100
    /// Frontend -> core display metadata for the OS media session (Windows
    /// SMTC). Never affects playback: the OS side is a pure mirror of state.
    SmtcMeta(MediaHint),
    Shutdown,      // terminate the owner thread; dropping Mpv on its own thread
}

pub struct Player {
    tx: Sender<PlayerCmd>,
    pub state: Arc<Mutex<PlayerState>>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Player {
    pub fn new(app: AppHandle) -> Result<Self> {
        let (tx, rx) = channel::<PlayerCmd>();
        let state = Arc::new(Mutex::new(PlayerState {
            volume: 80,
            ..PlayerState::default()
        }));
        let st = Arc::clone(&state);
        let tx2 = tx.clone();
        // The window the Windows media session attaches to (SMTC needs an
        // HWND). Resolved here, on the setup thread, because the player thread
        // must not touch the window; `None` simply means "no media session".
        let hwnd = main_window_hwnd(&app);
        let handle = thread::Builder::new()
            .name("blobtunes-mpv".into())
            .spawn(move || run_core(app, tx2, rx, st, hwnd))
            .map_err(|e| anyhow!("spawn player thread: {e}"))?;
        Ok(Self {
            tx,
            state,
            handle: Mutex::new(Some(handle)),
        })
    }
    pub fn send(&self, cmd: PlayerCmd) -> Result<()> {
        self.tx
            .send(cmd)
            .map_err(|_| anyhow!("player core is down"))
    }
    /// Graceful teardown: ask the owner thread to exit (dropping Mpv there),
    /// then join it. Called from the tray Quit path before `app.exit(0)`.
    pub fn shutdown(&self) {
        let _ = self.tx.send(PlayerCmd::Shutdown);
        if let Some(h) = self.handle.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

/// The main window's native handle, for the Windows media session (SMTC
/// attaches to a window: `ISystemMediaTransportControlsInterop::GetForWindow`).
/// `None` just means no media session is created — never an error.
#[cfg(windows)]
fn main_window_hwnd(app: &AppHandle) -> Option<u64> {
    use tauri::Manager;
    let hwnd = app.get_webview_window("main")?.hwnd().ok()?;
    // HWND is an opaque pointer wrapper; SMTC wants it as an integer.
    Some(hwnd.0 as usize as u64)
}

#[cfg(not(windows))]
fn main_window_hwnd(_app: &AppHandle) -> Option<u64> {
    None
}

/// Where OS media commands land (hardware media keys, the Windows media flyout,
/// Venu). Transport commands re-enter the player's own command channel — the
/// same one every UI button uses — and skips are forwarded to the queue owner
/// in the frontend, whose ⏮/⏭ actions are the single source of truth. There is
/// no second command system and no duplicated transport logic.
struct SmtcCommands {
    /// `std::sync::mpsc::Sender` is `Send` but not `Sync`, and the WinRT event
    /// handler must be both. Media keys arrive at human speed, so this lock is
    /// effectively uncontended; nothing else ever touches it.
    tx: Mutex<Sender<PlayerCmd>>,
    app: AppHandle,
}

impl CommandSink for SmtcCommands {
    fn dispatch(&self, cmd: MediaCommand) {
        let to_player = |cmd: PlayerCmd| {
            if let Ok(tx) = self.tx.lock() {
                let _ = tx.send(cmd);
            }
        };
        match cmd {
            MediaCommand::Play => to_player(PlayerCmd::Play),
            MediaCommand::Pause => to_player(PlayerCmd::Pause),
            MediaCommand::Stop => to_player(PlayerCmd::Stop),
            MediaCommand::SeekTo(secs) => to_player(PlayerCmd::Seek(secs)),
            MediaCommand::Next => {
                let _ = self.app.emit("player://media-command", "next");
            }
            MediaCommand::Previous => {
                let _ = self.app.emit("player://media-command", "prev");
            }
        }
    }
}

/// Map the player's state + the display metadata onto one Windows media-session
/// snapshot. Borrowed throughout, so a call costs a handful of comparisons and
/// no allocation unless [`Smtc::publish`] decides the OS needs telling.
///
/// State mapping (mirrors the app's own transport):
/// - `playing && !paused` -> SMTC Playing;
/// - loaded but paused -> SMTC Paused;
/// - nothing loaded (never loaded / user Stop / terminal error) -> SMTC Stopped
///   with no metadata at all;
/// - natural EOF keeps the finished track (SMTC Paused) for as long as the
///   frontend keeps it selected: the app's own UI leaves that same track on
///   screen with a working ▶ (post-EOF reload), and a Stopped session would
///   grey the OS play button out and make it un-replayable from the OS side.
fn now_playing<'a>(
    st: &'a PlayerState,
    hint: Option<&'a MediaHint>,
    meta: Option<&'a TrackMeta>,
    is_live: bool,
) -> NowPlaying<'a> {
    // Resolved (yt-dlp) metadata wins where it exists — it is authoritative for
    // what is actually playing — then the frontend's hint.
    let first = |meta: Option<&'a str>, hint: Option<&'a str>, fallback: &'a str| -> &'a str {
        [meta, hint]
            .into_iter()
            .flatten()
            .find(|s| !s.is_empty())
            .unwrap_or(fallback)
    };
    let meta_title = meta.map(|m| m.title.as_str());
    let hint_title = hint.and_then(|h| h.title.as_deref());
    let title = first(meta_title, hint_title, "Blobtunes");
    // Stable per-track id: the YouTube video id when known, else the title, so
    // the OS can tell "new track" from "metadata republished".
    let track_id = first(
        meta.map(|m| m.id.as_str()).filter(|id| !id.is_empty()),
        hint_title,
        title,
    );
    let artist = first(
        meta.map(|m| m.channel.as_str()),
        hint.and_then(|h| h.channel.as_deref()),
        "Blobtunes",
    );
    let artwork = meta
        .and_then(|m| m.thumbnail.as_deref())
        .or_else(|| hint.and_then(|h| h.artwork.as_deref()))
        .filter(|a| !a.is_empty())
        .unwrap_or("");
    let live = is_live || meta.map(|m| m.is_live).unwrap_or(false);
    NowPlaying {
        track_id,
        title,
        artist,
        album: if live { "Live" } else { "YouTube" },
        artwork_url: artwork,
        // Loaded — or the track that just finished, which is still selected and
        // replayable (see the mapping note above).
        has_track: st.playing || st.ended,
        playing: st.playing && !st.paused && !st.ended,
        position_secs: st.position,
        // `st.duration` is already classified: live HLS reports the sliding
        // window, not a length, and the player turns that into None.
        duration_secs: st.duration,
        can_seek: st.seekable,
        // The queue decides: a real next/previous item, reported by the
        // frontend. No hint (nothing selected) -> leave both disabled rather
        // than inventing a target.
        can_next: hint.map(|h| h.can_next).unwrap_or(false),
        can_prev: hint.map(|h| h.can_prev).unwrap_or(false),
    }
}

fn build_mpv() -> Result<Mpv> {
    let mpv = Mpv::with_initializer(|init| {
        init.set_option("vo", "null")?; // never create a video output
        init.set_option("video", "no")?; // never decode a video track
        init.set_option("audio-display", "no")?;
        // Loads are ALWAYS pre-resolved direct URLs (ytdlp::resolve_stream), so
        // the ytdl hook has nothing useful to do. Left on, it actively hurts:
        // when a loadfile URL fails, the hook runs yt-dlp on that googlevideo
        // URL as a [generic] extractor — a ~13s doomed round trip (it 403s)
        // before our own recovery even gets a chance, plus another spawned
        // process on the failure path.
        init.set_option("ytdl", "no")?;
        // Harmless Chrome UA for any direct googlevideo fetch mpv still does
        // (e.g. HLS segments). NOTE: the VOD 403s were NOT a UA problem —
        // default-client (ANDROID_VR) googlevideo edges reject open-ended
        // Range requests outright, and lavf only ever issues open-ended
        // ranges; the fix is resolving the android-client progressive URL as
        // PRIMARY (see ytdlp::resolve_stream), not headers.
        init.set_option("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")?;
        // Live HLS robustness: ffmpeg's lavf demuxer auto-reconnects on
        // transient segment/playlist failures so live audio doesn't cut
        // out minutes in. Generous readahead buffer absorbs network jitter.
        // NOTE: no reconnect-at-eof — a live HLS playlist's sliding window
        // ALWAYS looks like EOF at its current edge; reconnect-at-eof would
        // restart the fetch, hit the edge again, and loop forever ("keeps loading").
        // IMPORTANT: this only retries the *same* URL through transient
        // network blips. It does NOT help once the URL itself stops being
        // valid (e.g. a signed googlevideo URL expiring after a few hours) —
        // that case is handled at the application level below via
        // attempt_recovery()/live re-resolve, not by these options.
        // These FFmpeg AVOptions are not supported by the bundled mpv build;
        // application-level recovery handles expired or stalled HLS URLs.
        init.set_option("demuxer-readahead-secs", "20")?;
        init.set_option("demuxer-max-bytes", "15M")?;
        // Deep-debug: mpv's own internal log (demuxer/segment-level truth —
        // fetch failures, EOF causes, reconnect behavior) goes to a file so
        // the next live-cutoff repro captures what our event log can't see.
        init.set_option("log-file", "E:/dev/blobtunes/mpv-debug.log")?;
        // NOTE on module names: mpv has demux/hls/ffmpeg/stream/ao modules
        // but no `network` or `core` modules (network traffic surfaces under
        // stream/ffmpeg, core state under cplayer) — those two are covered
        // by all=warn, not invented here.
        init.set_option(
            "msg-level",
            "all=warn,demux=debug,hls=debug,ffmpeg=debug,stream=debug,ao=debug",
        )?;
        Ok(())
    })?;
    mpv.enable_all_events()?;
    mpv.observe_property("time-pos", Format::Double, 1)?;
    mpv.observe_property("duration", Format::Double, 2)?;
    mpv.observe_property("pause", Format::Flag, 3)?;
    mpv.observe_property("seekable", Format::Flag, 4)?;
    mpv.observe_property("core-idle", Format::Flag, 5)?;
    Ok(mpv)
}

pub(crate) fn dbg(log: &std::sync::Arc<std::sync::Mutex<std::fs::File>>, msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = log.lock() {
        let _ = writeln!(f, "[{:.3}] {}", elapsed_secs(), msg);
        let _ = f.flush();
    }
}

fn elapsed_secs() -> f64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    start.elapsed().as_secs_f64()
}

/// Lock the shared state, recovering from a poisoned mutex instead of
/// panicking. A single earlier panic while the lock was held used to brick
/// every future state update (and silently, since the old call sites used
/// `if let Ok(...)` and just no-op'd on Err).
fn lock_state(state: &Arc<Mutex<PlayerState>>) -> std::sync::MutexGuard<'_, PlayerState> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Cancel flag for the latest spawned resolve. Every new resolve arms a
/// fresh flag and trips the previous one, so a superseded yt-dlp child is
/// killed in run_dump's wait loop instead of burning unpack+scan+network
/// (~30s here) to a completion nobody will read. Without this, rapid
/// skipping piles up concurrent yt-dlp processes that drag EACH OTHER past
/// the timeout (seen Sep 2026: 35+ gens in ~70s, nearly all stale/failed).
static RESOLVE_CANCEL: OnceLock<Mutex<Option<Arc<AtomicBool>>>> = OnceLock::new();

/// Arm a fresh cancel flag for a new resolve, tripping the previous one.
/// A finished thread's flag is harmless (setting it trips nothing alive).
fn arm_resolver_cancel() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    if let Some(slot) = RESOLVE_CANCEL.get_or_init(|| Mutex::new(None)).lock().ok() {
        let mut guard = slot;
        if let Some(old) = guard.replace(flag.clone()) {
            old.store(true, Ordering::SeqCst);
        }
    }
    flag
}

/// Kick off a yt-dlp resolve on its own thread and route the result back
/// into the owner thread's command channel, tagged with `gen` so a
/// superseded resolve can be told apart from the current one.
fn spawn_live_resolver(tx: &Sender<PlayerCmd>, gen: u64, url: String) {
    let tx2 = tx.clone();
    let cancel = arm_resolver_cancel();
    thread::Builder::new()
        .name("blobtunes-live-resolve".into())
        .spawn(move || {
            let cmd = match crate::ytdlp::resolve_live_hls(&url, &cancel) {
                Ok(t) => PlayerCmd::LoadResolved(gen, t),
                Err(e) => PlayerCmd::ResolveFailed(gen, e.to_string()),
            };
            let _ = tx2.send(cmd);
        })
        .map_err(|e| {
            let _ = tx.send(PlayerCmd::ResolveFailed(
                gen,
                format!("failed to start live resolver: {e}"),
            ));
        })
        .ok();
}

fn spawn_resolver(tx: &Sender<PlayerCmd>, gen: u64, url: String) {
    let tx2 = tx.clone();
    let cancel = arm_resolver_cancel();
    thread::Builder::new()
        .name("blobtunes-resolve".into())
        .spawn(move || {
            let cmd = match crate::ytdlp::resolve_stream(&url, &cancel) {
                Ok(t) => PlayerCmd::LoadResolved(gen, t),
                Err(e) => PlayerCmd::ResolveFailed(gen, e.to_string()),
            };
            let _ = tx2.send(cmd);
        })
        .map_err(|e| {
            let _ = tx.send(PlayerCmd::ResolveFailed(
                gen,
                format!("failed to start resolver: {e}"),
            ));
        })
        .ok();
}

/// (Re)play the current track after EOF. After EndFile(Eof) mpv's core is
/// idle with NO file loaded, so pause is a silent no-op and seek fails with
/// MPV_ERROR_COMMAND ("seek: raw(-12)") — and `current_url` is the original
/// YouTube URL, which a direct loadfile can't play (the ytdl hook is OFF;
/// mpv would try to decode the watch page → "Failed to recognize file
/// format"). So go back through the resolver: the 20-min resolve cache
/// returns the still-valid googlevideo URL instantly, and past the TTL a
/// fresh resolve is exactly what an expired URL needs anyway.
fn reload_current(
    state: &Arc<Mutex<PlayerState>>,
    tx: &Sender<PlayerCmd>,
    current_url: &Option<String>,
    load_gen: &mut u64,
    log: &std::sync::Arc<std::sync::Mutex<std::fs::File>>,
) -> Result<()> {
    let url = current_url
        .clone()
        .ok_or_else(|| anyhow!("nothing loaded to replay"))?;
    dbg(log, "post-EOF reload (re-resolving)");
    *load_gen += 1;
    {
        let mut st = lock_state(state);
        st.ended = false;
        st.loading = true;
        st.playing = true;
        st.error = None;
        st.position = None;
        st.duration = None;
        st.seekable = false;
        st.title = None;
    }
    spawn_resolver(tx, *load_gen, url);
    Ok(())
}

/// Max automatic reconnect attempts for a live stream before giving up and
/// surfacing a real error (guards against spinning forever on a broadcast
/// that has genuinely ended).
const MAX_LIVE_RETRIES: u32 = 3;

/// Attempt automatic recovery after a playback failure. `reason` names the
/// trigger (EndFile tag / wait_event error / watchdog) and is logged with
/// every attempt so the log always shows WHY a recovery happened.
/// - VOD: one reactive android-progressive resolve per track load
///   (`vod_fallback_used`), skipped entirely when the primary already came
///   from the android client (`fallback_exhausted`), then a terminal error.
///   Bounded by construction.
/// - Live: re-resolve + reload, capped at MAX_LIVE_RETRIES.
/// Returns true if a recovery attempt was kicked off (state left in
/// loading/playing), false if the caller should surface a terminal error.
/// Never sets `ended`: recovering means "still trying" — only the terminal
/// paths in the EndFile handler may mark a track ended.
#[allow(clippy::too_many_arguments)]
fn attempt_recovery(
    st: &mut PlayerState,
    tx: &Sender<PlayerCmd>,
    is_live: bool,
    live_retry_count: &mut u32,
    current_url: &Option<String>,
    load_gen: &mut u64,
    log: &std::sync::Arc<std::sync::Mutex<std::fs::File>>,
    vod_fallback_used: &mut bool,
    reason: &str,
) -> bool {
    // VOD: the primary URL failed (e.g. a 403). If the primary was NOT the
    // android progressive URL, resolve that reactively and retry it once —
    // one attempt per track load. When the primary IS the android URL (the
    // normal case; `vod_fallback_used` was set from fallback_exhausted at
    // LoadResolved) there is nothing left to resolve and a second failure
    // surfaces a terminal error.
    if !is_live && !*vod_fallback_used {
        let url = match current_url.clone() {
            Some(u) => u,
            None => return false,
        };
        *vod_fallback_used = true;
        let tx = tx.clone();
        let log = log.clone();
        let reason = reason.to_string();
        let gen_plus = *load_gen + 1;
        *load_gen += 1;
        let inner = move || {
            dbg(
                &log,
                &format!("VOD tier-2 fallback reactively resolving (trigger: {reason})"),
            );
            let fallback = match crate::ytdlp::resolve_fallback_url(
                &url,
                // Owner-thread reactive fallback: synchronous by design, so it
                // carries a never-tripped flag and keeps its 90s budget.
                &AtomicBool::new(false),
            ) {
                Ok(Some(u)) => u,
                Ok(None) => {
                    dbg(&log, "VOD tier-2 fallback: no progressive format found");
                    return;
                }
                Err(e) => {
                    dbg(&log, &format!("VOD tier-2 fallback resolve failed: {e}"));
                    return;
                }
            };
            let cmd = PlayerCmd::LoadResolved(
                gen_plus,
                crate::ytdlp::ResolvedTrack {
                    meta: crate::ytdlp::TrackMeta {
                        id: String::new(),
                        title: String::new(),
                        channel: String::new(),
                        duration: None,
                        is_live: false,
                        thumbnail: None,
                    },
                    primary_url: fallback.clone(),
                    is_hls: false,
                    fallback_exhausted: true,
                },
            );
            let _ = tx.send(cmd);
        };
        thread::Builder::new()
            .name("blobtunes-fallback".into())
            .spawn(inner)
            .ok();
        return true;
    }
    // Live track with no pre-armed fallback — re-resolve the original URL
    // and reconnect (the existing live-reconnect path).
    if is_live && *live_retry_count < MAX_LIVE_RETRIES {
        if let Some(url) = current_url.clone() {
            *live_retry_count += 1;
            dbg(
                log,
                &format!(
                    "live reconnect attempt {}/{MAX_LIVE_RETRIES} (trigger: {reason})",
                    *live_retry_count
                ),
            );
            st.error = None;
            st.loading = true;
            st.playing = true;
            *load_gen += 1;
            spawn_live_resolver(tx, *load_gen, url);
            return true;
        }
    }
    false
}

/// Decide the duration to show the UI. A live HLS stream reports a small,
/// growing "duration" that's really just its currently-known sliding
/// window — never its true (unbounded) length — and never becomes
/// seekable. `seekable` alone is enough to tell the two apart: a VOD
/// reports seekable essentially as soon as it opens, so this costs it at
/// most one property-change tick before its real duration shows up.
fn classify_duration(seekable: bool, raw: Option<f64>) -> Option<f64> {
    if seekable {
        raw
    } else {
        None
    }
}

fn run_core(
    app: AppHandle,
    tx: Sender<PlayerCmd>,
    rx: Receiver<PlayerCmd>,
    state: Arc<Mutex<PlayerState>>,
    hwnd: Option<u64>,
) {
    // Prefer the usual dev-machine path, but never let a missing drive/dir
    // panic this thread (that used to brick the whole player permanently
    // with no supervision to restart it). Fall back to the OS temp dir, and
    // to no logging at all rather than crashing if even that fails.
    let log_file = std::sync::Arc::new(std::sync::Mutex::new(
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("E:/dev/blobtunes/player-debug.log")
        {
            Ok(f) => f,
            Err(_) => match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(std::env::temp_dir().join("blobtunes-player-debug.log"))
            {
                Ok(f) => f,
                Err(_) => return,
            },
        },
    ));
    let log = log_file.clone();
    dbg(&log, "run_core start");
    let mpv = match build_mpv() {
        Ok(m) => m,
        Err(e) => {
            if let Ok(mut st) = state.lock() {
                st.error = Some(format!("mpv init failed: {e}"));
            }
            dbg(&log, &format!("mpv init failed: {e}"));
            let _ = app.emit(
                "player://state",
                state.lock().map(|s| s.clone()).unwrap_or_default(),
            );
            return;
        }
    };
    let mut last: Option<PlayerState> = None;
    // Windows media session (SMTC): a pure side channel that mirrors what this
    // thread already knows, so Windows' media flyout, the hardware media keys
    // and Venu can see it. Started here — not in setup — so OS commands come
    // back on THIS command channel, the same one every UI button uses; there is
    // no second control path. `None` when there is no window handle (no window
    // to attach the session to, or not Windows at all), in which case nothing is
    // published and playback is entirely unaffected.
    let smtc = hwnd.map(|hwnd| {
        dbg(&log, &format!("smtc: starting bridge for hwnd {hwnd:#x}"));
        Smtc::start(
            hwnd,
            Arc::new(SmtcCommands {
                tx: Mutex::new(tx.clone()),
                app: app.clone(),
            }),
            Arc::clone(&log),
        )
    });
    // Display metadata for that session: what the frontend queued (title,
    // channel, artwork + whether ⏮/⏭ exist) and what the yt-dlp resolve
    // confirmed. Display only — never read by anything that drives playback.
    let mut media_hint: Option<MediaHint> = None;
    let mut resolved_meta: Option<TrackMeta> = None;
    // Original URL of the currently loaded track, kept so a live stream can
    // be re-resolved and reloaded on failure, and so a post-EOF Play/Seek can
    // reload the same track.
    let mut current_url: Option<String> = None;
    // Whether the current track is a live HLS stream (gates auto-reconnect).
    let mut is_live: bool = false;
    // Consecutive auto-reconnect attempts for the current live track.
    let mut live_retry_count: u32 = 0;
    // Monotonic counter tagging each resolve request; lets us discard a
    // resolve result that's been superseded by a newer Load/reconnect.
    let mut load_gen: u64 = 0;
    // Raw `duration` value last reported by mpv (filtered to >0.5, unfiltered
    // by our live/VOD classification of it). Cached so a `seekable` update
    // that arrives after `duration` can re-run classify_duration() without
    // waiting on another `duration` change that may not come for a while.
    let mut last_raw_duration: Option<f64> = None;
    // Heartbeat: proves the owner thread is alive and shows mpv's state even
    // when no events arrive — a silent stall and a dead thread look identical
    // without this.
    let mut last_beat = std::time::Instant::now();
    // Frozen-buffering watchdog (live only): mpv can sit in "buffering @ 0%"
    // forever when ffmpeg's HLS demuxer stalls on a timestamp splice (e.g.
    // a server-side ad insert with no DISCONTINUITY tag) — no EndFile, no
    // error, just core-idle with a frozen position. Track advancement here
    // so the heartbeat can tell "buffering but progressing" apart from dead.
    let mut core_idle = false;
    let mut last_advance_pos = 0.0;
    let mut last_advance_t: Option<std::time::Instant> = None;
    // When the last (re)load recovery kicked off — grace period so a fresh
    // stream gets time to fill its buffer before the watchdog may fire.
    let mut last_recovery: Option<std::time::Instant> = None;
    // VOD reactive-fallback guard: exactly one android-progressive resolve
    // per track load (reset on Load/Stop, set from `fallback_exhausted` on
    // LoadResolved). Without this, a track whose fallback URL also fails
    // would re-resolve forever.
    let mut vod_fallback_used = false;
    // Seek requested while the player was post-EOF (no file loaded). Applied
    // as a plain seek once the reload's FileLoaded arrives.
    let mut pending_seek: Option<f64> = None;
    // True between a post-EOF Play/Seek reload and its FileLoaded. In that
    // window there is no file to seek, so drag events (which arrive as a
    // STREAM of Seek commands) must retarget pending_seek instead of hitting
    // mpv — each would fail with raw(-12) and the first (stale, end-of-track)
    // value would otherwise be the only one that ever applied.
    let mut reload_in_flight = false;

    loop {
        // 1) drain pending commands (non-blocking)
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, PlayerCmd::Shutdown) {
                dbg(&log, "Shutdown received");
                // Release the OS media session first: Windows (and Venu) must
                // not keep a ghost "Blobtunes is playing" entry after the app is
                // gone. detach() joins the SMTC thread; the Drop below is then a
                // no-op.
                if let Some(smtc) = smtc.as_ref() {
                    smtc.detach();
                }
                mpv.command("stop", &[]).ok(); // release audio promptly
                return; // drops Mpv on THIS thread, then the thread exits; Player::shutdown joins it
            }
            let r: Result<()> = match &cmd {
                // Resolve off-thread: yt-dlp takes ~5-15s and must never stall
                // this event loop (state pushes + playback events would freeze).
                PlayerCmd::Load(url) => {
                    dbg(&log, &format!("Load: {}", &url[..url.len().min(60)]));
                    load_gen += 1;
                    let my_gen = load_gen;
                    current_url = Some(url.clone());
                    is_live = false;
                    live_retry_count = 0;
                    vod_fallback_used = false; // fresh track, fresh fallback budget
                    pending_seek = None; // a queued post-EOF seek is moot now
                    reload_in_flight = false;
                    last_raw_duration = None;
                    // The OS session shows the new item's metadata (from the
                    // frontend hint) until the resolve below confirms it.
                    resolved_meta = None;
                    {
                        let mut st = lock_state(&state);
                        st.loading = true;
                        st.playing = true;
                        st.error = None;
                        st.ended = false;
                        st.position = None;
                        st.duration = None;
                        st.seekable = false; // stale from a previous track otherwise
                        st.title = None;
                    }
                    spawn_resolver(&tx, my_gen, url.clone());
                    Ok(())
                }
                PlayerCmd::ResolveFailed(gen, e) => {
                    if *gen != load_gen {
                        dbg(
                            &log,
                            &format!("stale ResolveFailed (gen {gen}) ignored: {e}"),
                        );
                    } else {
                        dbg(&log, &format!("ResolveFailed: {e}"));
                        let mut st = lock_state(&state);
                        if is_live && live_retry_count < MAX_LIVE_RETRIES {
                            if let Some(u) = current_url.clone() {
                                live_retry_count += 1;
                                dbg(
                                    &log,
                                    &format!(
                                        "live reconnect (re-resolve) attempt {live_retry_count}/{MAX_LIVE_RETRIES}"
                                    ),
                                );
                                st.error = None;
                                st.loading = true;
                                st.playing = true;
                                load_gen += 1;
                                drop(st);
                                spawn_resolver(&tx, load_gen, u);
                            } else {
                                st.loading = false;
                                st.playing = false;
                                st.error = Some(e.to_string());
                            }
                        } else {
                            st.loading = false;
                            st.playing = false;
                            st.error = Some(e.to_string());
                        }
                    }
                    Ok(())
                }
                PlayerCmd::LoadResolved(gen, t) => {
                    if *gen != load_gen {
                        dbg(
                            &log,
                            &format!("stale LoadResolved (gen {gen}) ignored: {}", t.meta.title),
                        );
                        Ok(())
                    } else {
                        dbg(
                            &log,
                            &format!(
                                "LoadResolved: live={} hls={} title={}",
                                t.meta.is_live, t.is_hls, t.meta.title
                            ),
                        );
                        is_live = t.is_hls;
                        // Record whether a reactive android re-resolve could
                        // ever help this track: it can't when the primary
                        // already came from the android client (the normal
                        // VOD case) or after a reactive resolve. Reactive VOD
                        // fallback resolves carry an empty meta (URL-only
                        // recovery) — don't clobber the real title.
                        vod_fallback_used = t.fallback_exhausted;
                        if !t.meta.title.is_empty() {
                            let mut st = lock_state(&state);
                            st.title = Some(t.meta.title.clone());
                            // Resolved metadata is authoritative for the OS
                            // media session (real channel + thumbnail). Reactive
                            // VOD-fallback resolves carry an empty meta and must
                            // NOT clobber what the frontend already handed us.
                            resolved_meta = Some(t.meta.clone());
                        }
                        // mpv's pause is sticky across loadfile: a pause left
                        // over from an earlier track would start this one
                        // silent while state says playing. Every load is an
                        // explicit play intent — clear it first (log-only:
                        // the loadfile tail below owns this arm's Result).
                        if let Err(e) = mpv.set_property("pause", false) {
                            dbg(&log, &format!("unpause failed: {e}"));
                        }
                        mpv.command("loadfile", &[t.primary_url.as_str(), "replace"])
                            .map_err(|e| anyhow!("loadfile: {e}"))
                    }
                }
                PlayerCmd::Play => {
                    dbg(&log, "Play");
                    // Post-EOF the core is idle with no file loaded — unpausing
                    // is a silent no-op and the UI would stay stuck on "ended".
                    // Reload the track instead (VOD only: `ended` is only ever
                    // set on the VOD EOF path).
                    let ended = lock_state(&state).ended;
                    if ended {
                        let r = reload_current(&state, &tx, &current_url, &mut load_gen, &log);
                        if r.is_ok() {
                            reload_in_flight = true;
                        }
                        r
                    } else {
                        mpv.set_property("pause", false)
                            .map_err(|e| anyhow!("play: {e}"))
                    }
                }
                PlayerCmd::Pause => {
                    dbg(&log, "Pause");
                    mpv.set_property("pause", true)
                        .map_err(|e| anyhow!("pause: {e}"))
                }
                PlayerCmd::Stop => {
                    dbg(&log, "Stop");
                    // Prevent a stray post-Stop EndFile(Error) from
                    // triggering an unwanted reconnect on a track the user
                    // explicitly stopped.
                    is_live = false;
                    current_url = None;
                    live_retry_count = 0;
                    vod_fallback_used = false;
                    pending_seek = None;
                    reload_in_flight = false;
                    resolved_meta = None; // nothing playing: no session metadata
                    mpv.command("stop", &[]).map_err(|e| anyhow!("stop: {e}"))
                }
                PlayerCmd::Seek(s) => {
                    dbg(&log, &format!("Seek: {s}"));
                    // Post-EOF there is no file loaded — a seek would fail
                    // with MPV_ERROR_COMMAND ("seek: raw(-12)"). Reload the
                    // track and jump to the requested position once it loads.
                    let ended = lock_state(&state).ended;
                    if ended {
                        pending_seek = Some(*s);
                        let r = reload_current(&state, &tx, &current_url, &mut load_gen, &log);
                        if r.is_ok() {
                            reload_in_flight = true;
                        }
                        r
                    } else if reload_in_flight {
                        // Reload in flight, file not loaded yet: a plain seek
                        // has nothing to act on. A drag arrives as a stream of
                        // Seek commands — keep only the freshest target.
                        pending_seek = Some(*s);
                        Ok(())
                    } else {
                        mpv.command("seek", &[&s.to_string(), "absolute"])
                            .map_err(|e| anyhow!("seek: {e}"))
                    }
                }
                PlayerCmd::SetVolume(v) => {
                    dbg(&log, &format!("SetVolume: {v}"));
                    {
                        let mut st = lock_state(&state);
                        st.volume = *v;
                    }
                    mpv.set_property("volume", i64::from(*v))
                        .map_err(|e| anyhow!("volume: {e}"))
                }
                // Display metadata for the OS media session (see MediaHint).
                // Logged so the OS side can be diagnosed from the player log.
                PlayerCmd::SmtcMeta(hint) => {
                    dbg(
                        &log,
                        &format!(
                            "SmtcMeta: title={:?} channel={:?} artwork={} next={} prev={}",
                            hint.title.as_deref().unwrap_or(""),
                            hint.channel.as_deref().unwrap_or(""),
                            hint.artwork.is_some(),
                            hint.can_next,
                            hint.can_prev
                        ),
                    );
                    media_hint = Some(hint.clone());
                    Ok(())
                }
                PlayerCmd::Shutdown => Ok(()), // handled above; unreachable here
            };
            if let Err(e) = r {
                dbg(&log, &format!("cmd error: {e}"));
                let mut st = lock_state(&state);
                st.error = Some(e.to_string());
            }
        }

        // 2) process one mpv event (250ms poll keeps command latency low).
        // NOTE: libmpv2 reports a failed EndFile as `Err`, NOT as an Event —
        // filtering on `Some(Ok(..))` drops it and hangs the UI on loading
        // forever. Both arms below must stay in sync.
        if let Some(evt) = mpv.wait_event(0.25) {
            match evt {
                Err(e) => {
                    dbg(&log, &format!("EVENT ERROR: {e}"));
                    let mut st = lock_state(&state);
                    st.playing = false;
                    st.loading = false;
                    if !attempt_recovery(
                        &mut st,
                        &tx,
                        is_live,
                        &mut live_retry_count,
                        &current_url,
                        &mut load_gen,
                        &log,
                        &mut vod_fallback_used,
                        &format!("wait_event error: {e}"),
                    ) {
                        st.error = Some(format!("mpv event error: {e}"));
                    }
                }
                Ok(ev) => {
                    let mut st = lock_state(&state);
                    match ev {
                        Event::StartFile => {
                            dbg(&log, "StartFile");
                            st.loading = true;
                            st.playing = true;
                            st.error = None;
                            st.ended = false;
                            st.position = None;
                            st.duration = None;
                            st.seekable = false; // authoritative reset for every loadfile (fresh/fallback/reconnect)
                            last_raw_duration = None;
                        }
                        Event::FileLoaded => {
                            dbg(&log, "FileLoaded");
                            st.loading = false;
                            st.paused = false;
                            // Recovery confirmation: FileLoaded while retries are
                            // outstanding means a re-resolve/reload actually
                            // brought playback back (vs. erroring out).
                            if live_retry_count > 0 {
                                dbg(
                                    &log,
                                    &format!(
                                        "live recovery confirmed after {} attempt(s)",
                                        live_retry_count
                                    ),
                                );
                            }
                            live_retry_count = 0; // confirmed healthy again
                                                  // Fresh stream: restart the watchdog clock.
                            last_advance_pos = 0.0;
                            last_advance_t = Some(std::time::Instant::now());
                            if let Ok(title) = mpv.get_property::<String>("media-title") {
                                st.title = Some(title);
                            }
                            // A seek that arrived while post-EOF (no file
                            // loaded) — the reload just confirmed; jump now.
                            reload_in_flight = false;
                            if let Some(s) = pending_seek.take() {
                                dbg(&log, &format!("applying pending post-EOF seek: {s}"));
                                if let Err(e) = mpv.command("seek", &[&s.to_string(), "absolute"]) {
                                    dbg(&log, &format!("pending seek failed: {e}"));
                                }
                            }
                        }
                        Event::EndFile(reason) => {
                            let tag = if reason == mpv_end_file_reason::Eof {
                                "EOF"
                            } else if reason == mpv_end_file_reason::Error {
                                "ERROR"
                            } else {
                                "OTHER"
                            };
                            dbg(
                                &log,
                                &format!("EndFile({tag}) pos={:.1}", st.position.unwrap_or(0.0)),
                            );
                            st.playing = false;
                            st.loading = false;
                            if reason == mpv_end_file_reason::Eof && !is_live {
                                // Genuine VOD end.
                                st.ended = true; // frontend advances queue on this
                            } else if reason == mpv_end_file_reason::Eof
                                || reason == mpv_end_file_reason::Error
                            {
                                // A live HLS sliding window can report Eof at its
                                // current edge even though the broadcast is still
                                // going (see the demuxer-lavf-o comment above) —
                                // that used to just stop playback outright, since
                                // only Error triggered a reconnect. Treat a live
                                // Eof the same as an Error: try the VOD tier-2
                                // fallback if one's armed, else re-resolve and
                                // reconnect for live tracks. This does NOT mean
                                // mpv's own reconnect-at-eof is in play (still
                                // off) — each attempt here goes through a full
                                // yt-dlp re-resolve, so it's naturally paced and
                                // can't tight-loop the way that option would.
                                if !attempt_recovery(
                                    &mut st,
                                    &tx,
                                    is_live,
                                    &mut live_retry_count,
                                    &current_url,
                                    &mut load_gen,
                                    &log,
                                    &mut vod_fallback_used,
                                    &format!("EndFile({tag})"),
                                ) {
                                    if is_live {
                                        // An HLS EOF can be a transport or
                                        // demuxer failure, not a completed
                                        // broadcast. Do not advance the queue;
                                        // surface a terminal error instead.
                                        st.error =
                                            Some("live stream ended or could not reconnect".into());
                                        st.ended = false;
                                    } else if reason == mpv_end_file_reason::Eof {
                                        st.ended = true;
                                    } else {
                                        st.error = Some("mpv playback error".into());
                                    }
                                }
                            }
                            // else: Quit/Redirect etc — playback already cleared above.
                        }
                        Event::PropertyChange {
                            change,
                            reply_userdata,
                            ..
                        } => match reply_userdata {
                            1 => {
                                if let PropertyData::Double(v) = change {
                                    st.position = Some(v);
                                    if v > last_advance_pos + 0.5 {
                                        last_advance_pos = v;
                                        last_advance_t = Some(std::time::Instant::now());
                                    }
                                }
                            }
                            2 => {
                                if let PropertyData::Double(v) = change {
                                    // BUG FIX: this used to also require
                                    // `position > 2.0` before trusting `seekable`,
                                    // to give a VOD time to become seekable. That
                                    // left a ~2s window at the start of EVERY
                                    // stream where a live HLS's small, growing
                                    // sliding-window duration (e.g. ~3s) got
                                    // reported as if it were the track's real,
                                    // final duration — anything reacting to
                                    // "position caught up to duration" then cut
                                    // the live stream off right on schedule.
                                    // `seekable` alone is the correct signal: a
                                    // live stream never becomes seekable, full
                                    // stop, no timing heuristic needed.
                                    last_raw_duration = Some(v).filter(|d| *d > 0.5);
                                    let was_none = st.duration.is_none();
                                    st.duration = classify_duration(st.seekable, last_raw_duration);
                                    // log duration transitions (esp. reappearances)
                                    if was_none && st.duration.is_some() {
                                        dbg(
                                            &log,
                                            &format!(
                                                "duration REAPPEARED: {:.1} — flipping to VOD UI",
                                                st.duration.unwrap()
                                            ),
                                        );
                                    }
                                }
                            }
                            3 => {
                                // BUG FIX: this used to only handle Flag(true),
                                // so `st.paused` could never go back to false
                                // via a property-change event (only FileLoaded
                                // reset it) — resuming from pause left the UI
                                // stuck showing "paused" even though mpv was
                                // actually playing again.
                                if let PropertyData::Flag(v) = change {
                                    st.paused = v;
                                }
                            }
                            4 => {
                                if let PropertyData::Flag(v) = change {
                                    st.seekable = v;
                                    // Reclassify using the cached raw duration:
                                    // if `duration` arrived before `seekable` did
                                    // (possible for a VOD that isn't seekable the
                                    // instant it opens), we need to redo the call
                                    // now rather than wait for another `duration`
                                    // change that may never come.
                                    st.duration = classify_duration(st.seekable, last_raw_duration);
                                }
                            }
                            5 => {
                                // core-idle is THE stall signal: store it for
                                // the frozen-buffering watchdog below, and log
                                // it (must stay visible until the cutoff is fixed).
                                if let PropertyData::Flag(v) = change {
                                    core_idle = v;
                                    if v {
                                        let st_pos = st.position.unwrap_or(0.0);
                                        dbg(
                                            &log,
                                            &format!(
                                                "core-idle pos={:.1} dur={:?} paused={}",
                                                st_pos, st.duration, st.paused
                                            ),
                                        );
                                    }
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    } // close match ev
                } // close Ok(ev) arm
            } // close match evt
        }

        // Heartbeat (see decl above): 15s tick with the full playback
        // picture, so "nothing in the log" becomes a diagnosis instead of
        // a mystery — frozen pos = mpv stalled, missing beats = thread dead.
        // Also runs the frozen-buffering watchdog for live tracks (see the
        // decl comment for why EndFile-based recovery alone can't catch this).
        if last_beat.elapsed() > std::time::Duration::from_secs(15) {
            last_beat = std::time::Instant::now();
            let mut st = lock_state(&state);
            // Buffer + audio-output reads are best-effort probes: an unknown
            // property name yields None (visible as such below), never a crash.
            let cache_secs: Option<f64> = mpv.get_property("demuxer-cache-time").ok();
            let cur_ao: Option<String> = mpv.get_property("current-ao").ok();
            // NOTE: this is the file-logging dbg() helper above, NOT the std
            // dbg! macro — the macro variant dumped every heartbeat to stderr.
            dbg(
                &log,
                &format!(
                    "beat pos={:.1} dur={:?} paused={} playing={} loading={} seekable={} idle={} cache={:?} ao={:?}",
                    st.position.unwrap_or(0.0),
                    st.duration,
                    st.paused,
                    st.playing,
                    st.loading,
                    st.seekable,
                    core_idle,
                    cache_secs,
                    cur_ao
                ),
            );
            // Watchdog: live, unpaused, supposedly playing, mpv idle, and no
            // position advancement for 30s+ = the demuxer is wedged (e.g. on
            // an ad-splice discontinuity), not merely buffering. Recover via
            // the same re-resolve/reload path as a hard failure. The 30s +
            // post-reload grace + retry cap keep this from ever tight-looping
            // the way the old fire-on-every-idle logic did.
            let cache_empty = cache_secs.map(|secs| secs <= 0.25).unwrap_or(false);
            // core-idle alone is normal while mpv waits between HLS segments.
            // Only recover when the demuxer also has no buffered data; this
            // prevents short live interruptions from forcing a full reload.
            if is_live
                && !st.paused
                && st.playing
                && st.duration.is_none()
                && core_idle
                && cache_empty
            {
                let now = std::time::Instant::now();
                let frozen_for = last_advance_t.map(|t| now - t);
                let in_grace = last_recovery
                    .map(|t| now - t < std::time::Duration::from_secs(45))
                    .unwrap_or(false);
                if let Some(d) = frozen_for {
                    if d > std::time::Duration::from_secs(30) && !in_grace {
                        if live_retry_count < MAX_LIVE_RETRIES {
                            dbg(
                                &log,
                                &format!("watchdog: live frozen {:.0}s, recovering", d.as_secs()),
                            );
                            last_recovery = Some(now);
                            attempt_recovery(
                                &mut st,
                                &tx,
                                is_live,
                                &mut live_retry_count,
                                &current_url,
                                &mut load_gen,
                                &log,
                                &mut vod_fallback_used,
                                "watchdog: position frozen with core-idle",
                            );
                        } else if st.error.is_none() {
                            st.error = Some("Live stream stalled — no data for a while".into());
                        }
                    }
                }
            }
        }

        // 3) Windows media session (SMTC): offer a snapshot every iteration and
        //    let Smtc decide whether anything has to cross to WinRT — metadata
        //    and state changes go immediately, position every ~2s (see
        //    smtc.rs). Free when there is no session (non-Windows, no window
        //    handle) and a pure mirror: it can never affect playback.
        let snapshot = lock_state(&state).clone();
        if let Some(smtc) = smtc.as_ref() {
            smtc.publish(now_playing(
                &snapshot,
                media_hint.as_ref(),
                resolved_meta.as_ref(),
                is_live,
            ));
        }

        // 4) push state to the UI if it changed
        if last.as_ref() != Some(&snapshot) {
            let _ = app.emit("player://state", &snapshot);
            last = Some(snapshot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ytdlp::TrackMeta;

    #[test]
    fn live_duration_maps_to_none() {
        let duration = 0.0_f64;
        assert_eq!(Some(duration).filter(|d| *d > 0.5), None);
    }

    // --- OS media session mapping (now_playing) ----------------------------

    fn playing() -> PlayerState {
        PlayerState {
            playing: true,
            volume: 80,
            ..PlayerState::default()
        }
    }

    fn resolved() -> TrackMeta {
        TrackMeta {
            id: "vid1".to_string(),
            title: "Resolved title".to_string(),
            channel: "Channel".to_string(),
            duration: Some(200.0),
            is_live: false,
            thumbnail: Some("https://i.ytimg.com/vi/vid1/maxresdefault.jpg".to_string()),
        }
    }

    fn hint() -> MediaHint {
        MediaHint {
            title: Some("Queued title".to_string()),
            channel: Some("Hint channel".to_string()),
            artwork: Some("https://i.ytimg.com/vi/vid1/hqdefault.jpg".to_string()),
            is_live: false,
            can_next: true,
            can_prev: false,
        }
    }

    #[test]
    fn resolved_metadata_wins_over_the_queue_hint() {
        let st = playing();
        let meta = resolved();
        let h = hint();
        let np = now_playing(&st, Some(&h), Some(&meta), false);
        assert_eq!(np.title, "Resolved title");
        assert_eq!(np.artist, "Channel");
        assert_eq!(np.track_id, "vid1");
        assert_eq!(np.album, "YouTube");
        assert_eq!(np.artwork_url, "https://i.ytimg.com/vi/vid1/maxresdefault.jpg");
    }

    #[test]
    fn queue_hint_fills_the_gap_before_the_resolve_lands() {
        let st = playing();
        let h = hint();
        let np = now_playing(&st, Some(&h), None, false);
        assert_eq!(np.title, "Queued title");
        assert_eq!(np.artist, "Hint channel");
        assert_eq!(np.artwork_url, "https://i.ytimg.com/vi/vid1/hqdefault.jpg");
        // No resolve yet == no video id, so the title stands in as the track id.
        assert_eq!(np.track_id, "Queued title");
    }

    #[test]
    fn an_empty_session_is_still_named_and_offers_no_skip() {
        let st = playing();
        let np = now_playing(&st, None, None, false);
        assert_eq!(np.title, "Blobtunes");
        assert!(!np.title.is_empty(), "the OS widget needs a primary line");
        assert!(!np.can_next);
        assert!(!np.can_prev);
        assert_eq!(np.artwork_url, "", "no artwork must still publish metadata");
    }

    #[test]
    fn playback_state_maps_to_the_os_transport() {
        let mut st = playing();
        let np = now_playing(&st, None, None, false);
        assert!(np.has_track && np.playing, "playing -> Playing");

        st.paused = true;
        let np = now_playing(&st, None, None, false);
        assert!(np.has_track && !np.playing, "paused -> Paused");

        st.paused = false;
        st.playing = false;
        let np = now_playing(&st, None, None, false);
        assert!(!np.has_track, "stopped / nothing loaded -> Stopped, no track");

        st.ended = true;
        let h = hint();
        let np = now_playing(&st, Some(&h), None, false);
        assert!(np.has_track && !np.playing, "natural EOF keeps a replayable track");
    }

    #[test]
    fn live_streams_publish_no_duration_and_no_seek() {
        let st = playing(); // live: no `duration`, never `seekable`
        let h = hint();
        let np = now_playing(&st, Some(&h), None, true);
        assert_eq!(np.album, "Live");
        assert_eq!(np.duration_secs, None);
        assert_eq!(np.position_secs, None);
        assert!(!np.can_seek);
    }

    #[test]
    fn queue_geometry_drives_the_skip_buttons() {
        let st = playing();
        let h = hint(); // first of two: next yes, previous no
        let np = now_playing(&st, Some(&h), None, false);
        assert!(np.can_next && !np.can_prev);
    }
}
