//! Production-path harness: resolve a YouTube URL exactly like the app does
//! (ytdlp::resolve_stream — android-client dump: progressive primary for
//! VOD, HLS for live), then play it the way the Player does.
//!
//! Usage: play_check <youtube-url>
//! Expected VOD:  `CONFIRMED VOD: dur=...` then `CONFIRMED SEEK: pos=...`
//! Expected LIVE: `CONFIRMED LIVE` with no finite duration.

use blobtunes_lib::ytdlp;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args.get(1).expect("usage: play_check <youtube-url>");
    let t = ytdlp::resolve_stream(url, &std::sync::atomic::AtomicBool::new(false))
        .expect("resolve failed");
    println!(
        "resolved: {} | live={} | hls={} | fallback_exhausted={}",
        t.meta.title, t.meta.is_live, t.is_hls, t.fallback_exhausted
    );
    let mpv = libmpv2::Mpv::with_initializer(|init| {
        init.set_option("vo", "null")?;
        init.set_option("video", "no")?;
        // Match production: no ytdl hook on pre-resolved direct URLs.
        init.set_option("ytdl", "no")?;
        init.set_option("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")?;
        // Debug harness: always keep mpv's own log next to the run log.
        init.set_option("msg-level", "all=v")?;
        init.set_option("log-file", "E:/dev/wavesurf/playcheck-mpv.log")?;
        Ok(())
    })
    .expect("mpv init");
    mpv.enable_all_events().unwrap();
    mpv.disable_deprecated_events().unwrap();
    println!("loading primary");
    mpv.command("loadfile", &[t.primary_url.as_str(), "replace"])
        .unwrap();
    let start = std::time::Instant::now();
    let mut last_snap = 0u64;
    loop {
        // NOTE: libmpv2 reports failed EndFile as Err, not as an Event.
        if let Some(evt) = mpv.wait_event(0.5) {
            match evt {
                Err(e) => {
                    println!("FATAL: playback errored ({e})");
                    break;
                }
                Ok(ev) => {
                    println!("[{:>5.1}s] event: {ev:?}", start.elapsed().as_secs_f64());
                }
            }
        }
        let dur: f64 = mpv.get_property("duration").unwrap_or(0.0);
        let pos: f64 = mpv.get_property("time-pos").unwrap_or(0.0);
        let seekable: bool = mpv.get_property("seekable").unwrap_or(false);
        let elapsed = start.elapsed().as_secs();
        if elapsed - last_snap >= 5 {
            last_snap = elapsed;
            println!("[{elapsed}s] snap: pos={pos:.1} dur={dur:.1} seekable={seekable}");
        }
        if start.elapsed().as_secs_f64() > 150.0 {
            println!("TIMEOUT (pos={pos:.1} dur={dur:.1})");
            break;
        }
        if pos > 3.0 && seekable {
            println!("CONFIRMED VOD: dur={dur:.0}s pos={pos:.1} seekable={seekable}");
            mpv.command("seek", &["10", "absolute"]).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(2));
            let pos2: f64 = mpv.get_property("time-pos").unwrap_or(0.0);
            println!("CONFIRMED SEEK: pos={pos2:.1} (must be ~10-12)");
            break;
        }
        if pos > 3.0 && !seekable {
            println!("CONFIRMED LIVE: playing unseekable at pos={pos:.1} (dur={dur:.1})");
            break;
        }
    }
}
