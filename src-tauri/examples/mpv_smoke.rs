fn main() {
    let _mpv = libmpv2::Mpv::new().expect("mpv init");
    println!("libmpv OK, client API: {}", libmpv2::MPV_CLIENT_API_VERSION);
}
