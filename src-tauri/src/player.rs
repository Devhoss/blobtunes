use anyhow::{anyhow, Result};
use libmpv2::{events::Event, events::PropertyData, mpv_end_file_reason, Format, Mpv};
use serde::Serialize;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::{AppHandle, Emitter};

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
        let handle = thread::Builder::new()
            .name("wavesurf-mpv".into())
            .spawn(move || run_core(app, tx2, rx, st))
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

fn build_mpv() -> Result<Mpv> {
    let mpv = Mpv::with_initializer(|init| {
        init.set_option("vo", "null")?; // never create a video output
        init.set_option("video", "no")?; // never decode a video track
        init.set_option("audio-display", "no")?;
        init.set_option("ytdl", "yes")?; // fallback only; our loads are pre-resolved
        init.set_option("ytdl-format", "bestaudio/best")?; // audio-first; libmpv decodes anything
                                                           // Harmless Chrome UA for any direct googlevideo fetch mpv still does
                                                           // (e.g. HLS segments). NOTE: the VOD 403s were NOT a UA problem — they
                                                           // were range-gated edges rejecting ffmpeg's open-ended ranges; the fix
                                                           // is tiered URLs (dash primary, progressive fallback), not headers.
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
        init.set_option("log-file", "E:/dev/wavesurf/mpv-debug.log")?;
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

fn dbg(log: &std::sync::Arc<std::sync::Mutex<std::fs::File>>, msg: &str) {
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

/// Kick off a yt-dlp resolve on its own thread and route the result back
/// into the owner thread's command channel, tagged with `gen` so a
/// superseded resolve can be told apart from the current one.
fn spawn_live_resolver(tx: &Sender<PlayerCmd>, gen: u64, url: String) {
    let tx2 = tx.clone();
    thread::Builder::new()
        .name("wavesurf-live-resolve".into())
        .spawn(move || {
            let cmd = match crate::ytdlp::resolve_live_hls(&url) {
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
    thread::Builder::new()
        .name("wavesurf-resolve".into())
        .spawn(move || {
            let cmd = match crate::ytdlp::resolve_stream(&url) {
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

/// Max automatic reconnect attempts for a live stream before giving up and
/// surfacing a real error (guards against spinning forever on a broadcast
/// that has genuinely ended).
const MAX_LIVE_RETRIES: u32 = 3;

/// Attempt automatic recovery after a playback failure. `reason` names the
/// trigger (EndFile tag / wait_event error / watchdog) and is logged with
/// every attempt so the log always shows WHY a recovery happened.
/// - VOD: one reactive android-progressive resolve per track load
///   (`vod_fallback_used`), then a terminal error. Bounded by construction.
/// - Live: re-resolve + reload, capped at MAX_LIVE_RETRIES.
/// Returns true if a recovery attempt was kicked off (state left in
/// loading/playing), false if the caller should surface a terminal error.
/// Never sets `ended`: recovering means "still trying" — only the terminal
/// paths in the EndFile handler may mark a track ended.
#[allow(clippy::too_many_arguments)]
fn attempt_recovery(
    st: &mut PlayerState,
    mpv: &Mpv,
    tx: &Sender<PlayerCmd>,
    fallback: &mut Option<String>,
    is_live: bool,
    live_retry_count: &mut u32,
    current_url: &Option<String>,
    load_gen: &mut u64,
    log: &std::sync::Arc<std::sync::Mutex<std::fs::File>>,
    vod_fallback_used: &mut bool,
    reason: &str,
) -> bool {
    if let Some(url) = fallback.take() {
        dbg(
            log,
            &format!("VOD tier-2 fallback: retrying with progressive URL (trigger: {reason})"),
        );
        st.error = None;
        st.loading = true;
        st.playing = true;
        if mpv.command("loadfile", &[url.as_str(), "replace"]).is_err() {
            st.loading = false;
            st.playing = false;
            st.error = Some("mpv playback error".into());
            return false;
        }
        return true;
    }
    // VOD live-reconnect/no-fallback: the tier-1 DASH URL failed (e.g. a 403).
    // Resolve the android progressive URL reactively and retry it once — this
    // is what keeps the lazy-fallback design working without paying for the
    // slow android extraction on the fast path.
    // Bounded: exactly one reactive attempt per track load. A second failure
    // means a different problem (or a dead URL) — surfacing an error is more
    // honest than re-resolving forever.
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
            let fallback = match crate::ytdlp::resolve_fallback_url(&url) {
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
                    fallback_url: None,
                    is_hls: false,
                },
            );
            let _ = tx.send(cmd);
        };
        thread::Builder::new()
            .name("wavesurf-fallback".into())
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
) {
    // Prefer the usual dev-machine path, but never let a missing drive/dir
    // panic this thread (that used to brick the whole player permanently
    // with no supervision to restart it). Fall back to the OS temp dir, and
    // to no logging at all rather than crashing if even that fails.
    let log_file = std::sync::Arc::new(std::sync::Mutex::new(
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("E:/dev/wavesurf/player-debug.log")
        {
            Ok(f) => f,
            Err(_) => match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(std::env::temp_dir().join("wavesurf-player-debug.log"))
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
    // Tier-2 fallback URL (progressive), armed per track. Consumed once on an
    // early EndFile(Error); cleared on every new load/stop.
    let mut fallback: Option<String> = None;
    // Original URL of the currently loaded track, kept so a live stream can
    // be re-resolved and reloaded on failure.
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
    // per track load (reset on Load/Stop). Without this, a track whose
    // fallback URL also fails would re-resolve forever.
    let mut vod_fallback_used = false;

    loop {
        // 1) drain pending commands (non-blocking)
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, PlayerCmd::Shutdown) {
                dbg(&log, "Shutdown received");
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
                    last_raw_duration = None;
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
                        dbg(&log, &format!("stale ResolveFailed (gen {gen}) ignored"));
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
                        // Reactive VOD fallback resolves carry an empty meta
                        // (URL-only recovery) — don't clobber the real title.
                        if !t.meta.title.is_empty() {
                            let mut st = lock_state(&state);
                            st.title = Some(t.meta.title.clone());
                        }
                        if t.is_hls {
                            // Live HLS: native playback, no VOD fallback tier applies
                            // (auto-reconnect on failure is handled separately, above).
                            fallback = None;
                            mpv.command("loadfile", &[t.primary_url.as_str(), "replace"])
                                .map_err(|e| anyhow!("loadfile: {e}"))
                        } else {
                            // VOD tier 1 (best dash) now, tier 2 (progressive)
                            // armed for one automatic retry on early failure.
                            fallback = t.fallback_url.clone();
                            mpv.command("loadfile", &[t.primary_url.as_str(), "replace"])
                                .map_err(|e| anyhow!("loadfile: {e}"))
                        }
                    }
                }
                PlayerCmd::Play => {
                    dbg(&log, "Play");
                    mpv.set_property("pause", false)
                        .map_err(|e| anyhow!("play: {e}"))
                }
                PlayerCmd::Pause => {
                    dbg(&log, "Pause");
                    mpv.set_property("pause", true)
                        .map_err(|e| anyhow!("pause: {e}"))
                }
                PlayerCmd::Stop => {
                    dbg(&log, "Stop");
                    fallback = None;
                    // Prevent a stray post-Stop EndFile(Error) from
                    // triggering an unwanted reconnect on a track the user
                    // explicitly stopped.
                    is_live = false;
                    current_url = None;
                    live_retry_count = 0;
                    vod_fallback_used = false;
                    mpv.command("stop", &[]).map_err(|e| anyhow!("stop: {e}"))
                }
                PlayerCmd::Seek(s) => {
                    dbg(&log, &format!("Seek: {s}"));
                    mpv.command("seek", &[&s.to_string(), "absolute"])
                        .map_err(|e| anyhow!("seek: {e}"))
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
                        &mpv,
                        &tx,
                        &mut fallback,
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
                                fallback = None;
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
                                    &mpv,
                                    &tx,
                                    &mut fallback,
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
            dbg!(
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
                                &mpv,
                                &tx,
                                &mut fallback,
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

        // 3) push state to the UI if it changed
        let snapshot = lock_state(&state).clone();
        if last.as_ref() != Some(&snapshot) {
            let _ = app.emit("player://state", &snapshot);
            last = Some(snapshot);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn live_duration_maps_to_none() {
        let duration = 0.0_f64;
        assert_eq!(Some(duration).filter(|d| *d > 0.5), None);
    }
}
