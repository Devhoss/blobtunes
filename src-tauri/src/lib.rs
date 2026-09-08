pub mod player;
pub mod search;
pub mod tray;
pub mod ytdlp;

use player::{Player, PlayerCmd, PlayerState};
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
    p.send(PlayerCmd::SetVolume(volume)).map_err(|e| e.to_string())
}
#[tauri::command]
fn player_get_state(p: tauri::State<Player>) -> PlayerState {
    p.state.lock().unwrap().clone()
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
    tokio::task::spawn_blocking(move || ytdlp::probe(&url))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
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
        .on_window_event(|w, e| tray::intercept_close(w, e))
        .invoke_handler(tauri::generate_handler![
            player_load,
            player_play,
            player_pause,
            player_stop,
            player_seek,
            player_set_volume,
            player_get_state,
            player_shutdown,
            search_youtube,
            set_api_key,
            has_api_key,
            probe_url,
        ])
        .run(tauri::generate_context!())
        .expect("error while running wavesurf");
}
