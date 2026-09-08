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
                title: i.snippet.title.clone(),
                channel: i.snippet.channel_title.clone(),
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
        let dur = v.content_details.duration.as_deref().and_then(parse_iso8601);
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
pub fn key_file(app: &tauri::AppHandle) -> Result<PathBuf> {
    use tauri::Manager;
    let dir = app.path().app_config_dir().context("no config dir")?;
    Ok(dir.join("wavesurf.json"))
}

pub fn load_api_key(app: &tauri::AppHandle) -> Result<Option<String>> {
    let p = key_file(app)?;
    if !p.exists() {
        return Ok(None);
    }
    #[derive(Deserialize)]
    struct Cfg {
        youtube_api_key: Option<String>,
    }
    let cfg: Cfg = serde_json::from_str(&std::fs::read_to_string(p)?)?;
    Ok(cfg.youtube_api_key.filter(|k| !k.trim().is_empty()))
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
            anyhow!("YouTube API key not configured — open Settings (⚙) in Wavesurf and paste a key from https://console.cloud.google.com/")
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
