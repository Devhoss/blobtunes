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
    pub loading: bool, // resolving/buffering (StartFile->FileLoaded)
    pub position: Option<f64>, // seconds; None until known
    pub duration: Option<f64>, // None = unknown or LIVE (never guess)
    pub seekable: bool,
    pub volume: u8,
    pub title: Option<String>,
    pub ended: bool, // one-shot: natural EOF consumed by frontend
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum PlayerCmd {
    Load(String), // any public YouTube URL; resolved off-thread, then played
    /// Internal: resolver thread → owner thread. NEVER sent from the UI.
    LoadResolved(crate::ytdlp::ResolvedTrack),
    /// Internal: resolver thread → owner thread on yt-dlp failure.
    ResolveFailed(String),
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
        let state = Arc::new(Mutex::new(PlayerState::default()));
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
        self.tx.send(cmd).map_err(|_| anyhow!("player core is down"))
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

fn run_core(
    app: AppHandle,
    tx: Sender<PlayerCmd>,
    rx: Receiver<PlayerCmd>,
    state: Arc<Mutex<PlayerState>>,
) {
    let mpv = match build_mpv() {
        Ok(m) => m,
        Err(e) => {
            if let Ok(mut st) = state.lock() {
                st.error = Some(format!("mpv init failed: {e}"));
            }
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

    loop {
        // 1) drain pending commands (non-blocking)
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, PlayerCmd::Shutdown) {
                mpv.command("stop", &[]).ok(); // release audio promptly
                return; // drops Mpv on THIS thread, then the thread exits; Player::shutdown joins it
            }
            let r: Result<()> = match &cmd {
                // Resolve off-thread: yt-dlp takes ~5-15s and must never stall
                // this event loop (state pushes + playback events would freeze).
                PlayerCmd::Load(url) => {
                    if let Ok(mut st) = state.lock() {
                        st.loading = true;
                        st.playing = true;
                        st.error = None;
                        st.ended = false;
                        st.position = None;
                        st.duration = None;
                        st.title = None;
                    }
                    let tx2 = tx.clone();
                    let url = url.clone();
                    thread::Builder::new()
                        .name("wavesurf-resolve".into())
                        .spawn(move || {
                            let cmd = match crate::ytdlp::resolve_stream(&url) {
                                Ok(t) => PlayerCmd::LoadResolved(t),
                                Err(e) => PlayerCmd::ResolveFailed(e.to_string()),
                            };
                            let _ = tx2.send(cmd);
                        })
                        .ok();
                    Ok(())
                }
                PlayerCmd::ResolveFailed(e) => {
                    if let Ok(mut st) = state.lock() {
                        st.loading = false;
                        st.playing = false;
                        st.error = Some(e.to_string());
                    }
                    Ok(())
                }
                PlayerCmd::LoadResolved(t) => {
                    if let Ok(mut st) = state.lock() {
                        st.title = Some(t.meta.title.clone());
                    }
                    if t.is_hls {
                        // Live HLS: native playback, no fallback tier applies.
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
                PlayerCmd::Play => mpv
                    .set_property("pause", false)
                    .map_err(|e| anyhow!("play: {e}")),
                PlayerCmd::Pause => mpv
                    .set_property("pause", true)
                    .map_err(|e| anyhow!("pause: {e}")),
                PlayerCmd::Stop => {
                    fallback = None;
                    mpv.command("stop", &[]).map_err(|e| anyhow!("stop: {e}"))
                }
                PlayerCmd::Seek(s) => mpv
                    .command("seek", &[&s.to_string(), "absolute"])
                    .map_err(|e| anyhow!("seek: {e}")),
                PlayerCmd::SetVolume(v) => mpv
                    .set_property("volume", i64::from(*v))
                    .map_err(|e| anyhow!("volume: {e}")),
                PlayerCmd::Shutdown => Ok(()), // handled above; unreachable here
            };
            if let Err(e) = r {
                if let Ok(mut st) = state.lock() {
                    st.error = Some(e.to_string());
                }
            }
        }

        // 2) process one mpv event (250ms poll keeps command latency low).
        // NOTE: libmpv2 reports a failed EndFile as `Err`, NOT as an Event —
        // filtering on `Some(Ok(..))` drops it and hangs the UI on loading
        // forever. Both arms below must stay in sync.
        if let Some(evt) = mpv.wait_event(0.25) {
            match evt {
                Err(e) => {
                    let mut st = state.lock().unwrap();
                    st.playing = false;
                    st.loading = false;
                    if let Some(url) = fallback.take() {
                        st.error = None;
                        st.loading = true;
                        st.playing = true;
                        if mpv.command("loadfile", &[url.as_str(), "replace"]).is_err() {
                            st.loading = false;
                            st.playing = false;
                            st.error = Some(format!("mpv event error: {e}"));
                        }
                    } else {
                        st.error = Some(format!("mpv event error: {e}"));
                    }
                }
                Ok(ev) => {
                    let mut st = state.lock().unwrap();
                    match ev {
                Event::StartFile => {
                    st.loading = true;
                    st.playing = true;
                    st.error = None;
                    st.ended = false;
                    st.position = None;
                    st.duration = None;
                }
                Event::FileLoaded => {
                    st.loading = false;
                    st.paused = false;
                    if let Ok(title) = mpv.get_property::<String>("media-title") {
                        st.title = Some(title);
                    }
                }
                Event::EndFile(reason) => {
                    if reason == mpv_end_file_reason::Eof {
                        st.ended = true; // frontend advances queue on this
                        st.playing = false;
                        fallback = None;
                    } else {
                        // Commanded stop/quit/redirect: clear playback, never set `ended`.
                        st.playing = false;
                        st.loading = false;
                        if reason == mpv_end_file_reason::Error {
                            // Tier-2 retry, once: if the primary (dash) URL was
                            // rejected (e.g. range-gated edge 403), fall back to
                            // the progressive URL. Otherwise surface the error.
                            if let Some(url) = fallback.take() {
                                st.error = None;
                                st.loading = true;
                                st.playing = true;
                                if mpv.command("loadfile", &[url.as_str(), "replace"]).is_err() {
                                    st.loading = false;
                                    st.playing = false;
                                    st.error = Some("mpv playback error".into());
                                }
                            } else {
                                st.error = Some("mpv playback error".into());
                            }
                        }
                    }
                }
                Event::PropertyChange {
                    change, reply_userdata, ..
                } => match reply_userdata {
                    1 => st.position = as_opt_f64(&change), // time-pos
                    2 => {
                        let d = as_opt_f64(&change).filter(|d| *d > 0.5);
                        // HLS live reports a GROWING duration window (15s, 20s…)
                        // while never becoming seekable — a bare ">0.5 ⟹ VOD"
                        // check would show fake totals on live streams.
                        // live ⟺ never seekable AND position advanced past 2s
                        // (rules out VODs still buffering, where pos == 0).
                        // Direct VOD streams report seekable from stream open.
                        let live = !st.seekable
                            && st.position.unwrap_or(0.0) > 2.0
                            && d.is_some();
                        st.duration = if live { None } else { d };
                    }
                    3 => st.paused = matches!(change, PropertyData::Flag(true)),
                    4 => st.seekable = matches!(change, PropertyData::Flag(true)),
                    5 => {} // core-idle: covered by FileLoaded/Idle events
                    _ => {}
                },
                _ => {}
                    } // close match ev
                } // close Ok(ev) arm
            } // close match evt
        }

        // 3) push state to the UI if it changed
        let snapshot = state.lock().unwrap().clone();
        if last.as_ref() != Some(&snapshot) {
            let _ = app.emit("player://state", &snapshot);
            last = Some(snapshot);
        }
    }
}

fn as_opt_f64(d: &PropertyData) -> Option<f64> {
    match d {
        PropertyData::Double(v) => Some(*v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_duration_maps_to_none() {
        // duration property is 0.0 on live -> UI must never see a fake 0s duration
        let d = as_opt_f64(&PropertyData::Double(0.0)).filter(|d| *d > 0.5);
        assert_eq!(d, None);
    }
}
