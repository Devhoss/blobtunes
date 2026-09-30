use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchItem {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub thumbnail_url: String,
    pub duration: Option<f64>, // None until known; live stays None
    pub is_live: bool, // HINT ONLY from liveBroadcastContent — yt-dlp probe + mpv state are authoritative
}

// ---- response shapes (only fields we use; YouTube API is camelCase) ----
#[derive(Deserialize)]
pub struct ApiSearch {
    #[serde(default)]
    items: Vec<ApiSearchItem>,
}
#[derive(Deserialize)]
pub struct ApiSearchItem {
    id: ApiVideoId,
    snippet: ApiSnippet,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiVideoId {
    #[serde(default)]
    video_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiSnippet {
    #[serde(default)]
    title: String,
    #[serde(default)]
    channel_title: String,
    #[serde(default)]
    live_broadcast_content: String,
    #[serde(default)]
    thumbnails: ApiThumbs,
}
#[derive(Deserialize, Default)]
pub struct ApiThumbs {
    #[serde(default)]
    default: Option<ApiThumb>,
    #[serde(default)]
    medium: Option<ApiThumb>,
    #[serde(default)]
    high: Option<ApiThumb>,
}
#[derive(Deserialize)]
pub struct ApiThumb {
    url: String,
}

#[derive(Deserialize)]
pub struct ApiVideos {
    #[serde(default)]
    items: Vec<ApiVideoItem>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiVideoItem {
    id: String,
    content_details: ApiContentDetails,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiContentDetails {
    #[serde(default)]
    duration: Option<String>,
}
// NOTE: regionRestriction is intentionally NOT parsed. The Data API cannot tell us
// whether *this user* is region-blocked, and `allowed.is_some()` does NOT mean blocked.
// Playability is decided by yt-dlp probe + mpv at play time (see ytdlp.rs / player.rs).

fn pick_thumb(t: &ApiThumbs) -> String {
    t.medium
        .as_ref()
        .or(t.default.as_ref())
        .or(t.high.as_ref())
        .map(|x| x.url.clone())
        .unwrap_or_default()
}

/// Decode the HTML entities YouTube puts in some Data API snippet fields
/// (`90&#39;s`, `A &amp; B`). Runs at ingestion so the results list and the
/// player/queue path (via `toTrack`) see identical text. Unknown entities
/// are left untouched; decoding is idempotent for already-plain text.
fn decode_html_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            // Copy the whole CHARACTER, never the byte. `bytes[i] as char`
            // maps each byte of a multi-byte sequence into Latin-1, so every
            // non-ASCII title (emoji, curly quotes, em dashes, accents)
            // reached the UI as mojibake.
            let ch = s[i..].chars().next().expect("i is a char boundary");
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let rest = &s[i..];
        let Some(semi) = rest.find(';') else {
            out.push('&');
            i += 1;
            continue;
        };
        if semi > 12 {
            // longer than any entity we handle (`&#x10FFFF;` is 10 chars)
            out.push('&');
            i += 1;
            continue;
        }
        let entity = &rest[..=semi];
        let inner = &entity[1..entity.len() - 1];
        let decoded: Option<String> = if inner.starts_with('#') {
            let body = &inner[1..];
            let code: Option<u32> =
                if let Some(hex) = body.strip_prefix('x').or_else(|| body.strip_prefix('X')) {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    body.parse().ok()
                };
            code.and_then(char::from_u32).map(|c| c.to_string())
        } else {
            match inner.to_ascii_lowercase().as_str() {
                "amp" => Some("&".into()),
                "apos" => Some("'".into()),
                "gt" => Some(">".into()),
                "lt" => Some("<".into()),
                "nbsp" => Some(" ".into()),
                "quot" => Some("\"".into()),
                _ => None,
            }
        };
        match decoded {
            Some(d) => {
                out.push_str(&d);
                i += entity.len();
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

/// Pure: search.list JSON -> SearchItem list (skips non-video results).
pub fn parse_search_response(json: &str) -> Result<Vec<SearchItem>> {
    let resp: ApiSearch = serde_json::from_str(json).context("bad search response")?;
    Ok(resp
        .items
        .into_iter()
        .filter_map(|i| {
            let id = i.id.video_id?;
            Some(SearchItem {
                id: id.clone(),
                title: decode_html_entities(&i.snippet.title),
                channel: decode_html_entities(&i.snippet.channel_title),
                thumbnail_url: pick_thumb(&i.snippet.thumbnails),
                duration: None,
                is_live: i.snippet.live_broadcast_content == "live",
            })
        })
        .collect())
}

/// Pure: "PT3M12S" -> 192.0. Returns None for P0D/garbage (live sentinel).
pub fn parse_iso8601(s: &str) -> Option<f64> {
    if s == "P0D" {
        return None;
    }
    let rest = s.strip_prefix("PT")?;
    if rest.is_empty() {
        return None; // bare "PT" is not a duration
    }
    let mut num = String::new();
    let mut secs = 0.0f64;
    for c in rest.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let v: f64 = num.parse().ok()?;
        num.clear();
        secs += match c {
            'H' => v * 3600.0,
            'M' => v * 60.0,
            'S' => v,
            _ => return None,
        };
    }
    if !num.is_empty() {
        return None;
    }
    Some(secs)
}

/// Pure: join videos.list contentDetails durations into search items.
/// Does NOT filter on region — unplayable videos surface as errors from yt-dlp/mpv,
/// which are authoritative for playability (Data API is discovery-only).
pub fn merge_durations(mut items: Vec<SearchItem>, videos_json: &str) -> Result<Vec<SearchItem>> {
    let vids: ApiVideos = serde_json::from_str(videos_json).context("bad videos response")?;
    let mut by_id: HashMap<String, Option<f64>> = HashMap::new();
    for v in vids.items {
        let dur = v
            .content_details
            .duration
            .as_deref()
            .and_then(parse_iso8601);
        by_id.insert(v.id, dur);
    }
    for i in items.iter_mut() {
        if let Some(dur) = by_id.get(&i.id) {
            i.duration = if i.is_live { None } else { *dur };
        }
    }
    Ok(items)
}

// ---- API key persistence (outside repo, in the OS config dir) ----
// The file was named wavesurf.json before the Blobtunes rename; load runs a
// one-time migration, same spirit as webview_profile::prepare().
pub fn key_file(app: &tauri::AppHandle) -> Result<PathBuf> {
    use tauri::Manager;
    let dir = app.path().app_config_dir().context("no config dir")?;
    Ok(dir.join("blobtunes.json"))
}

#[derive(Deserialize)]
struct Cfg {
    youtube_api_key: Option<String>,
}

fn read_key_at(p: &std::path::Path) -> Result<Option<String>> {
    let cfg: Cfg = serde_json::from_str(&std::fs::read_to_string(p)?)?;
    Ok(cfg.youtube_api_key.filter(|k| !k.trim().is_empty()))
}

/// One-time rename migration: read the legacy file, write under the new name,
/// and only then remove the old one — any failure keeps the key under the
/// legacy path instead of losing it. Returns None (touching nothing) when the
/// new file already exists or there is no legacy file.
fn migrate_legacy_key(
    new_p: &std::path::Path,
    old_p: &std::path::Path,
) -> Option<String> {
    if new_p.exists() || !old_p.exists() {
        return None;
    }
    let key = read_key_at(old_p).ok().flatten()?;
    let json = serde_json::json!({ "youtube_api_key": key });
    std::fs::write(new_p, serde_json::to_string_pretty(&json).ok()?).ok()?;
    let _ = std::fs::remove_file(old_p);
    Some(key)
}

pub fn load_api_key(app: &tauri::AppHandle) -> Result<Option<String>> {
    use tauri::Manager;
    let p = key_file(app)?;
    if p.exists() {
        return read_key_at(&p);
    }
    let dir = app.path().app_config_dir().context("no config dir")?;
    Ok(migrate_legacy_key(&p, &dir.join("wavesurf.json")))
}

pub fn save_api_key(app: &tauri::AppHandle, key: &str) -> Result<()> {
    let p = key_file(app)?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let json = serde_json::json!({ "youtube_api_key": key.trim() });
    std::fs::write(&p, serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

// ---- live API client ----
pub struct SearchClient {
    http: reqwest::Client,
    key: Mutex<Option<String>>,
    cache: Mutex<HashMap<String, (Instant, Vec<SearchItem>)>>,
}

impl SearchClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            key: Mutex::new(None),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_key(&self, key: String) {
        *self.key.lock().unwrap() = Some(key);
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchItem>> {
        let key = self.key.lock().unwrap().clone().ok_or_else(|| {
            anyhow!("YouTube API key not configured — open Settings (⚙) in Blobtunes and paste a key from https://console.cloud.google.com/")
        })?;
        let norm = query.trim().to_lowercase();
        {
            let c = self.cache.lock().unwrap();
            if let Some((t, items)) = c.get(&norm) {
                if t.elapsed() < Duration::from_secs(600) {
                    return Ok(items.clone());
                }
            }
        }
        let search_url = format!(
            "https://www.googleapis.com/youtube/v3/search?part=snippet&type=video&maxResults=15&key={key}&q={}",
            urlencode(&norm)
        );
        let body = self
            .http
            .get(&search_url)
            .send()
            .await?
            .error_for_status()
            .context("YouTube API request failed")?
            .text()
            .await?;
        let items = parse_search_response(&body)?;
        // One cheap videos.list call (1 quota unit) for durations.
        let ids = items
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let videos_url = format!(
            "https://www.googleapis.com/youtube/v3/videos?part=contentDetails&id={ids}&key={key}"
        );
        let items = match self.http.get(&videos_url).send().await {
            Ok(r) if r.status().is_success() => {
                let vbody = r.text().await.unwrap_or_default();
                let fallback = items.clone();
                merge_durations(items, &vbody).unwrap_or(fallback)
            }
            _ => items, // durations are optional; do not fail the search
        };
        self.cache
            .lock()
            .unwrap()
            .insert(norm, (Instant::now(), items.clone()));
        Ok(items)
    }
}

pub(crate) fn urlencode(s: &str) -> String {
    // minimal percent-encoding, enough for query strings
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_FIXTURE: &str = r#"{"kind":"youtube#searchListResponse","items":[
      {"kind":"youtube#searchResult","id":{"kind":"youtube#video","videoId":"abc123XYZ_-"},
       "snippet":{"title":"Song A","channelTitle":"Chan A","liveBroadcastContent":"none",
       "thumbnails":{"medium":{"url":"https://i.ytimg.com/vi/abc123XYZ_-/mqdefault.jpg"}}}},
      {"kind":"youtube#searchResult","id":{"kind":"youtube#video","videoId":"liveVideo123"},
       "snippet":{"title":"Radio 🔴","channelTitle":"Chan B","liveBroadcastContent":"live",
       "thumbnails":{"default":{"url":"https://i.ytimg.com/vi/liveVideo123/default.jpg"}}}}
    ]}"#;

    const VIDEOS_FIXTURE: &str = r#"{"items":[
      {"id":"abc123XYZ_-","contentDetails":{"duration":"PT3M12S"}},
      {"id":"liveVideo123","contentDetails":{"duration":"P0D"}}
    ]}"#;

    #[test]
    fn parses_search_fixture() {
        let items = parse_search_response(SEARCH_FIXTURE).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "abc123XYZ_-");
        assert_eq!(items[0].title, "Song A");
        assert_eq!(items[0].channel, "Chan A");
        assert!(!items[0].is_live);
        assert_eq!(
            items[0].thumbnail_url,
            "https://i.ytimg.com/vi/abc123XYZ_-/mqdefault.jpg"
        );
        assert!(items[1].is_live);
        assert_eq!(
            items[1].thumbnail_url,
            "https://i.ytimg.com/vi/liveVideo123/default.jpg"
        );
    }

    #[test]
    fn decodes_html_entities_in_titles() {
        let json = r#"{"items":[{
          "id":{"videoId":"abc123XYZ_-"},
          "snippet":{"title":"90&#39;s Chill &amp; LoFi","channelTitle":"Chan &quot;A&quot;",
            "liveBroadcastContent":"none","thumbnails":{}}}]}"#;
        let items = parse_search_response(json).unwrap();
        assert_eq!(items[0].title, "90's Chill & LoFi");
        assert_eq!(items[0].channel, "Chan \"A\"");
    }

    #[test]
    fn keeps_non_ascii_titles_intact() {
        // Regression: the byte-at-a-time copy mapped every byte of a
        // multi-byte sequence into Latin-1, so emoji and curly quotes showed
        // up in the results list as mojibake.
        let title = "Lofi \u{1F3A7} 90\u{2019}s \u{2014} Chill";
        let json = format!(
            r#"{{"items":[{{"id":{{"videoId":"abc"}},"snippet":{{"title":"{title}","channelTitle":"c","liveBroadcastContent":"none","thumbnails":{{}}}}}}]}}"#
        );
        let items = parse_search_response(&json).unwrap();
        assert_eq!(items[0].title, title);
    }

    #[test]
    fn skips_non_video_results() {
        let channel_only = r#"{"items":[{"id":{"kind":"youtube#channel","channelId":"x"},"snippet":{"title":"t","channelTitle":"c","liveBroadcastContent":"none","thumbnails":{}}}]}"#;
        assert!(parse_search_response(channel_only).unwrap().is_empty());
    }

    #[test]
    fn iso8601_durations() {
        assert_eq!(parse_iso8601("PT3M12S"), Some(192.0));
        assert_eq!(parse_iso8601("PT1H2M3S"), Some(3723.0));
        assert_eq!(parse_iso8601("PT58S"), Some(58.0));
        assert_eq!(parse_iso8601("P0D"), None); // "live/zero" sentinel
        assert_eq!(parse_iso8601("garbage"), None);
        assert_eq!(parse_iso8601("PT"), None);
    }

    #[test]
    fn merges_durations_without_region_filtering() {
        // Region filtering was removed: the Data API cannot decide playability for this user.
        // yt-dlp probe + mpv are authoritative; unplayable items surface as errors at play time.
        let items = parse_search_response(SEARCH_FIXTURE).unwrap();
        let mut merged = merge_durations(items, VIDEOS_FIXTURE).unwrap();
        merged.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(merged.len(), 2);
        let vod = merged.iter().find(|i| i.id == "abc123XYZ_-").unwrap();
        assert_eq!(vod.duration, Some(192.0));
        let live = merged.iter().find(|i| i.id == "liveVideo123").unwrap();
        assert_eq!(live.duration, None); // PT0D->None; live stays None anyway
    }

    /// Explicit live integration test. Run with:
    ///   WAVESURF_YT_API_KEY=<key> cargo test live_search -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_search() {
        let key = std::env::var("WAVESURF_YT_API_KEY").expect("set WAVESURF_YT_API_KEY");
        let c = SearchClient::new();
        c.set_key(key);
        let items = futures_block_on(c.search("lofi hip hop radio")).expect("search failed");
        assert!(!items.is_empty(), "expected search results");
        for i in items.iter().take(3) {
            println!("{} | live={} | dur={:?}", i.title, i.is_live, i.duration);
        }
    }

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blobtunes-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn api_key_migration_renames_once() {
        let dir = unique_tmp_dir("keytest");
        let old = dir.join("wavesurf.json");
        let new = dir.join("blobtunes.json");

        std::fs::write(&old, r#"{"youtube_api_key":"AIza-TEST"}"#).unwrap();
        assert_eq!(
            migrate_legacy_key(&new, &old).as_deref(),
            Some("AIza-TEST")
        );
        assert!(!old.exists(), "legacy file must be gone after migration");
        assert_eq!(read_key_at(&new).unwrap().as_deref(), Some("AIza-TEST"));

        // Idempotent: new name exists, so a reappearing legacy file is left
        // alone and never clobbers the live one.
        std::fs::write(&old, r#"{"youtube_api_key":"AIza-STALE"}"#).unwrap();
        assert_eq!(migrate_legacy_key(&new, &old), None);
        assert_eq!(read_key_at(&new).unwrap().as_deref(), Some("AIza-TEST"));
        assert!(old.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn api_key_migration_noop_without_legacy() {
        let dir = unique_tmp_dir("keytest-none");
        let old = dir.join("wavesurf.json");
        let new = dir.join("blobtunes.json");
        assert_eq!(migrate_legacy_key(&new, &old), None);
        assert!(!new.exists());
        // Empty-key legacy file: nothing to migrate, old file untouched.
        std::fs::write(&old, r#"{"youtube_api_key":""}"#).unwrap();
        assert_eq!(migrate_legacy_key(&new, &old), None);
        assert!(old.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn futures_block_on<F: std::future::Future>(f: F) -> F::Output {
        // tiny bespoke executor: poll to completion (test-only)
        use std::task::{Context, Poll, Wake, Waker};
        struct NoopWaker;
        impl Wake for NoopWaker {
            fn wake(self: std::sync::Arc<Self>) {}
        }
        let waker = Waker::from(std::sync::Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(f);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
