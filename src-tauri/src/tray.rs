//! Tray + close-to-tray. Filled in Task 9; stubbed now so Task 6 compiles.

use tauri::{App, Window, WindowEvent};

pub fn setup(_app: &App) -> tauri::Result<()> {
    Ok(())
}

/// Window ✕ hides instead of exiting; audio keeps playing.
pub fn intercept_close(window: &Window, event: &WindowEvent) {
    if let WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        let _ = window.hide();
    }
}
