//! Deterministic, app-owned WebView2 user-data folder + startup hardening.
//!
//! ROOT CAUSE (Sep 2026): WaveSurf set NO user-data folder itself and relied
//! on the user-global HKCU env var `WEBVIEW2_USER_DATA_FOLDER=E:\wavesurf-data\wv2`.
//! That env var is read by EVERY WebView2 host in the session that doesn't
//! pass an explicit folder — wry passes none — so Windows components
//! (SearchHost.exe, Outlook's olkBg.exe) were hosting msedgewebview2 in
//! WaveSurf's own profile. After a Windows restart those hosts start first
//! and hold the Chromium profile lock; `CreateCoreWebView2Environment` then
//! fails inside WaveSurf, and the native process stays alive (tray, audio
//! engine) with NO window. A fresh folder "fixing" it instantly, and GPU
//! toggles doing nothing, both match this chain.
//!
//! FIX, in three parts (see the requirements doc):
//! 1. Overwrite the env var IN THIS PROCESS with an app-owned path
//!    (`E:\wavesurf-data\wavesurf\wv2`, LOCALAPPDATA fallback) before the
//!    Tauri builder runs — deterministic, and independent of the ambient
//!    (shared) env var. The WebView2 loader appends `EBWebView` itself.
//! 2. Migrate the old profile ONCE by COPYING it to the new location,
//!    excluding transient Chromium/WebView2 state (LOCK/lockfile/Singleton*/
//!    DevToolsActivePort, rebuildable caches, Crashpad). The old profile is
//!    never touched — it stays as a backup. Copy is best-effort per entry:
//!    a locked or unreadable file is skipped, never fatal.
//! 3. Stale-lock hygiene on the ACTIVE profile before the webview exists:
//!    lock files that no live browser holds are deleted; ones still held
//!    fail the delete and are left for WebView2 to handle (never blind-delete
//!    the profile).
//!
//! Multiple WaveSurf instances launching against one profile wedge the same
//! way; `tauri-plugin-single-instance` (wired in lib.rs) closes that door.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The app-owned base directory (wry/WebView2 append `EBWebView` to it).
/// Returns the legacy shared location as the migration source candidate.
pub fn prepare() {
    let ambient = std::env::var("WEBVIEW2_USER_DATA_FOLDER").ok();
    let base = resolve_base();
    if let Err(e) = fs::create_dir_all(&base) {
        // Not fatal: without this dir WebView2 will fail later, but panicking
        // here would take down the whole app before the tray even exists.
        log(&base, &format!("create_dir_all({base:?}) failed: {e}"));
    }
    std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &base);

    let dst_eb = base.join("EBWebView");
    // Migration sources, most specific first: the folder the ambient env var
    // pointed at (if any), then the known historical location. Only one will
    // exist in practice; the first with data wins.
    let mut sources: Vec<PathBuf> = Vec::new();
    if let Some(v) = &ambient {
        if !v.is_empty() {
            sources.push(Path::new(v).join("EBWebView"));
        }
    }
    sources.push(PathBuf::from(r"E:\wavesurf-data\wv2\EBWebView"));
    for src in sources {
        if dst_eb.exists() || !src.is_dir() {
            continue;
        }
        log(&base, &format!("migrating profile {src:?} -> {dst_eb:?}"));
        if let Err(e) = copy_profile(&src, &dst_eb) {
            log(&base, &format!("migration error (continuing): {e}"));
        }
    }

    clean_stale_locks(&dst_eb, &base);
    log(&base, &format!("profile ready: {dst_eb:?}"));
}

/// App-owned, deterministic location. E: is this machine's data drive; if it
/// is missing (other machine, drive letter changed) fall back to
/// LOCALAPPDATA so the app still starts.
fn resolve_base() -> PathBuf {
    if std::fs::metadata(r"E:\").is_ok() {
        return PathBuf::from(r"E:\wavesurf-data\wavesurf\wv2");
    }
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    local.join("com.hoss.wavesurf").join("wv2")
}

/// Transient Chromium/WebView2 state that must NEVER be carried into a
/// copied profile (Nemu's root cause: a copied `LOCK` wedged the next start)
/// plus rebuildable caches that make up most of the payload.
fn is_transient(name: &str) -> bool {
    matches!(
        name,
        "LOCK"
            | "lockfile"
            | "DevToolsActivePort"
            | "Crashpad"
            | "Cache"
            | "Code Cache"
            | "GPUCache"
            | "GrShaderCache"
            | "ShaderCache"
            | "DawnGraphiteCache"
            | "DawnWebGPUCache"
            | "BrowserMetrics-spare.pma"
    ) || name.starts_with("Singleton")
        || name.ends_with(".tmp")
}

/// Copy a Chromium profile tree, skipping transient/locked entries
/// individually. The source is never modified or deleted (backup guarantee).
fn copy_profile(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_transient(&name) {
            log(
                dst,
                &format!("migration: excluded transient entry {name:?}"),
            );
            continue;
        }
        let from = entry.path();
        let to = dst.join(name.as_ref());
        if entry.file_type()?.is_dir() {
            // Per-entry best effort: one locked/unreadable subtree must not
            // abort the whole migration; Chromium recreates missing pieces.
            if let Err(e) = copy_profile(&from, &to) {
                log(dst, &format!("migration: skipped {name:?}: {e}"));
                let _ = fs::remove_dir_all(&to); // no half-copied subtrees
            }
        } else {
            if let Err(e) = fs::copy(&from, &to) {
                log(dst, &format!("migration: skipped file {name:?}: {e}"));
            }
        }
    }
    Ok(())
}

/// Delete lock artifacts that no live browser is holding. On Windows a lock
/// file held open (without FILE_SHARE_DELETE) by a running msedgewebview2
/// FAILS to delete, so a live browser is never harmed; a stale file from a
/// crash/forced shutdown deletes cleanly.
fn clean_stale_locks(eb: &Path, log_base: &Path) {
    let candidates = [
        eb.join("lockfile"),
        eb.join("LOCK"),
        eb.join("Default").join("LOCK"),
        eb.join("Default").join("lockfile"),
    ];
    for f in candidates {
        if !f.exists() {
            continue;
        }
        match fs::remove_file(&f) {
            Ok(()) => log(log_base, &format!("removed stale lock {}", f.display())),
            Err(e) => log(
                log_base,
                &format!(
                    "lock {} still held (live browser?) — left in place: {e}",
                    f.display()
                ),
            ),
        }
    }
}

/// Best-effort startup log next to the profile (append; never fatal).
fn log(base: &Path, msg: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = base.with_file_name("wv2-setup.log");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniq_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wv2-test-{tag}-{}-{}",
            std::process::id(),
            std::time::Instant::now().elapsed().as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn transient_state_is_classified() {
        assert!(is_transient("LOCK"));
        assert!(is_transient("lockfile"));
        assert!(is_transient("SingletonLock"));
        assert!(is_transient("SingletonCookie"));
        assert!(is_transient("DevToolsActivePort"));
        assert!(is_transient("Crashpad"));
        assert!(is_transient("x.tmp"));
        assert!(!is_transient("Local Storage"));
        assert!(!is_transient("Cookies"));
        assert!(!is_transient("IndexedDB"));
        assert!(!is_transient("Local State"));
    }

    #[test]
    fn migration_copies_data_excludes_locks_and_caches() {
        let root = uniq_dir("migrate");
        let src = root.join("EBWebView");
        let default = src.join("Default").join("Local Storage").join("leveldb");
        fs::create_dir_all(&default).unwrap();
        fs::write(src.join("Local State"), "state").unwrap();
        fs::write(src.join("lockfile"), "stale").unwrap();
        fs::write(src.join("Default").join("LOCK"), "stale").unwrap();
        fs::write(src.join("DevToolsActivePort"), "123").unwrap();
        fs::write(default.join("000003.log"), "userdata").unwrap();

        let dst_root = uniq_dir("migrate-dst");
        let dst = dst_root.join("EBWebView");
        copy_profile(&src, &dst).unwrap();

        assert_eq!(
            fs::read_to_string(dst.join("Local State")).unwrap(),
            "state"
        );
        assert_eq!(
            fs::read_to_string(
                dst.join("Default")
                    .join("Local Storage")
                    .join("leveldb")
                    .join("000003.log")
            )
            .unwrap(),
            "userdata"
        );
        assert!(
            !dst.join("lockfile").exists(),
            "lockfile must not be copied"
        );
        assert!(
            !dst.join("Default").join("LOCK").exists(),
            "LOCK must not be copied"
        );
        assert!(!dst.join("DevToolsActivePort").exists());
        // Source untouched (backup guarantee).
        assert!(src.join("lockfile").exists());
        assert!(src.join("Default").join("LOCK").exists());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&dst_root);
    }

    #[test]
    fn migration_skips_locked_files_without_dying() {
        let root = uniq_dir("locked");
        let src = root.join("EBWebView");
        fs::create_dir_all(src.join("good")).unwrap();
        fs::write(src.join("good").join("data"), "ok").unwrap();

        let dst_root = uniq_dir("locked-dst");
        let dst = dst_root.join("EBWebView");

        // A file held open with no sharing (as a live browser holds profile
        // files) must be SKIPPED, not abort the migration.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            fs::write(src.join("busyfile"), "x").unwrap();
            let _guard = fs::OpenOptions::new()
                .read(true)
                .share_mode(0) // no sharing -> fs::copy gets a sharing violation
                .open(src.join("busyfile"))
                .unwrap();
            copy_profile(&src, &dst).unwrap(); // must not error
            assert!(!dst.join("busyfile").exists(), "locked file skipped");
        }
        assert_eq!(
            fs::read_to_string(dst.join("good").join("data")).unwrap(),
            "ok"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&dst_root);
    }

    #[test]
    fn stale_locks_removed_live_ones_survive() {
        let root = uniq_dir("locks");
        let eb = root.join("EBWebView");
        fs::create_dir_all(eb.join("Default")).unwrap();
        fs::write(eb.join("lockfile"), "stale").unwrap();
        fs::write(eb.join("Default").join("LOCK"), "stale").unwrap();
        clean_stale_locks(&eb, &root);
        assert!(!eb.join("lockfile").exists());
        assert!(!eb.join("Default").join("LOCK").exists());

        // A held lock: open with no sharing and keep the handle while we clean.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let held = eb.join("held-lock");
            fs::write(&held, "live").unwrap();
            let _guard = fs::OpenOptions::new()
                .read(true)
                .share_mode(0) // no sharing -> delete must fail while open
                .open(&held)
                .unwrap();
            fs::write(eb.join("lockfile"), "stale2").unwrap();
            clean_stale_locks(&eb, &root);
            assert!(held.exists(), "held lock must not be deletable");
            assert!(!eb.join("lockfile").exists(), "stale lock still cleaned");
        }
        let _ = fs::remove_dir_all(&root);
    }
}
