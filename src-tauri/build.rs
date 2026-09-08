use std::path::PathBuf;

fn main() {
    tauri_build::build();

    // Wavesurf: libmpv link setup.
    // libmpv2-sys only emits `cargo:rustc-link-lib=mpv`, so WE must put a
    // compatible `mpv.lib` on the native search path. The shinchiro mpv-dev
    // archive ships a MinGW import lib (`libmpv.dll.a`); rust-lld (our linker —
    // this machine has no MSVC link.exe) accepts it when presented as mpv.lib.
    // `tools/fetch-mpv.ps1` + `tools/copy-dll.sh` produce these artifacts.
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mpv_dir = manifest
        .join("..")
        .join("tools")
        .join("mpv")
        .join("extracted");
    if mpv_dir.join("mpv.lib").exists() {
        println!(
            "cargo:rustc-link-search=native={}",
            mpv_dir.canonicalize().unwrap().display()
        );
    } else {
        println!(
            "cargo:warning=mpv.lib not found in {} — run tools/fetch-mpv.ps1 first",
            mpv_dir.display()
        );
    }
}
