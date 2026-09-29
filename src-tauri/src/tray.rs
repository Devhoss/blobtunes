use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    App, AppHandle, Manager, Theme, Window, WindowEvent,
};

/// Tray artwork: a one-colour blob silhouette with the eyes and smile cut out
/// as transparent holes. It has to contrast with the TRAY background, not with
/// our window, so the white copy is for a dark taskbar and the black one for a
/// light taskbar — the OS theme picks between them.
///
/// Embedded at compile time on purpose. `Image::from_path` reads relative to the
/// process CWD, which a portable exe can be launched with set to anywhere;
/// `include_image!` resolves from src-tauri/ and bakes the pixels into the
/// binary, so the tray can never come up blank.
const TRAY_ON_DARK: Image<'_> = tauri::include_image!("./icons/tray-white.png");
const TRAY_ON_LIGHT: Image<'_> = tauri::include_image!("./icons/tray-template.png");

/// macOS menu bars are template images: the OS paints them, so it always gets
/// the black-on-transparent artwork and permission to recolour it.
fn tray_icon(light_tray: bool) -> Image<'static> {
    if cfg!(target_os = "macos") || light_tray {
        TRAY_ON_LIGHT
    } else {
        TRAY_ON_DARK
    }
}

/// Is the OS in light mode? Read from the window, which is up before the tray
/// is built, so there is no theme-change event to wait for.
fn light_mode(app: &App) -> bool {
    matches!(
        app.get_webview_window("main").map(|w| w.theme()),
        Some(Ok(Theme::Light))
    )
}

/// Retint the tray when the user flips Windows between light and dark: the
/// white blob would vanish on a light taskbar. No restart required.
fn retint(app: &AppHandle, light_tray: bool) {
    if let Some(tray) = app.tray_by_id("tray") {
        let _ = tray.set_icon(Some(tray_icon(light_tray)));
    }
}

pub fn setup(app: &App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Blobtunes", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    let builder = TrayIconBuilder::with_id("tray")
        .icon(tray_icon(light_mode(app)))
        .tooltip("Blobtunes")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "quit" => {
                // Graceful teardown first: stop mpv + join the owner thread,
                // then exit. Fire-and-forget from the frontend is NOT enough —
                // the backend must finish dropping Mpv before the process dies.
                if let Some(p) = app.try_state::<crate::player::Player>() {
                    p.shutdown();
                }
                app.exit(0);
            }
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            _ => {}
        });
    // macOS only: "this is a mask, recolour it for the current menu bar".
    #[cfg(target_os = "macos")]
    let builder = builder.icon_as_template(true);
    builder.build(app)?;
    Ok(())
}

/// Called from lib.rs and multiplexed deliberately: the ✕ must hide instead of
/// exiting (audio keeps playing), and the theme retint arrives as a window
/// event too, so one handler owns both.
pub fn on_window_event(window: &Window, event: &WindowEvent) {
    match event {
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = window.hide();
        }
        WindowEvent::ThemeChanged(Theme::Light) => retint(window.app_handle(), true),
        WindowEvent::ThemeChanged(Theme::Dark) => retint(window.app_handle(), false),
        _ => {}
    }
}
