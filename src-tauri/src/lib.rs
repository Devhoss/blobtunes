pub mod player;
pub mod search;
pub mod smtc;
pub mod tray;
pub mod webview_profile;
pub mod ytdlp;

use player::{MediaHint, Player, PlayerCmd, PlayerState};
use search::{SearchClient, SearchItem};
use tauri::Manager;

#[tauri::command]
fn player_load(p: tauri::State<Player>, url: String) -> Result<(), String> {
    p.send(PlayerCmd::Load(url)).map_err(|e| e.to_string())
}
#[tauri::command]
fn player_play(p: tauri::State<Player>) -> Result<(), String> {
    p.send(PlayerCmd::Play).map_err(|e| e.to_string())
}
#[tauri::command]
fn player_pause(p: tauri::State<Player>) -> Result<(), String> {
    p.send(PlayerCmd::Pause).map_err(|e| e.to_string())
}
#[tauri::command]
fn player_stop(p: tauri::State<Player>) -> Result<(), String> {
    p.send(PlayerCmd::Stop).map_err(|e| e.to_string())
}
#[tauri::command]
fn player_seek(p: tauri::State<Player>, seconds: f64) -> Result<(), String> {
    p.send(PlayerCmd::Seek(seconds)).map_err(|e| e.to_string())
}
#[tauri::command]
fn player_set_volume(p: tauri::State<Player>, volume: u8) -> Result<(), String> {
    p.send(PlayerCmd::SetVolume(volume))
        .map_err(|e| e.to_string())
}
#[tauri::command]
fn player_get_state(p: tauri::State<Player>) -> PlayerState {
    p.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
}
/// Display metadata for the OS media session (Windows SMTC): title/channel/
/// artwork plus whether the queue has a next/previous item. The frontend owns
/// the queue, so it owns these; the Rust side owns playback state/position.
/// Display only — playback never consults it.
#[tauri::command]
fn player_smtc_meta(p: tauri::State<Player>, hint: MediaHint) -> Result<(), String> {
    p.send(PlayerCmd::SmtcMeta(hint)).map_err(|e| e.to_string())
}
#[tauri::command]
fn player_shutdown(p: tauri::State<Player>) {
    p.shutdown(); // graceful: stop mpv, drop handle on owner thread, join
}

#[tauri::command]
async fn search_youtube(
    sc: tauri::State<'_, SearchClient>,
    query: String,
) -> Result<Vec<SearchItem>, String> {
    sc.search(&query).await.map_err(|e| e.to_string())
}
#[tauri::command]
fn set_api_key(
    app: tauri::AppHandle,
    sc: tauri::State<SearchClient>,
    key: String,
) -> Result<(), String> {
    search::save_api_key(&app, &key).map_err(|e| e.to_string())?;
    sc.set_key(key.trim().to_string());
    Ok(())
}
#[tauri::command]
fn has_api_key(app: tauri::AppHandle) -> bool {
    search::load_api_key(&app).ok().flatten().is_some()
}
#[tauri::command]
async fn probe_url(url: String) -> Result<ytdlp::TrackMeta, String> {
    // spawn_blocking: yt-dlp takes seconds and must not stall the async runtime
    // Paste probes are user-initiated and rare: never-cancelled flag.
    tokio::task::spawn_blocking(move || {
        ytdlp::probe(&url, &std::sync::atomic::AtomicBool::new(false))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // MUST run before the Tauri builder creates any webview (see module docs):
    // pins the WebView2 user-data folder to an app-owned path instead of the
    // user-global env var that Windows components (SearchHost, Outlook) share.
    webview_profile::prepare();
    tauri::Builder::default()
        // First plugin: a second launch must focus the existing window, not
        // start a second WebView2 on the same profile (which wedges with the
        // same "process alive, no UI" symptom as the profile-sharing bug).
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            use tauri::Manager;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let player = Player::new(app.handle().clone())?;
            app.manage(player);
            let sc = SearchClient::new();
            if let Ok(Some(key)) = search::load_api_key(app.handle()) {
                sc.set_key(key);
            }
            app.manage(sc);
            tray::setup(app)?;
            Ok(())
        })
        .on_window_event(tray::on_window_event)
        .invoke_handler(tauri::generate_handler![
            player_load,
            player_play,
            player_pause,
            player_stop,
            player_seek,
            player_set_volume,
            player_get_state,
            player_smtc_meta,
            player_shutdown,
            search_youtube,
            set_api_key,
            has_api_key,
            probe_url,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Blobtunes");
}
