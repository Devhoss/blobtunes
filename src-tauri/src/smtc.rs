//! Windows System Media Transport Controls (SMTC) bridge — the OS media session
//! that Windows' media flyout, the hardware media keys and Venu's Media slide
//! read.
//!
//! WHY THIS EXISTS: WebView2 has no media-session integration (Chromium's
//! `navigator.mediaSession` never reaches WinRT from a WebView2 host), so before
//! this module the player was invisible to the OS even while audibly playing.
//!
//! SHAPE — deliberately thin, and NOT a second media pipeline:
//!
//! ```text
//!   player core thread --publish(NowPlaying)--> smtc thread --> playwire --> WinRT SMTC
//!   player core thread <----CommandSink-------- smtc thread <-- media keys / Venu
//! ```
//!
//! * ONE extra thread, blocked on a channel. No polling loop, no timers, no
//!   per-frame work anywhere.
//! * `player.rs` owns WHAT is published (it has the truth); this module owns HOW
//!   it maps onto SMTC and WHEN it is worth crossing to WinRT: metadata/state
//!   changes go out immediately, position-only changes at most every
//!   [`POSITION_PUBLISH_INTERVAL`] seconds. Windows extrapolates the timeline
//!   from the last update plus the playback status, so 2s stays continuous.
//! * Artwork is handed to Windows as the YouTube thumbnail **URI**: the OS
//!   fetches and caches it asynchronously on its own thread, and playwire only
//!   rebuilds the thumbnail reference when the track (or its duration) changes.
//!   We never download, decode, resize, re-upload or hold the image ourselves,
//!   and nothing image-related happens on a position tick.
//! * Commands travel back through the player's EXISTING command channel
//!   ([`CommandSink`]); skip commands are forwarded to the queue owner (the
//!   frontend), whose reducer actions are the single source of truth. No second
//!   command system, no duplicated transport state machine.
//! * Every failure is logged and swallowed: SMTC is a pure side channel, so
//!   playback can never depend on it. On non-Windows targets this module
//!   compiles to a no-op.
//! * The session exists only while something is loaded: it is created on the
//!   first track and released again when the player stops, so nothing lingers
//!   in Windows/Venu while Blobtunes is idle. A natural end of a track is NOT a
//!   stop — the finished track stays visible (and replayable from the OS).
//!
//! The WinRT/SMTC layer itself is `playwire` (a maintained cross-platform
//! media-controls crate whose Windows backend is SMTC); nothing here talks to
//! `windows`/WinRT directly except through it.

/// A transport command that arrived from the OS (hardware media key, Windows
/// media flyout, Venu, ...).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MediaCommand {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    /// Absolute target, in seconds. The SMTC thread resolves fast-forward /
    /// rewind into an absolute position before dispatching it, so the player
    /// only ever sees the seek it already supports.
    SeekTo(f64),
}

/// Where OS commands go. Implemented by the player core, which reuses its own
/// command channel and the frontend's queue actions for them.
///
/// `Send + Sync + 'static` because WinRT delivers media keys on a thread-pool
/// thread, not on the thread that created the session.
pub trait CommandSink: Send + Sync + 'static {
    fn dispatch(&self, cmd: MediaCommand);
}

/// Everything the Windows media session needs about the current item.
///
/// Borrowed from the player core's own state, so building it costs nothing: it
/// is filled in on every loop iteration and [`Smtc::publish`] decides whether
/// anything actually has to cross to WinRT.
pub struct NowPlaying<'a> {
    /// Stable per-track id (the YouTube video id when known; used by the OS to
    /// tell a genuinely new item from a metadata republish).
    pub track_id: &'a str,
    /// Never empty — the OS widget shows this as its primary line.
    pub title: &'a str,
    /// Channel / uploader.
    pub artist: &'a str,
    /// Where the item came from: "Live" or "YouTube".
    pub album: &'a str,
    /// Thumbnail URL, or "" when the track has no artwork (the session is still
    /// published, just without a picture).
    pub artwork_url: &'a str,
    /// A track is loaded/selected. `false` reports Stopped with no metadata.
    pub has_track: bool,
    /// Playback is advancing (Playing vs Paused).
    pub playing: bool,
    /// `None` until mpv reports one; also `None` for live HLS, whose "duration"
    /// is only its sliding window (the player classifies that away).
    pub position_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    /// Whether the OS should offer a scrubber / seek buttons.
    pub can_seek: bool,
    pub can_next: bool,
    pub can_prev: bool,
}

/// Minimum movement that makes a position-only update worth publishing.
///
/// Windows interpolates the timeline between updates, so a couple of seconds is
/// visually indistinguishable — and this is what keeps SMTC to ~1 WinRT call per
/// 2s of playback instead of one per position tick.
pub const POSITION_PUBLISH_INTERVAL: f64 = 2.0;

#[cfg(windows)]
pub use win::Smtc;
#[cfg(not(windows))]
pub use other::Smtc;

// ---------------------------------------------------------------------------
// Windows: the real SMTC session, driven by `playwire`
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod win {
    use super::{CommandSink, MediaCommand, NowPlaying, POSITION_PUBLISH_INTERVAL};
    use crate::player::dbg;
    use playwire::{Capabilities, Event, MediaControls, PlaybackState, PlayerConfig, Repeat, Track};
    use std::fs::File;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    /// Player thread -> SMTC thread.
    enum Request {
        /// Publish this snapshot to the OS.
        Publish(Published),
        /// Release the session and stop the thread (player shutdown).
        Detach,
    }

    /// An owned copy of a [`NowPlaying`] snapshot, built only when something is
    /// actually worth sending.
    #[derive(Clone, Default)]
    struct Published {
        track_id: String,
        title: String,
        artist: String,
        album: String,
        artwork: String,
        has_track: bool,
        playing: bool,
        can_seek: bool,
        can_next: bool,
        can_prev: bool,
        duration_secs: Option<f64>,
        position_secs: f64,
    }

    /// Seconds, rounded. mpv reports durations with sub-second noise; comparing
    /// raw `f64`s would republish metadata on jitter alone.
    fn duration_bucket(secs: Option<f64>) -> Option<f64> {
        secs.filter(|d| d.is_finite() && *d > 0.0)
            .map(|d| d.round())
    }

    impl Published {
        /// Copies a borrowed snapshot. Allocation happens HERE and nowhere else,
        /// so the per-iteration cost on the player thread is a handful of
        /// comparisons against the last published copy.
        fn from(np: &NowPlaying<'_>) -> Self {
            Self {
                track_id: np.track_id.to_owned(),
                title: np.title.to_owned(),
                artist: np.artist.to_owned(),
                album: np.album.to_owned(),
                artwork: np.artwork_url.to_owned(),
                has_track: np.has_track,
                playing: np.playing,
                can_seek: np.can_seek,
                can_next: np.can_next,
                can_prev: np.can_prev,
                duration_secs: duration_bucket(np.duration_secs),
                position_secs: np.position_secs.unwrap_or(0.0).max(0.0),
            }
        }

        /// Everything except the position. Any change here is published
        /// immediately: track change, play/pause, duration discovered, queue
        /// geometry (which greys out the OS next/prev buttons).
        fn matches(&self, np: &NowPlaying<'_>) -> bool {
            self.track_id == np.track_id
                && self.title == np.title
                && self.artist == np.artist
                && self.album == np.album
                && self.artwork == np.artwork_url
                && self.has_track == np.has_track
                && self.playing == np.playing
                && self.can_seek == np.can_seek
                && self.can_next == np.can_next
                && self.can_prev == np.can_prev
                && duration_bucket(np.duration_secs) == self.duration_secs
        }

        /// Whether the timeline moved enough to be worth telling the OS:
        /// backwards is always a seek, forwards waits out one interval.
        fn position_is_fresh(&self, np: &NowPlaying<'_>) -> bool {
            let pos = np.position_secs.unwrap_or(0.0).max(0.0);
            let delta = pos - self.position_secs;
            delta < 0.0 || delta >= POSITION_PUBLISH_INTERVAL
        }

        /// The `playwire` snapshot: the only place in Blobtunes that knows what
        /// SMTC looks like.
        fn to_playwire(&self) -> PlaybackState {
            let track = self.has_track.then(|| Track {
                id: self.track_id.clone(),
                title: self.title.clone(),
                artists: if self.artist.is_empty() {
                    Vec::new()
                } else {
                    vec![self.artist.clone()]
                },
                album: self.album.clone(),
                // Windows fetches and caches this URL itself (see module docs).
                artwork_url: self.artwork.clone(),
                url: String::new(), // no SMTC field for it
            });
            PlaybackState {
                track,
                playing: self.playing,
                position: Duration::from_secs_f64(self.position_secs.max(0.0)),
                duration: self.duration_secs.map(Duration::from_secs_f64),
                volume: 1.0, // MPRIS-only field; ignored by the Windows backend
                // The frontend queue owns repeat and there is no shuffle, so
                // never advertise a mode the player would not honour.
                repeat: Repeat::Off,
                shuffle: false,
                capabilities: Capabilities {
                    can_go_next: self.can_next,
                    can_go_previous: self.can_prev,
                    can_seek: self.can_seek,
                },
            }
        }
    }

    /// Handle owned by the player core thread.
    pub struct Smtc {
        tx: Sender<Request>,
        /// Last snapshot the OS was told about; the throttle lives here so the
        /// player thread only ever does comparisons.
        last: Mutex<Option<Published>>,
        log: Arc<Mutex<File>>,
        handle: Mutex<Option<JoinHandle<()>>>,
    }

    impl Smtc {
        /// Spawn the SMTC thread. `hwnd` is the app's main window: SMTC attaches
        /// to a window, and the interop call that creates the session needs one.
        pub fn start(hwnd: u64, sink: Arc<dyn CommandSink>, log: Arc<Mutex<File>>) -> Self {
            let (tx, rx) = channel::<Request>();
            let worker_log = Arc::clone(&log);
            let handle = thread::Builder::new()
                .name("blobtunes-smtc".into())
                .spawn(move || worker(rx, hwnd, sink, worker_log))
                .map_err(|e| dbg(&log, &format!("smtc thread spawn failed: {e}")))
                .ok();
            Self {
                tx,
                last: Mutex::new(None),
                log,
                handle: Mutex::new(handle),
            }
        }

        /// Offer a snapshot. A no-op unless something changed (or the timeline
        /// moved a whole interval), and never blocking: all WinRT work happens
        /// on the SMTC thread.
        pub fn publish(&self, np: NowPlaying<'_>) {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let send = match last.as_ref() {
                None => true,
                Some(prev) => !prev.matches(&np) || prev.position_is_fresh(&np),
            };
            if !send {
                return;
            }
            let next = Published::from(&np);
            *last = Some(next.clone());
            drop(last);
            if self.tx.send(Request::Publish(next)).is_err() {
                dbg(&self.log, "smtc thread is gone; publish ignored");
            }
        }

        /// Release the OS session and join the SMTC thread. Idempotent, so it
        /// can run on the shutdown path and again from `Drop`.
        pub fn detach(&self) {
            {
                let handle = self.handle.lock().unwrap_or_else(|e| e.into_inner());
                if handle.is_none() {
                    return; // already detached
                }
            }
            let _ = self.tx.send(Request::Detach);
            if let Some(h) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = h.join();
            }
        }
    }

    impl Drop for Smtc {
        fn drop(&mut self) {
            self.detach();
        }
    }

    /// WinRT setup + the command mapping. Runs on the SMTC thread.
    ///
    /// `MediaControls::new` is the only fallible step: a failure is logged once
    /// and the session is never retried, so a machine without the SMTC service
    /// just plays on without a media session.
    fn start_controls(
        hwnd: u64,
        sink: Arc<dyn CommandSink>,
        position: Arc<Mutex<f64>>,
        log: &Arc<Mutex<File>>,
    ) -> playwire::Result<MediaControls> {
        // No COM/WinRT apartment setup is needed: `playwire` activates SMTC via
        // windows-rs, whose factory path falls back to `CoIncrementMTAUsage` on
        // a thread that has no apartment, and SMTC's objects are agile.
        let event_log = Arc::clone(log);
        MediaControls::new(PlayerConfig::new("Blobtunes").hwnd(hwnd), move |event| {
            // Called on a WinRT thread-pool thread: hand the command straight to
            // the player and return. Never block, never touch player state.
            let cmd = match event {
                Event::Play => Some(MediaCommand::Play),
                Event::Pause => Some(MediaCommand::Pause),
                Event::Stop => Some(MediaCommand::Stop),
                Event::Next => Some(MediaCommand::Next),
                Event::Previous => Some(MediaCommand::Previous),
                Event::SeekTo(target) => Some(MediaCommand::SeekTo(target.as_secs_f64())),
                // Windows fast-forward/rewind carry no magnitude: resolve them
                // against the position last published to the OS and send the
                // absolute seek the player already understands.
                Event::SeekBy(delta) => {
                    let base = *position.lock().unwrap_or_else(|e| e.into_inner());
                    Some(MediaCommand::SeekTo((base + delta).max(0.0)))
                }
                // PlayPause never arrives on Windows (the key is resolved to
                // Play/Pause first); shuffle/repeat/volume/URI are not part of
                // this player's model, so they are ignored rather than faked.
                _ => None,
            };
            if let Some(cmd) = cmd {
                dbg(&event_log, &format!("smtc command from OS: {cmd:?}"));
                sink.dispatch(cmd);
            }
        })
    }

    fn worker(rx: Receiver<Request>, hwnd: u64, sink: Arc<dyn CommandSink>, log: Arc<Mutex<File>>) {
        dbg(&log, "smtc worker up");
        // Created on the first publish that has something to show, so an idle
        // Blobtunes never registers an empty media session.
        let mut controls: Option<MediaControls> = None;
        let mut unavailable = false;
        // Latest published position, for resolving relative seeks.
        let position = Arc::new(Mutex::new(0.0_f64));

        loop {
            let request = match rx.recv() {
                Ok(request) => request,
                Err(_) => break, // player core is gone
            };
            match request {
                Request::Detach => break,
                Request::Publish(p) => {
                    if unavailable {
                        continue;
                    }
                    // Nothing playing (explicit Stop, terminal error, or nothing
                    // loaded yet): take the session down instead of publishing
                    // an empty one. A session with no track is not "cleared" by
                    // the OS — the display updater stamps its own placeholder
                    // title on it — so a lingering stopped entry would show up in
                    // Venu with a bogus name. Released here, it simply
                    // disappears, and the next track recreates it.
                    if !p.has_track {
                        if let Some(mut controls) = controls.take() {
                            controls.detach();
                            dbg(&log, "smtc session released (nothing playing)");
                        }
                        continue;
                    }
                    if controls.is_none() {
                        match start_controls(hwnd, Arc::clone(&sink), Arc::clone(&position), &log) {
                            Ok(c) => {
                                controls = Some(c);
                                dbg(&log, "smtc session created");
                            }
                            Err(e) => {
                                unavailable = true;
                                dbg(&log, &format!("smtc init failed (playback unaffected): {e}"));
                                continue;
                            }
                        }
                    }
                    *position.lock().unwrap_or_else(|e| e.into_inner()) = p.position_secs;
                    if let Some(controls) = controls.as_mut() {
                        if let Err(e) = controls.set_state(&p.to_playwire()) {
                            // Transient by nature (a window being torn down, a
                            // hiccup in the OS service): keep going.
                            dbg(&log, &format!("smtc set_state failed: {e}"));
                        }
                    }
                }
            }
        }

        if let Some(mut controls) = controls {
            controls.detach(); // releases the session: no ghost entry in Venu/Windows
            dbg(&log, "smtc session released");
        }
        dbg(&log, "smtc worker down");
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn snapshot() -> NowPlaying<'static> {
            NowPlaying {
                track_id: "abc123",
                title: "Song",
                artist: "Channel",
                album: "YouTube",
                artwork_url: "https://i.ytimg.com/vi/abc123/hqdefault.jpg",
                has_track: true,
                playing: true,
                position_secs: Some(10.0),
                duration_secs: Some(240.0),
                can_seek: true,
                can_next: false,
                can_prev: false,
            }
        }

        fn published() -> Published {
            Published::from(&snapshot())
        }

        #[test]
        fn unchanged_snapshot_needs_no_publish() {
            let last = published();
            assert!(last.matches(&snapshot()));
            assert!(!last.position_is_fresh(&snapshot()));
        }

        #[test]
        fn small_position_move_is_throttled_large_or_backward_is_not() {
            let last = published();
            let mut np = snapshot();
            np.position_secs = Some(11.0);
            assert!(last.matches(&np));
            assert!(!last.position_is_fresh(&np), "1s move must stay throttled");
            np.position_secs = Some(12.0);
            assert!(last.position_is_fresh(&np), "2s move must publish");
            np.position_secs = Some(3.0);
            assert!(last.position_is_fresh(&np), "backward jump is a seek");
        }

        #[test]
        fn metadata_and_state_changes_always_publish() {
            let last = published();
            let mut np = snapshot();
            np.title = "Another";
            assert!(!last.matches(&np));
            let mut np = snapshot();
            np.playing = false;
            assert!(!last.matches(&np));
            let mut np = snapshot();
            np.can_next = true;
            assert!(!last.matches(&np));
            let mut np = snapshot();
            np.duration_secs = Some(241.9);
            assert!(!last.matches(&np), "a different second of duration counts");
            let mut np = snapshot();
            np.duration_secs = Some(240.4);
            assert!(last.matches(&np), "sub-second duration noise does not");
        }

        #[test]
        fn live_track_publishes_without_a_duration() {
            let mut np = snapshot();
            np.duration_secs = None;
            np.album = "Live";
            np.can_seek = false;
            let state = Published::from(&np).to_playwire();
            assert_eq!(state.duration, None);
            assert!(state.track.is_some());
            assert!(!state.capabilities.can_seek);
            assert!(!state.capabilities.can_go_next);
        }

        #[test]
        fn stopped_publishes_no_track() {
            let mut np = snapshot();
            np.has_track = false;
            np.playing = false;
            assert!(Published::from(&np).to_playwire().track.is_none());
        }
    }
}

// ---------------------------------------------------------------------------
// Everything else: no-op, so the player core compiles unchanged
// ---------------------------------------------------------------------------
#[cfg(not(windows))]
mod other {
    use super::{CommandSink, NowPlaying};
    use std::fs::File;
    use std::sync::{Arc, Mutex};

    /// SMTC is a Windows feature. Elsewhere this is inert — and the player core
    /// never even starts one, because there is no window handle to attach to.
    pub struct Smtc;

    impl Smtc {
        pub fn start(_hwnd: u64, _sink: Arc<dyn CommandSink>, _log: Arc<Mutex<File>>) -> Self {
            Self
        }
        pub fn publish(&self, _np: NowPlaying<'_>) {}
        pub fn detach(&self) {}
    }
}
