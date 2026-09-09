use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

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

/// Everything playback needs, from (usually) two yt-dlp calls merged.
/// URLs are NEVER stored in the queue — they expire. Resolved fresh per play.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTrack {
    pub meta: TrackMeta,
    /// Best quality: audio-only dash (128k opus). Works on normal networks;
    /// 403s behind range-gated edges (see below) → then `fallback_url` is used.
    pub primary_url: String,
    /// Progressive MP4 (itag-18 class) from the android client. Its URLs route
    /// to edges that honor closed ranges, so it plays where dash 403s.
    /// None when the video has no progressive format (or android call failed).
    pub fallback_url: Option<String>,
    /// True when primary is HLS (live): plays natively, no fallback applies.
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
    if let Ok(out) = std::process::Command::new("where").arg("yt-dlp").output() {
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

fn run_dump(exe: &str, extra: &[&str], url: &str) -> Result<String> {
    use std::process::Stdio;
    use std::time::Duration;
    use wait_timeout::ChildExt;
    let mut child = std::process::Command::new(exe)
        .args([
            "--no-playlist",
            "--dump-json",
            "--no-warnings",
            "--socket-timeout",
            "10",
        ])
        .args(extra)
        .args(["--", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
    let status = child
        .wait_timeout(Duration::from_secs(45))
        .map_err(|e| anyhow!("yt-dlp wait failed ({e})"))?;
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = out_thread.join();
        let _ = err_thread.join();
        return Err(anyhow!("yt-dlp timed out after 45 seconds"));
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
pub fn probe(url: &str) -> Result<TrackMeta> {
    let exe = find_yt_dlp()?;
    let json = run_dump(&exe, &["-f", "bestaudio/best"], url)?;
    let d: DlpDump = serde_json::from_str(json.trim())?;
    Ok(TrackMeta {
        id: d.id,
        title: d.title,
        channel: d.channel.unwrap_or_default(),
        duration: d.duration,
        is_live: d.is_live.unwrap_or(false),
        thumbnail: d.thumbnail,
    })
}

fn bitrate(f: &DlpFormat) -> f64 {
    f.abr.or(f.tbr).unwrap_or(0.0)
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

/// Full resolve for playback: metadata + primary stream URL.
/// VOD costs one fast default-client dump (~6-8s); the tier-2 progressive
/// fallback resolves lazily (reactively, on the first 403) via
/// `resolve_fallback_url`. Live costs a second android-client dump for the
/// HLS URL (see the live branch below for why the default one is unusable).
/// Blocking — NEVER call on the mpv owner thread.
pub fn resolve_stream(url: &str) -> Result<ResolvedTrack> {
    let exe = find_yt_dlp()?;
    let json = run_dump(&exe, &["-f", "bestaudio/best"], url)?;
    let d: DlpDump = serde_json::from_str(json.trim()).context("default dump not valid JSON")?;
    let meta = TrackMeta {
        id: d.id.clone(),
        title: d.title.clone(),
        channel: d.channel.clone().unwrap_or_default(),
        duration: d.duration,
        is_live: d.is_live.unwrap_or(false),
        thumbnail: d.thumbnail.clone(),
    };

    // Live playback must remain on the critical path of a single extraction.
    // A second client extraction adds 10–20 seconds to startup and every
    // reconnect. The URL from the primary dump is a valid native HLS stream;
    // reconnects can obtain a fresh URL through resolve_live_hls when needed.
    if meta.is_live {
        // Web playlists frequently contain ad splice dateranges that wedge
        // mpv's HLS demuxer. Use one Android-client playlist for playback;
        // metadata is still obtained from the initial probe above.
        if let Ok(android) = android_dump(&exe, url) {
            if let Some(u) = pick_hls_url(&android.formats) {
                return Ok(ResolvedTrack {
                    meta,
                    primary_url: u,
                    fallback_url: None,
                    is_hls: true,
                });
            }
        }
        if let Some(u) = pick_hls_url(&d.formats) {
            return Ok(ResolvedTrack {
                meta,
                primary_url: u,
                fallback_url: None,
                is_hls: true,
            });
        }
        // is_live but no m3u8 anywhere: fall through to VOD tiering rather
        // than failing — mpv will surface the real outcome.
    }

    // VOD tier 1: best audio-only https (best quality on normal networks).
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
        fallback_url: None, // lazy: resolved reactively by resolve_fallback_url on first 403
        is_hls: false,
    })
}

/// Android-client dump, parsed. Used for live HLS (ad-free playlists) and
/// the VOD tier-2 fallback — one shared code path so each call site pays
/// for the slower android extraction only when it needs it.
fn android_dump(exe: &str, url: &str) -> Result<DlpDump> {
    let json = run_dump(
        exe,
        &[
            "--extractor-args",
            "youtube:player_client=android",
            "-f",
            "best[acodec!=none][vcodec!=none]/best",
        ],
        url,
    )?;
    serde_json::from_str(json.trim()).context("android dump not valid JSON")
}

/// Resolve just the tier-2 progressive fallback URL from the android client.
/// Used reactively — only on the first 403 for a VOD track — not eagerly
/// on every load, so the common case never pays for android extraction.
pub fn resolve_fallback_url(url: &str) -> Result<Option<String>> {
    let exe = find_yt_dlp()?;
    let d = android_dump(&exe, url)?;
    let fallback = d
        .formats
        .iter()
        .filter(|f| is_progressive(f) && f.protocol.as_deref().unwrap_or("") == "https")
        .filter_map(|f| http_url(f).map(|u| (f.height.unwrap_or(u64::MAX), u)))
        .min_by_key(|(h, _)| *h)
        .map(|(_, u)| u.to_string());
    Ok(fallback)
}

/// Android-only live HLS resolve: fresh playlist URL without the default
/// dump. Used for watchdog reconnects, where metadata is already known
/// (the empty-title guard in player.rs keeps the old title) and only a new
/// URL is needed — roughly halves reconnect reload time. If this fails, the
/// caller degrades to a full `resolve_stream` via the normal retry path.
/// Blocking — NEVER call on the mpv owner thread.
pub fn resolve_live_hls(url: &str) -> Result<ResolvedTrack> {
    let exe = find_yt_dlp()?;
    let d = android_dump(&exe, url)?;
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
        fallback_url: None,
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
        let (meta, formats) = parse_default(VOD_DUMP).unwrap();
        assert_eq!(meta.channel, "Rick Astley");
        assert!(!meta.is_live);
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
        let m = probe("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
        assert_eq!(m.id, "dQw4w9WgXcQ");
    }

    #[test]
    #[ignore] // needs network + yt-dlp — run explicitly (single default dump)
    fn resolve_real() {
        let r = resolve_stream("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
        assert_eq!(r.meta.id, "dQw4w9WgXcQ");
        assert!(!r.is_hls);
        assert!(r.primary_url.starts_with("https://"));
        // Lazy tier-2: no eager android dump anymore — the fallback resolves
        // reactively on the first 403 instead.
        assert!(r.fallback_url.is_none());
    }

    #[test]
    #[ignore] // needs network + yt-dlp — run explicitly (android dump only, ~10s)
    fn resolve_live_fast_real() {
        let r = resolve_live_hls("https://www.youtube.com/watch?v=rFZHOHl-L8A").unwrap();
        assert!(r.is_hls);
        assert!(r.primary_url.starts_with("https://"));
        println!("live fast hls: {}…", &r.primary_url[..60]);
    }

    #[test]
    #[ignore] // needs network + yt-dlp — run explicitly (default + android dumps, ~20s)
    fn resolve_live_real() {
        // Must come back HLS: the android-client playlist is the ad-free one.
        let r = resolve_stream("https://www.youtube.com/watch?v=rFZHOHl-L8A").unwrap();
        assert!(r.meta.is_live, "expected live metadata");
        assert!(r.is_hls, "expected HLS for a live stream");
        assert!(r.primary_url.starts_with("https://"));
        assert!(r.fallback_url.is_none());
        println!("live hls: {}…", &r.primary_url[..60]);
    }
}
