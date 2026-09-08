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
    let out = std::process::Command::new(exe)
        .args(["--no-playlist", "--dump-json"])
        .args(extra)
        .args(["--", url])
        .output()
        .map_err(|e| anyhow!("failed to run yt-dlp ({e})"))?;
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

/// Full resolve for playback: metadata + tiered stream URLs.
/// Runs the default-client and android-client dumps CONCURRENTLY (~12s wall).
/// Blocking — NEVER call on the mpv owner thread.
pub fn resolve_stream(url: &str) -> Result<ResolvedTrack> {
    let exe = find_yt_dlp()?;
    let url_owned = url.to_string();
    let (def, and) = std::thread::scope(|s| {
        let exe2 = exe.clone();
        let url2 = url_owned.clone();
        let h_default = s.spawn(move || run_dump(&exe, &["-f", "bestaudio/best"], &url_owned));
        let h_android = s.spawn(move || {
            run_dump(
                &exe2,
                &[
                    "--extractor-args",
                    "youtube:player_client=android",
                    "-f",
                    "best[acodec!=none][vcodec!=none]/best",
                ],
                &url2,
            )
        });
        (h_default.join().unwrap(), h_android.join().unwrap())
    });
    let def_json = def?;
    let d: DlpDump =
        serde_json::from_str(def_json.trim()).context("default dump not valid JSON")?;
    let meta = TrackMeta {
        id: d.id.clone(),
        title: d.title.clone(),
        channel: d.channel.clone().unwrap_or_default(),
        duration: d.duration,
        is_live: d.is_live.unwrap_or(false),
        thumbnail: d.thumbnail.clone(),
    };

    // Live: first HLS URL from the default dump plays natively.
    if meta.is_live {
        if let Some(u) = d
            .formats
            .iter()
            .filter(|f| f.protocol.as_deref().unwrap_or("").contains("m3u8"))
            .find_map(http_url_allow_m3u8)
        {
            return Ok(ResolvedTrack {
                meta,
                primary_url: u.to_string(),
                fallback_url: None,
                is_hls: true,
            });
        }
        // is_live but no m3u8 in dump: fall through to VOD tiering rather
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

    // VOD tier 2: progressive MP4 from the android dump (range-clean edges).
    // The android call failing must NOT fail the whole resolve — dash alone
    // works on normal networks.
    let fallback = and
        .ok()
        .and_then(|j| serde_json::from_str::<DlpDump>(j.trim()).ok())
        .and_then(|a| {
            a.formats
                .iter()
                .filter(|f| is_progressive(f) && f.protocol.as_deref().unwrap_or("") == "https")
                .filter_map(|f| http_url(f).map(|u| (f.height.unwrap_or(u64::MAX), u)))
                .min_by_key(|(h, _)| *h)
                .map(|(_, u)| u.to_string())
        });

    Ok(ResolvedTrack {
        meta,
        primary_url: primary,
        fallback_url: fallback,
        is_hls: false,
    })
}

/// m3u8 URLs are playlist fetches, not ranged media — accept any http(s) URL.
fn http_url_allow_m3u8(f: &DlpFormat) -> Option<&str> {
    f.url.as_deref().filter(|u| u.starts_with("http"))
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
        let hls = formats
            .iter()
            .filter(|f| f.protocol.as_deref().unwrap_or("").contains("m3u8"))
            .find_map(http_url_allow_m3u8)
            .unwrap();
        assert!(hls.ends_with(".m3u8"));
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
    #[ignore] // needs network + yt-dlp — run explicitly (~25s, two dumps)
    fn resolve_real() {
        let r = resolve_stream("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
        assert_eq!(r.meta.id, "dQw4w9WgXcQ");
        assert!(!r.is_hls);
        assert!(r.primary_url.starts_with("https://"));
        println!("primary: {}…", &r.primary_url[..60]);
        println!(
            "fallback: {:?}",
            r.fallback_url.as_deref().map(|u| &u[..60])
        );
        assert!(
            r.fallback_url.is_some(),
            "expected android progressive fallback"
        );
    }
}
