use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use wait_timeout::ChildExt;

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
/// cost). googlevideo URLs live hours; 20min TTL is conservative.
/// Live HLS is NEVER cached: playlists go stale fast and reconnects want
/// fresh URLs. Entries are ~1KB; the map is pruned on every store.
const CACHE_TTL_SECS: u64 = 20 * 60;

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
            return Err(anyhow!("superseded by newer load"));
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

fn run_dump(exe: &str, extra: &[&str], url: &str, cancel: &AtomicBool) -> Result<String> {
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
/// Blocking — call from a background thread.
pub fn probe(url: &str, cancel: &AtomicBool) -> Result<TrackMeta> {
    let exe = find_yt_dlp()?;
    let json = run_dump(&exe, &["-f", "bestaudio/best"], url, cancel)?;
    let d: DlpDump = serde_json::from_str(json.trim())?;
    Ok(track_meta(&d))
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

/// Full resolve for playback: metadata + primary stream URL.
/// VOD costs ONE android-client dump (~6-10s) and yields the progressive
/// (muxed MP4, itag-18 class) URL as PRIMARY. Measured 2026-09-11 on the
/// dev machine's network: default-client (ANDROID_VR) googlevideo edges
/// reject open-ended Range requests (plain GET and `bytes=0-` → 403; a
/// closed `bytes=0-65535` → 206) and lavf/ffmpeg only ever issues
/// open-ended ranges — so default-client DASH audio can never open in mpv
/// there, while android progressive URLs redirect to edges that accept
/// them. The default-client dump is now only the LAST resort for videos
/// with no progressive format at all (then the DASH URL is primary and the
/// android URL is resolved reactively by the player on first failure).
/// Live: the android playlist is the ad-free HLS; one extraction serves
/// both cases. Blocking — NEVER call on the mpv owner thread.
pub fn resolve_stream(url: &str, cancel: &AtomicBool) -> Result<ResolvedTrack> {
    // Repeats and back-navigation within TTL skip yt-dlp entirely.
    if let Some(hit) = cache_lookup(url) {
        return Ok(hit);
    }
    let exe = find_yt_dlp()?;

    // Android client first: VOD progressive primary, live ad-free HLS.
    if let Ok(d) = android_dump(&exe, url, cancel) {
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

    let json = run_dump(&exe, &["-f", "bestaudio/best"], url, cancel)?;
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
fn android_dump(exe: &str, url: &str, cancel: &AtomicBool) -> Result<DlpDump> {
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
    )?;
    serde_json::from_str(json.trim()).context("android dump not valid JSON")
}

/// Resolve the tier-2 progressive fallback URL from the android client.
/// Used reactively — only when a VOD with NO progressive format at all
/// (DASH primary) fails its first open — not on every load.
pub fn resolve_fallback_url(url: &str, cancel: &AtomicBool) -> Result<Option<String>> {
    let exe = find_yt_dlp()?;
    let d = android_dump(&exe, url, cancel)?;
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
    let d = android_dump(&exe, url, cancel)?;
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
    use std::sync::atomic::{AtomicBool, Ordering};

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
