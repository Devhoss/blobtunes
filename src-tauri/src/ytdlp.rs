use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use wait_timeout::ChildExt;

/// Error message that marks a resolve as superseded (its child was already
/// killed by wait_cancel). single_flight keys on this exact string to
/// decide Gone vs Done — keep both sides using the constant.
pub(crate) const SUPERSEDED_MSG: &str = "superseded by newer load";

/// Spawn children without a console window. The release binary is a windowed
/// (no-console) app, so every console-subsystem child — yt-dlp.exe, `where` —
/// otherwise gets Windows to allocate a fresh, VISIBLE terminal per spawn
/// (the "two terminals per song" report). Piping stdio does NOT prevent it.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Subset of one entry in yt-dlp's `formats` array.
#[derive(Debug, Deserialize, Clone)]
struct DlpFormat {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    vcodec: Option<String>,
    #[serde(default)]
    acodec: Option<String>,
    #[serde(default)]
    abr: Option<f64>,
    #[serde(default)]
    tbr: Option<f64>,
    #[serde(default)]
    height: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DlpDump {
    id: String,
    title: String,
    #[serde(default)]
    channel: Option<String>, // yt-dlp "channel" — V2 contract needs it
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    is_live: Option<bool>,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    formats: Vec<DlpFormat>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TrackMeta {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration: Option<f64>,
    pub is_live: bool,
    pub thumbnail: Option<String>,
}

/// Everything playback needs, from (usually) one yt-dlp call merged.
/// URLs are never stored in the QUEUE (they expire) — but a short-TTL
/// resolve cache below absorbs repeats and back-navigation, which would
/// otherwise pay the full unpack + extraction again minutes later.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTrack {
    pub meta: TrackMeta,
    /// The URL to play first. VOD: android-client progressive (muxed MP4) —
    /// the only URL class mpv can actually open on range-gated googlevideo
    /// edges (see resolve_stream). Live: the android HLS playlist.
    pub primary_url: String,
    /// True when the primary already came from the fallback client, so the
    /// player's one reactive android re-resolve has nothing new to try — a
    /// second failure is terminal instead of spending ~10s re-resolving the
    /// same URL class.
    pub fallback_exhausted: bool,
    /// True when primary is HLS (live): plays natively.
    pub is_hls: bool,
}

/// Locate yt-dlp, returning an ABSOLUTE path.
/// mpv's ytdl hook cannot spawn by bare name on Windows ("Subprocess failed:
/// init"), and our own `Command::new` shouldn't depend on shell PATH state —
/// so never return a bare `"yt-dlp"`.
pub fn find_yt_dlp() -> Result<String> {
    // 1) WinGet command alias dir (present after a shell restart).
    // 2) WinGet package dir (works immediately after install).
    // 3) Anything called yt-dlp(.exe) already on PATH.
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = std::path::PathBuf::from(local);
        candidates.push(
            local
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("yt-dlp.exe"),
        );
        let pkgs = local.join("Microsoft").join("WinGet").join("Packages");
        if let Ok(entries) = std::fs::read_dir(&pkgs) {
            for e in entries.flatten() {
                let p = e.path().join("yt-dlp.exe");
                if p.is_file() {
                    candidates.push(p);
                }
            }
        }
    }
    for c in candidates {
        if c.is_file() {
            return Ok(c.to_string_lossy().into_owned());
        }
    }
    // PATH fallback: `where` on Windows.
    let mut where_cmd = std::process::Command::new("where");
    where_cmd.arg("yt-dlp");
    #[cfg(windows)]
    where_cmd.creation_flags(CREATE_NO_WINDOW);
    if let Ok(out) = where_cmd.output() {
        let first = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if !first.is_empty() && std::path::Path::new(&first).is_file() {
            return Ok(first);
        }
    }
    Err(anyhow!(
        "yt-dlp not found — install it: winget install yt-dlp.yt-dlp (then restart the shell)"
    ))
}

/// Short-TTL VOD resolve cache: video URL -> resolved track. Repeats and
/// back-navigation within the window skip the ~5s unpack plus the full
/// network extraction (measured: same video re-resolved 30s apart at full
/// cost). googlevideo URLs live hours; 60min TTL is still ~6x conservative
/// and outlives the longest normal song, so a prefetch fired ~45s before
/// track end is a hit when the next load resolves. A rare expired URL is
/// covered by the reactive re-resolve in player.rs.
/// Live HLS is NEVER cached: playlists go stale fast and reconnects want
/// fresh URLs. Entries are ~1KB; the map is pruned on every store.
const CACHE_TTL_SECS: u64 = 60 * 60;

static RESOLVE_CACHE: OnceLock<Mutex<HashMap<String, (std::time::Instant, ResolvedTrack)>>> =
    OnceLock::new();

fn cache_slot() -> &'static Mutex<HashMap<String, (std::time::Instant, ResolvedTrack)>> {
    RESOLVE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_fresh(when: std::time::Instant) -> bool {
    when.elapsed().as_secs() < CACHE_TTL_SECS
}

fn cache_lookup(url: &str) -> Option<ResolvedTrack> {
    let slot = cache_slot().lock().ok()?;
    let (when, track) = slot.get(url)?;
    if !track.is_hls && cache_fresh(*when) {
        return Some(track.clone());
    }
    None
}

fn cache_store(url: &str, track: &ResolvedTrack) {
    if track.is_hls {
        return;
    }
    let Ok(mut slot) = cache_slot().lock() else {
        return;
    };
    slot.retain(|_, (when, _)| cache_fresh(*when));
    slot.insert(url.to_string(), (std::time::Instant::now(), track.clone()));
}

// ---------------------------------------------------------------------------
// Single-flight resolves, keyed by URL.
//
// A prefetch and a play for the SAME video must not run two 7-10s yt-dlp
// dumps: the first caller (leader) runs the op, later callers (joiners)
// block on its result. A cancelled joiner never touches the leader — the
// user skipping away must not throw away a resolve that will still land in
// the cache. A leader that gets superseded (its own cancel tripped) stores
// Gone so a waiting joiner re-leads with its own fresh cancel instead of
// dying on a bumped prefetch.
// ---------------------------------------------------------------------------

enum FlightState {
    Running,
    Done,
    Gone,
}

pub(crate) struct Flight<T> {
    /// (state, result) — result is published once, cloned to every joiner.
    state: Mutex<(FlightState, Option<Result<T, String>>)>,
    cv: std::sync::Condvar,
}

impl<T> Flight<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new((FlightState::Running, None)),
            cv: std::sync::Condvar::new(),
        }
    }
}

type Flights<T> = Mutex<HashMap<String, Arc<Flight<T>>>>;

/// Joiners poll in 250ms slices (cancel checks) with a generous overall
/// budget: the leader's own dumps are bounded, so this only trips if the
/// leader's thread itself wedges.
const JOINER_BUDGET_SECS: u64 = 240;

pub(crate) fn single_flight<T, F>(
    flights: &Flights<T>,
    key: &str,
    cancel: &AtomicBool,
    op: F,
) -> Result<T>
where
    T: Clone + Send,
    F: FnOnce(&AtomicBool) -> Result<T>,
{
    loop {
        let (flight, leader) = {
            let mut map = flights.lock().unwrap_or_else(|e| e.into_inner());
            match map.get(key) {
                Some(f) => (f.clone(), false),
                None => {
                    let f = Arc::new(Flight::new());
                    map.insert(key.to_string(), f.clone());
                    (f, true)
                }
            }
        };
        if leader {
            let out = op(cancel);
            let superseded = out
                .as_ref()
                .err()
                .is_some_and(|e| e.to_string() == SUPERSEDED_MSG);
            {
                let mut st = flight.state.lock().unwrap_or_else(|e| e.into_inner());
                st.1 = Some(match &out {
                    Ok(v) => Ok(v.clone()),
                    Err(e) => Err(e.to_string()),
                });
                st.0 = if superseded {
                    FlightState::Gone
                } else {
                    FlightState::Done
                };
            }
            flight.cv.notify_all();
            let mut map = flights.lock().unwrap_or_else(|e| e.into_inner());
            if map.get(key).is_some_and(|f| Arc::ptr_eq(f, &flight)) {
                map.remove(key);
            }
            return out;
        }

        // Joiner: wait on the leader in cancel-checkable slices.
        let start = std::time::Instant::now();
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(anyhow!(SUPERSEDED_MSG));
            }
            if start.elapsed().as_secs() > JOINER_BUDGET_SECS {
                return Err(anyhow!("joiner wait budget exceeded"));
            }
            let mut st = flight.state.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(st.0, FlightState::Running) {
                let wait = flight
                    .cv
                    .wait_timeout_while(st, std::time::Duration::from_millis(250), |s| {
                        matches!(s.0, FlightState::Running)
                    })
                    .unwrap_or_else(|e| e.into_inner());
                st = wait.0;
            }
            match &st.0 {
                FlightState::Running => continue,
                FlightState::Done => {
                    return match st.1.as_ref() {
                        Some(Ok(v)) => Ok(v.clone()),
                        Some(Err(e)) => Err(anyhow!("{e}")),
                        None => Err(anyhow!("flight done without result")),
                    };
                }
                FlightState::Gone => {
                    // Drop the dead flight if nobody replaced it, then
                    // re-loop and try to lead ourselves.
                    drop(st);
                    let mut map = flights.lock().unwrap_or_else(|e| e.into_inner());
                    if map.get(key).is_some_and(|f| Arc::ptr_eq(f, &flight)) {
                        map.remove(key);
                    }
                    break;
                }
            }
        }
    }
}

static RESOLVE_FLIGHTS: OnceLock<Flights<ResolvedTrack>> = OnceLock::new();

fn resolve_flights() -> &'static Flights<ResolvedTrack> {
    RESOLVE_FLIGHTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Dedicated cancel slot for background prefetches. Kept SEPARATE from
/// player.rs's load resolver slot: starting or rebumping a prefetch must
/// never cancel the resolve for the track the user actually loaded.
static PREFETCH_CANCEL: OnceLock<Mutex<Option<Arc<AtomicBool>>>> = OnceLock::new();

/// Arm a fresh prefetch cancel flag, superseding any in-flight prefetch.
pub fn arm_prefetch_cancel() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    if let Some(slot) = PREFETCH_CANCEL.get_or_init(|| Mutex::new(None)).lock().ok() {
        let mut guard = slot;
        if let Some(old) = guard.replace(flag.clone()) {
            old.store(true, Ordering::SeqCst);
        }
    }
    flag
}

#[cfg(windows)]
/// Run a child at BELOW_NORMAL priority so a mid-song prefetch's PyInstaller
/// unpack + extractor scan doesn't fight mpv's audio decode for CPU.
pub(crate) fn set_child_below_normal(child: &std::process::Child) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    };
    unsafe {
        if let Ok(handle) = OpenProcess(
            PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            child.id(),
        ) {
            let _ = SetPriorityClass(handle, BELOW_NORMAL_PRIORITY_CLASS);
            let _ = CloseHandle(handle);
        }
    }
}

#[cfg(windows)]
pub(crate) fn child_priority_class(pid: u32) -> Option<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetPriorityClass, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        // GetPriorityClass returns 0 on failure in the windows-0.62 bindings.
        let class = GetPriorityClass(handle);
        let _ = CloseHandle(handle);
        if class == 0 {
            None
        } else {
            Some(class)
        }
    }
}

/// Wait on a spawned yt-dlp child in short slices so a superseded resolve
/// can be killed instead of burning unpack+scan+network (~30s here) to a
/// completion nobody will read.
/// - Ok(Some(status)): exited on its own.
/// - Ok(None): budget expired WITHOUT cancel — child left running for the
///   caller to kill on its existing timeout path.
/// - Err: WE cancelled it (child already killed + reaped here).
fn wait_cancel(
    child: &mut std::process::Child,
    cancel: &AtomicBool,
    budget: std::time::Duration,
) -> Result<Option<std::process::ExitStatus>> {
    let start = std::time::Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(SUPERSEDED_MSG));
        }
        match child
            .wait_timeout(std::time::Duration::from_millis(250))
            .map_err(|e| anyhow!("yt-dlp wait failed ({e})"))?
        {
            Some(status) => return Ok(Some(status)),
            None if start.elapsed() >= budget => return Ok(None),
            None => {}
        }
    }
}

fn run_dump(
    exe: &str,
    extra: &[&str],
    url: &str,
    cancel: &AtomicBool,
    bg: bool,
) -> Result<String> {
    use std::process::Stdio;
    use std::time::Duration;
    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "--no-playlist",
        "--dump-json",
        "--no-warnings",
        "--socket-timeout",
        "10",
    ])
    .args(extra)
    .args(["--", url])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to run yt-dlp ({e})"))?;
    // Prefetch children run below normal so they never steal CPU from the
    // track actually playing (Windows-only knob; other platforms are fine).
    #[cfg(windows)]
    if bg {
        set_child_below_normal(&child);
    }
    #[cfg(not(windows))]
    let _ = bg;
    // Drain both pipes concurrently: waiting before reading can deadlock when
    // yt-dlp fills stderr/stdout while the parent is blocked in wait.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut std::io::BufReader::new(stdout), &mut buf)
            .map(|_| buf)
            .unwrap_or_default()
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut std::io::BufReader::new(stderr), &mut buf)
            .map(|_| buf)
            .unwrap_or_default()
    });
    // Wall-clock budget per yt-dlp dump. Measured 2026-09-09 on this machine:
    // ~5s PyInstaller unpack+scan plus ~24s extraction for a plain VOD, so
    // 45s left no headroom and slow tracks died. 90s fits reality; the UI
    // still shows loading meanwhile and the kill path still cleans up.
    let status = match wait_cancel(&mut child, cancel, Duration::from_secs(90)) {
        Err(e) => {
            // Superseded: child already killed + reaped inside wait_cancel.
            // Just drain the pipes so the reader threads exit, then report.
            let _ = out_thread.join();
            let _ = err_thread.join();
            return Err(e);
        }
        Ok(s) => s,
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = out_thread.join();
        let _ = err_thread.join();
        return Err(anyhow!("yt-dlp timed out after 90 seconds"));
    }
    let status = status.unwrap();
    let out = std::process::Output {
        status,
        stdout: out_thread.join().unwrap_or_default(),
        stderr: err_thread.join().unwrap_or_default(),
    };
    if !out.status.success() {
        return Err(anyhow!(
            "yt-dlp: {}",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("unknown error")
                .trim_end()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Metadata-only probe for pasted URLs (fills the queue before playback).
/// Routes through the full resolve so ONE android dump serves both the
/// paste-time metadata and the play-time URL: VOD results land in the
/// resolve cache and the later load is a hit. Live metadata comes back the
/// same way; its URL is deliberately never cached.
/// Blocking — call from a background thread.
pub fn probe(url: &str, cancel: &AtomicBool) -> Result<TrackMeta> {
    resolve_stream(url, cancel).map(|t| t.meta)
}

/// Cap for one playlist import: mixes (RD…) and auto-playlists can hold
/// thousands of entries, and even paging them through would blow yt-dlp's
/// wall-clock budget below.
const PLAYLIST_IMPORT_CAP: u32 = 100;

/// Bulk metadata fetch for a pasted playlist URL. One yt-dlp call with
/// `--flat-playlist`: entries come straight off the playlist pages with no
/// per-video extraction, so a full import costs one subprocess spawn.
/// Blocking — call from a background thread.
pub fn fetch_playlist(url: &str, cancel: &AtomicBool) -> Result<Vec<TrackMeta>> {
    let exe = find_yt_dlp()?;
    // run_dump's fixed `--no-playlist` is a no-op on pure /playlist URLs
    // (it only rewrites watch?v=...&list= pages), so no flag refactor needed.
    let cap = PLAYLIST_IMPORT_CAP.to_string();
    let json = run_dump(&exe, &["--flat-playlist", "--playlist-end", &cap], url, cancel, false)?;
    parse_flat_playlist(&json)
}

/// NDJSON from `--flat-playlist --dump-json`: one JSON object per line.
/// Keeps video entries with well-formed 11-char ids; skips container
/// records (playlist-level dumps carry `entries` and long PL… ids).
fn parse_flat_playlist(out: &str) -> Result<Vec<TrackMeta>> {
    let mut tracks: Vec<TrackMeta> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("entries").is_some() {
            continue;
        }
        let Some(id) = v.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        if id.len() != 11 || !seen.insert(id.to_string()) {
            continue;
        }
        let id = id.to_string();
        let text = |keys: &[&str]| -> Option<String> {
            keys.iter()
                .find_map(|k| v.get(*k).and_then(|t| t.as_str()))
                .map(str::to_owned)
        };
        tracks.push(TrackMeta {
            title: text(&["title"]).unwrap_or_else(|| format!("Video {id}")),
            channel: text(&["channel", "uploader", "artist", "uploader_id"])
                .unwrap_or_default(),
            duration: v.get("duration").and_then(|d| d.as_f64()),
            is_live: v
                .get("is_live")
                .and_then(|l| l.as_bool())
                .unwrap_or(false),
            thumbnail: text(&["thumbnail"]),
            id,
        });
    }
    if tracks.is_empty() {
        return Err(anyhow!(
            "no entries found — private, empty, or region-locked playlist?"
        ));
    }
    Ok(tracks)
}

fn bitrate(f: &DlpFormat) -> f64 {
    f.abr.or(f.tbr).unwrap_or(0.0)
}

/// Shared metadata mapping for any parsed dump.
fn track_meta(d: &DlpDump) -> TrackMeta {
    TrackMeta {
        id: d.id.clone(),
        title: d.title.clone(),
        channel: d.channel.clone().unwrap_or_default(),
        duration: d.duration,
        is_live: d.is_live.unwrap_or(false),
        thumbnail: d.thumbnail.clone(),
    }
}

fn is_audio_only(f: &DlpFormat) -> bool {
    f.acodec.as_deref().is_some_and(|c| c != "none")
        && f.vcodec.as_deref().is_none_or(|c| c == "none")
}

fn is_progressive(f: &DlpFormat) -> bool {
    f.acodec.as_deref().is_some_and(|c| c != "none")
        && f.vcodec.as_deref().is_some_and(|c| c != "none")
}

fn http_url(f: &DlpFormat) -> Option<&str> {
    f.url
        .as_deref()
        .filter(|u| u.starts_with("http"))
        .filter(|_| f.protocol.as_deref().unwrap_or("") == "https")
}

/// Cheapest (lowest-height) progressive https URL in a format list — the
/// android client's muxed 360p (itag 18) in practice.
fn pick_progressive(formats: &[DlpFormat]) -> Option<String> {
    formats
        .iter()
        .filter(|f| is_progressive(f) && f.protocol.as_deref().unwrap_or("") == "https")
        .filter_map(|f| http_url(f).map(|u| (f.height.unwrap_or(u64::MAX), u)))
        .min_by_key(|(h, _)| *h)
        .map(|(_, u)| u.to_string())
}

/// Full resolve for playback: metadata + primary stream URL, protected by
/// the per-URL single-flight so a play arriving mid-prefetch joins the
/// in-flight dump instead of starting a competing one. Cache hits skip
/// everything. Blocking — NEVER call on the mpv owner thread.
pub fn resolve_stream(url: &str, cancel: &AtomicBool) -> Result<ResolvedTrack> {
    resolve_via(url, cancel, false)
}

/// Prefetch entry point: same flight/cache machinery, but the yt-dlp child
/// runs at below-normal priority (see run_dump). Blocking — call from a
/// background thread only.
pub fn resolve_prefetch(url: &str, cancel: &AtomicBool) -> Result<ResolvedTrack> {
    resolve_via(url, cancel, true)
}

fn resolve_via(url: &str, cancel: &AtomicBool, bg: bool) -> Result<ResolvedTrack> {
    // Repeats and back-navigation within TTL skip yt-dlp entirely.
    if let Some(hit) = cache_lookup(url) {
        return Ok(hit);
    }
    single_flight(resolve_flights(), url, cancel, |c| {
        // Re-check inside the flight: a previous flight may have stored
        // while we were queued behind it.
        if let Some(hit) = cache_lookup(url) {
            return Ok(hit);
        }
        do_resolve(url, c, bg)
    })
}

/// The actual extraction work (no cache, no flight — resolve_via wraps it).
/// VOD costs ONE android-client dump (~6-10s, mostly yt-dlp process startup)
/// and yields the progressive (muxed MP4, itag-18 class) URL as PRIMARY.
/// Measured 2026-09-11 on the dev machine's network: default-client
/// (ANDROID_VR) googlevideo edges reject open-ended Range requests (plain
/// GET and `bytes=0-` → 403; a closed `bytes=0-65535` → 206) and lavf/ffmpeg
/// only ever issues open-ended ranges — so default-client DASH audio can
/// never open in mpv there, while android progressive URLs redirect to
/// edges that accept them. The default-client dump is now only the LAST
/// resort for videos with no progressive format at all (then the DASH URL
/// is primary and the android URL is resolved reactively by the player on
/// first failure). Live: the android playlist is the ad-free HLS; one
/// extraction serves both cases.
fn do_resolve(url: &str, cancel: &AtomicBool, bg: bool) -> Result<ResolvedTrack> {
    let exe = find_yt_dlp()?;

    // Android client first: VOD progressive primary, live ad-free HLS.
    if let Ok(d) = android_dump(&exe, url, cancel, bg) {
        if d.is_live.unwrap_or(false) {
            if let Some(u) = pick_hls_url(&d.formats) {
                return Ok(ResolvedTrack {
                    meta: track_meta(&d),
                    primary_url: u,
                    fallback_exhausted: true,
                    is_hls: true,
                });
                // live: never cached (cache_store skips HLS)
            }
            // is_live but no m3u8 in the android dump: try the default dump below.
        } else if let Some(u) = pick_progressive(&d.formats) {
            return Ok(ResolvedTrack {
                meta: track_meta(&d),
                primary_url: u,
                // The primary already IS the fallback client's URL — a
                // reactive android re-resolve would return the same thing.
                fallback_exhausted: true,
                is_hls: false,
            })
            .inspect(|t| cache_store(url, t));
        }
        // No progressive https format → fall through to the default dump.
    }

    let json = run_dump(&exe, &["-f", "bestaudio/best"], url, cancel, bg)?;
    let d: DlpDump = serde_json::from_str(json.trim()).context("default dump not valid JSON")?;
    let meta = track_meta(&d);

    // Live playback must remain on the critical path of a single extraction.
    // The URL from this dump is a valid native HLS stream.
    if meta.is_live {
        if let Some(u) = pick_hls_url(&d.formats) {
            return Ok(ResolvedTrack {
                meta,
                primary_url: u,
                fallback_exhausted: true,
                is_hls: true,
            });
        }
        // is_live but no m3u8 anywhere: fall through to VOD tiering rather
        // than failing — mpv will surface the real outcome.
    }

    // Legacy VOD tier (videos with no progressive format): best audio-only
    // https DASH as primary, the android progressive URL resolved reactively
    // by the player on first failure (fallback_exhausted = false).
    let primary = d
        .formats
        .iter()
        .filter(|f| is_audio_only(f) && f.protocol.as_deref().unwrap_or("") == "https")
        .filter_map(|f| http_url(f).map(|u| (bitrate(f), u)))
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
        .map(|(_, u)| u.to_string())
        .ok_or_else(|| anyhow!("yt-dlp returned no audio-only https format"))?;

    Ok(ResolvedTrack {
        meta,
        primary_url: primary,
        fallback_exhausted: false,
        is_hls: false,
    })
    .inspect(|t| cache_store(url, t))
}

/// Android-client dump, parsed. Used for live HLS (ad-free playlists) and
/// the VOD tier-2 fallback — one shared code path so each call site pays
/// for the slower android extraction only when it needs it.
fn android_dump(
    exe: &str,
    url: &str,
    cancel: &AtomicBool,
    bg: bool,
) -> Result<DlpDump> {
    let json = run_dump(
        exe,
        &[
            "--extractor-args",
            "youtube:player_client=android",
            "-f",
            "best[acodec!=none][vcodec!=none]/best",
        ],
        url,
        cancel,
        bg,
    )?;
    serde_json::from_str(json.trim()).context("android dump not valid JSON")
}

/// Resolve the tier-2 progressive fallback URL from the android client.
/// Used reactively — only when a VOD with NO progressive format at all
/// (DASH primary) fails its first open — not on every load.
pub fn resolve_fallback_url(url: &str, cancel: &AtomicBool) -> Result<Option<String>> {
    let exe = find_yt_dlp()?;
    let d = android_dump(&exe, url, cancel, false)?;
    Ok(pick_progressive(&d.formats))
}

/// Android-only live HLS resolve: fresh playlist URL without the default
/// dump. Used for watchdog reconnects, where metadata is already known
/// (the empty-title guard in player.rs keeps the old title) and only a new
/// URL is needed — roughly halves reconnect reload time. If this fails, the
/// caller degrades to a full `resolve_stream` via the normal retry path.
/// Blocking — NEVER call on the mpv owner thread.
pub fn resolve_live_hls(url: &str, cancel: &AtomicBool) -> Result<ResolvedTrack> {
    let exe = find_yt_dlp()?;
    let d = android_dump(&exe, url, cancel, false)?;
    let primary =
        pick_hls_url(&d.formats).ok_or_else(|| anyhow!("android dump has no HLS format"))?;
    Ok(ResolvedTrack {
        meta: TrackMeta {
            id: String::new(),
            title: String::new(),
            channel: String::new(),
            duration: None,
            is_live: true,
            thumbnail: None,
        },
        primary_url: primary,
        fallback_exhausted: true,
        is_hls: true,
    })
}

/// m3u8 URLs are playlist fetches, not ranged media — accept any http(s) URL.
fn http_url_allow_m3u8(f: &DlpFormat) -> Option<&str> {
    f.url.as_deref().filter(|u| u.starts_with("http"))
}

/// First HLS (m3u8) URL in a format list, if any.
fn pick_hls_url(formats: &[DlpFormat]) -> Option<String> {
    formats
        .iter()
        .filter(|f| f.protocol.as_deref().unwrap_or("").contains("m3u8"))
        .find_map(http_url_allow_m3u8)
        .map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    const VOD_DUMP: &str = r#"{"id":"dQw4w9WgXcQ","title":"Never Gonna Give You Up","channel":"Rick Astley","duration":212.0,"thumbnail":"https://i.ytimg.com/vi/x/maxresdefault.jpg","formats":[
      {"format_id":"251","url":"https://rr5.googlevideo.com/v?dash-opus","protocol":"https","vcodec":"none","acodec":"opus","abr":128.0},
      {"format_id":"140","url":"https://rr5.googlevideo.com/v?dash-m4a","protocol":"https","vcodec":"none","acodec":"mp4a.40.2","abr":129.0},
      {"format_id":"18","url":"https://rr5.googlevideo.com/v?prog","protocol":"https","vcodec":"avc1","acodec":"mp4a.40.2","height":360}
    ]}"#;

    const LIVE_DUMP: &str = r#"{"id":"liveXYZ","title":"lofi radio","channel":"Lofi Girl","duration":null,"is_live":true,"formats":[
      {"format_id":"95","url":"https://example.com/live.m3u8","protocol":"m3u8_native","vcodec":"avc1","acodec":"mp4a.40.2"}
    ]}"#;

    fn parse_default(json: &str) -> Result<(TrackMeta, Vec<DlpFormat>)> {
        let d: DlpDump = serde_json::from_str(json.trim())?;
        Ok((
            TrackMeta {
                id: d.id,
                title: d.title,
                channel: d.channel.unwrap_or_default(),
                duration: d.duration,
                is_live: d.is_live.unwrap_or(false),
                thumbnail: d.thumbnail,
            },
            d.formats,
        ))
    }

    #[test]
    fn selects_best_dash_primary() {
        let (_, formats) = parse_default(VOD_DUMP).unwrap();
        let primary = formats
            .iter()
            .filter(|f| is_audio_only(f) && f.protocol.as_deref().unwrap_or("") == "https")
            .filter_map(|f| http_url(f).map(|u| (bitrate(f), u)))
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
            .map(|(_, u)| u)
            .unwrap();
        assert!(
            primary.contains("dash-m4a"),
            "want highest-abr dash, got {primary}"
        );
    }

    /// The VOD primary: cheapest progressive (android itag-18 class) — the
    /// only URL class that opens on range-gated googlevideo edges.
    #[test]
    fn selects_progressive_primary() {
        let (_, formats) = parse_default(VOD_DUMP).unwrap();
        let primary = pick_progressive(&formats).unwrap();
        assert!(primary.contains("prog"), "got {primary}");
    }

    #[test]
    fn selects_cheapest_progressive_fallback() {
        let (_, formats) = parse_default(VOD_DUMP).unwrap();
        let fb = formats
            .iter()
            .filter(|f| is_progressive(f) && f.protocol.as_deref().unwrap_or("") == "https")
            .filter_map(|f| http_url(f).map(|u| (f.height.unwrap_or(u64::MAX), u)))
            .min_by_key(|(h, _)| *h)
            .map(|(_, u)| u)
            .unwrap();
        assert!(fb.contains("prog"), "got {fb}");
    }

    #[test]
    fn live_picks_m3u8() {
        let (meta, formats) = parse_default(LIVE_DUMP).unwrap();
        assert!(meta.is_live);
        assert_eq!(meta.duration, None);
        let hls = pick_hls_url(&formats).unwrap();
        assert!(hls.ends_with(".m3u8"));
    }

    #[test]
    fn pick_hls_url_skips_non_m3u8_entries() {
        // A mixed list: the https progressive entry must not shadow the HLS one.
        let formats = vec![
            DlpFormat {
                url: Some("https://rr5.googlevideo.com/v?prog".into()),
                protocol: Some("https".into()),
                vcodec: Some("avc1".into()),
                acodec: Some("mp4a.40.2".into()),
                abr: None,
                tbr: None,
                height: Some(360),
            },
            DlpFormat {
                url: Some("https://example.com/live.m3u8".into()),
                protocol: Some("m3u8_native".into()),
                vcodec: Some("avc1".into()),
                acodec: Some("mp4a.40.2".into()),
                abr: None,
                tbr: None,
                height: None,
            },
        ];
        assert_eq!(
            pick_hls_url(&formats).as_deref(),
            Some("https://example.com/live.m3u8")
        );
        assert!(pick_hls_url(&formats[..1]).is_none());
    }

    #[test]
    fn garbage_is_error() {
        assert!(serde_json::from_str::<DlpDump>("nope").is_err());
    }

    #[test]
    fn flat_playlist_ndjson_parses() {
        // Realistic dump shape: container record first (skipped by both the
        // `entries` guard and the 11-char id filter), full entry, entry with
        // only `uploader`, entry missing title/duration, duplicate id.
        let out = r#"
{"id":"PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R","title":"My List","type":"playlist","entries":[{"id":"dQw4w9WgXcQ"}]}
{"id":"dQw4w9WgXcQ","title":"Never Gonna Give You Up","channel":"Rick Astley","duration":212.0,"thumbnail":"https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg","url":"https://www.youtube.com/watch?v=dQw4w9WgXcQ","ie_key":"Youtube"}
{"id":"JX4oZsnQhIA","title":"Ünïcødé ✓ track","uploader":"Chœur","duration":null}
{"id":"cYw-QfEYbaA","uploader":"NoTitleChan"}
{"id":"dQw4w9WgXcQ","title":"dupe of first"}
"#;
        let tracks = parse_flat_playlist(out).unwrap();
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].id, "dQw4w9WgXcQ");
        assert_eq!(tracks[0].channel, "Rick Astley");
        assert_eq!(tracks[0].duration, Some(212.0));
        assert_eq!(tracks[1].title, "Ünïcødé ✓ track");
        assert_eq!(tracks[1].channel, "Chœur");
        assert_eq!(tracks[1].duration, None);
        assert_eq!(tracks[2].title, "Video cYw-QfEYbaA");
        assert!(tracks.iter().all(|t| !t.is_live));
    }

    #[test]
    fn flat_playlist_garbage_is_error() {
        assert!(parse_flat_playlist("").is_err());
        assert!(parse_flat_playlist("not json at all").is_err());
        // Container record only -> no playable entries.
        assert!(parse_flat_playlist(
            r#"{"id":"PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R","entries":[],"type":"playlist"}"#
        )
        .is_err());
    }

    /// wait_cancel must kill a hung child as soon as the flag trips — this
    /// is what reaps superseded resolves instead of letting them pile up.
    /// Uses localhost ping as the dummy long process (Windows-only).
    #[cfg(windows)]
    #[test]
    fn cancel_kills_a_hung_child_fast() {
        use std::sync::Arc;
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            c2.store(true, Ordering::SeqCst);
        });
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 10 127.0.0.1 >NUL"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ping");
        let t = std::time::Instant::now();
        let r = wait_cancel(&mut child, &cancel, std::time::Duration::from_secs(60));
        assert!(r.is_err(), "expected superseded error, got {r:?}");
        assert!(
            t.elapsed() < std::time::Duration::from_secs(10),
            "kill took too long"
        );
    }

    /// Budget expiry (no cancel) reports Ok(None) so the caller runs its
    /// normal timeout path.
    #[cfg(windows)]
    #[test]
    fn budget_expiry_returns_none() {
        let cancel = AtomicBool::new(false);
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 4 127.0.0.1 >NUL"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ping");
        let r = wait_cancel(&mut child, &cancel, std::time::Duration::from_millis(500));
        assert!(matches!(r, Ok(None)), "expected Ok(None), got {r:?}");
    }

    /// Cache freshness window: now is fresh, older than TTL is not.
    #[test]
    fn cache_freshness_window() {
        assert!(cache_fresh(std::time::Instant::now()));
        assert!(!cache_fresh(
            std::time::Instant::now() - std::time::Duration::from_secs(CACHE_TTL_SECS + 1)
        ));
    }

    /// TTL must outlive the longest normal song so a prefetch fired ~45s
    /// before track end is still a hit when the next load resolves.
    /// 60min leaves room for a 40min+ mix while the reactive re-resolve in
    /// player.rs covers the rare expired URL.
    #[test]
    fn ttl_covers_long_tracks() {
        assert!(cache_fresh(
            std::time::Instant::now() - std::time::Duration::from_secs(30 * 60)
        ));
        assert!(CACHE_TTL_SECS >= 45 * 60);
    }

    fn test_cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    /// Two callers on the same key share ONE run of the underlying op:
    /// a play that arrives while a prefetch is mid-resolve joins it
    /// instead of spawning a second 7-10s yt-dlp dump.
    #[test]
    fn single_flight_shares_one_run() {
        let flights = Arc::new(Mutex::new(HashMap::<String, Arc<Flight<u32>>>::new()));
        let runs = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let f = flights.clone();
            let r = runs.clone();
            let c = test_cancel();
            handles.push(
                thread::Builder::new()
                    .spawn(move || {
                        single_flight(&f, "single-flight-share", &c, |_| {
                            r.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(std::time::Duration::from_millis(250));
                            Ok(42u32)
                        })
                    })
                    .unwrap(),
            );
        }
        for h in handles {
            assert_eq!(h.join().unwrap().unwrap(), 42);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1, "op must run exactly once");
        assert!(
            flights.lock().unwrap().is_empty(),
            "flight entry must be removed when done"
        );
    }

    /// A cancelled JOINER (user skipped to another track mid-wait) returns
    /// superseded WITHOUT touching the leader — the leader still finishes
    /// and fills the cache for later.
    #[test]
    fn joiner_cancel_leaves_leader_running() {
        let flights = Arc::new(Mutex::new(HashMap::<String, Arc<Flight<u32>>>::new()));
        let runs = Arc::new(AtomicUsize::new(0));
        let leader_flights = flights.clone();
        let leader_runs = runs.clone();
        let leader_cancel = test_cancel();
        let leader = thread::spawn(move || {
            single_flight(&leader_flights, "single-flight-joiner-cancel", &leader_cancel, |_| {
                runs_clone_inc(&leader_runs);
                thread::sleep(std::time::Duration::from_millis(400));
                Ok(7u32)
            })
        });
        thread::sleep(std::time::Duration::from_millis(60)); // leader registers first
        let joiner_cancel = test_cancel();
        let jf = flights.clone();
        let jc = joiner_cancel.clone();
        let joiner = thread::spawn(move || {
            single_flight(&jf, "single-flight-joiner-cancel", &jc, |_| Ok(0u32))
        });
        thread::sleep(std::time::Duration::from_millis(80));
        joiner_cancel.store(true, Ordering::SeqCst);
        let jr = joiner.join().unwrap();
        assert!(jr.is_err(), "cancelled joiner must not return a result");
        assert_eq!(leader.join().unwrap().unwrap(), 7, "leader must complete");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "cancel must not kill the leader");
    }

    fn runs_clone_inc(r: &Arc<AtomicUsize>) {
        r.fetch_add(1, Ordering::SeqCst);
    }

    /// When the LEADER is superseded (its prefetch got bumped by a newer
    /// target), it stores Gone and the joiner takes over as a fresh leader
    /// with its own cancel flag — the load never dies on a bumped prefetch.
    #[test]
    fn leader_superseded_lets_joiner_take_over() {
        let flights = Arc::new(Mutex::new(HashMap::<String, Arc<Flight<u32>>>::new()));
        let runs = Arc::new(AtomicUsize::new(0));
        let leader_flights = flights.clone();
        let leader_runs = runs.clone();
        let leader_cancel = test_cancel();
        let leader_trip = leader_cancel.clone();
        let leader = thread::spawn(move || {
            single_flight(&leader_flights, "single-flight-gone", &leader_trip, |c| {
                runs_clone_inc(&leader_runs);
                while !c.load(Ordering::SeqCst) {
                    thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(anyhow!(crate::ytdlp::SUPERSEDED_MSG))
            })
        });
        thread::sleep(std::time::Duration::from_millis(60));
        let joiner_cancel = test_cancel();
        let jf = flights.clone();
        let jr_runs = runs.clone();
        let joiner = thread::spawn(move || {
            single_flight(&jf, "single-flight-gone", &joiner_cancel, move |_| {
                runs_clone_inc(&jr_runs);
                Ok(9u32)
            })
        });
        thread::sleep(std::time::Duration::from_millis(80));
        leader_cancel.store(true, Ordering::SeqCst);
        assert_eq!(
            joiner.join().unwrap().unwrap(),
            9,
            "joiner must re-lead and get its own op's result"
        );
        let _ = leader.join();
        assert_eq!(runs.load(Ordering::SeqCst), 2, "second op must run after Gone");
    }

    /// The prefetch slot trips only the previous prefetch — it never
    /// touches the load-path resolver generations (they keep their own
    /// slot), so starting a prefetch cannot bump a playing track's load.
    #[test]
    fn prefetch_slot_trips_previous_only() {
        let a = arm_prefetch_cancel();
        let b = arm_prefetch_cancel();
        assert!(a.load(Ordering::SeqCst), "previous prefetch must be superseded");
        assert!(!b.load(Ordering::SeqCst), "fresh prefetch flag must be armed open");
    }

    /// A child demoted via set_child_below_normal really carries
    /// BELOW_NORMAL afterwards — and a control child left alone must NOT,
    /// so this test cannot pass vacuously.
    #[cfg(windows)]
    #[test]
    fn low_priority_child_is_below_normal() {
        use windows::Win32::System::Threading::BELOW_NORMAL_PRIORITY_CLASS;
        let spawn_ping = || {
            std::process::Command::new("cmd")
                .args(["/C", "ping -n 8 127.0.0.1 >NUL"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn cmd")
        };
        let mut control = spawn_ping();
        let mut demoted = spawn_ping();
        set_child_below_normal(&demoted);
        let control_class = child_priority_class(control.id());
        let demoted_class = child_priority_class(demoted.id());
        let _ = control.kill();
        let _ = control.wait();
        let _ = demoted.kill();
        let _ = demoted.wait();
        assert_eq!(
            demoted_class,
            Some(BELOW_NORMAL_PRIORITY_CLASS.0),
            "demoted child must be BELOW_NORMAL"
        );
        assert_ne!(
            control_class,
            Some(BELOW_NORMAL_PRIORITY_CLASS.0),
            "control child must not be BELOW_NORMAL"
        );
    }

    /// Store + hit roundtrip (unique keys: the cache is a shared global).
    #[test]
    fn cache_store_and_hit() {
        let track = ResolvedTrack {
            meta: TrackMeta {
                id: "x".into(),
                title: "t".into(),
                channel: "c".into(),
                duration: Some(1.0),
                is_live: false,
                thumbnail: None,
            },
            primary_url: "https://example.com/a".into(),
            fallback_exhausted: true,
            is_hls: false,
        };
        cache_store("test-cache-hit-audio-only", &track);
        assert_eq!(cache_lookup("test-cache-hit-audio-only"), Some(track));
    }

    /// Unknown keys miss.
    #[test]
    fn cache_miss_unknown_key() {
        assert!(cache_lookup("test-cache-miss-no-such-key").is_none());
    }

    #[test]
    fn finds_yt_dlp_absolute() {
        // This machine has yt-dlp via winget; the path must be absolute —
        // mpv's hook and our resolver both fail on bare names.
        let p = find_yt_dlp().unwrap();
        assert!(std::path::Path::new(&p).is_absolute(), "not absolute: {p}");
        assert!(std::path::Path::new(&p).is_file(), "not a file: {p}");
    }

    #[test]
    #[ignore] // needs network + yt-dlp — run explicitly
    fn probe_real() {
        let m = probe(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(m.id, "dQw4w9WgXcQ");
    }

    #[test]
    #[ignore] // needs network + yt-dlp — run explicitly
    fn fetch_playlist_real() {
        let t = fetch_playlist(
            "https://www.youtube.com/playlist?list=PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R",
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!t.is_empty());
        assert!(t.iter().all(|m| m.id.len() == 11));
        println!("playlist entries: {}", t.len());
    }

    #[test]
    #[ignore] // needs network + yt-dlp — run explicitly (single default dump)
    fn resolve_real() {
        let r = resolve_stream(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(r.meta.id, "dQw4w9WgXcQ");
        assert!(!r.is_hls);
        assert!(r.primary_url.starts_with("https://"));
        // VOD primary is the android progressive URL (single dump); a
        // reactive android re-resolve has nothing new to offer.
        assert!(r.fallback_exhausted);
    }

    #[test]
    #[ignore] // needs network + yt-dlp — run explicitly (android dump only, ~10s)
    fn resolve_live_fast_real() {
        let r = resolve_live_hls(
            "https://www.youtube.com/watch?v=rFZHOHl-L8A",
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(r.is_hls);
        assert!(r.primary_url.starts_with("https://"));
        println!("live fast hls: {}…", &r.primary_url[..60]);
    }

    #[test]
    #[ignore] // needs network + yt-dlp — run explicitly (android dump only, ~10s→fast path)
    fn resolve_live_real() {
        // Must come back HLS: the android-client playlist is the ad-free one.
        let r = resolve_stream(
            "https://www.youtube.com/watch?v=rFZHOHl-L8A",
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(r.meta.is_live, "expected live metadata");
        assert!(r.is_hls, "expected HLS for a live stream");
        assert!(r.primary_url.starts_with("https://"));
        println!("live hls: {}…", &r.primary_url[..60]);
    }
}
