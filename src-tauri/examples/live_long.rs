// Long-run live harness: loads a live HLS stream and logs mpv events for
// ~4 minutes so we can observe what happens around the ~1-min cutoff.
use libmpv2::{events::Event, events::PropertyData, mpv_end_file_reason, Format, Mpv};
use std::time::{Duration, Instant};

fn main() {
    let url = std::env::args().nth(1).expect("usage: live_long <url>");
    let mpv = Mpv::with_initializer(|init| {
        init.set_option("vo", "null")?;
        init.set_option("video", "no")?;
        init.set_option("audio-display", "no")?;
        init.set_option("ytdl", "no")?;
        init.set_option("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")?;
        init.set_option(
            "demuxer-lavf-o",
            "reconnect=1,reconnect_streamed=1,reconnect_delay_max=5",
        )?;
        init.set_option("demuxer-readahead-secs", "20")?;
        init.set_option("demuxer-max-bytes", "15M")?;
        Ok(())
    })
    .expect("mpv init");
    mpv.enable_all_events().unwrap();
    mpv.observe_property("time-pos", Format::Double, 1).unwrap();
    mpv.observe_property("duration", Format::Double, 2).unwrap();
    mpv.observe_property("pause", Format::Flag, 3).unwrap();
    mpv.observe_property("seekable", Format::Flag, 4).unwrap();
    mpv.observe_property("core-idle", Format::Flag, 5).unwrap();

    mpv.command("loadfile", &[&url, "replace"])
        .expect("loadfile");
    let start = Instant::now();
    let mut last_pos = 0.0;
    let mut last_event_t = Instant::now();
    println!("[  0.0s] loading {}", &url[..url.len().min(60)]);

    loop {
        let elapsed = start.elapsed().as_secs_f64();
        if elapsed > 240.0 {
            println!(
                "[{:5.1}] END pos={:.1} dur={:?} — 4min reached, still playing",
                elapsed,
                last_pos,
                mpv.get_property::<f64>("duration").ok()
            );
            break;
        }
        if let Some(evt) = mpv.wait_event(0.5) {
            match evt {
                Err(e) => println!("[{:5.1}s] EVENT ERROR: {}", elapsed, e),
                Ok(ev) => {
                    last_event_t = Instant::now();
                    match ev {
                        Event::StartFile => println!("[{:5.1}s] StartFile", elapsed),
                        Event::FileLoaded => println!("[{:5.1}s] FileLoaded", elapsed),
                        Event::EndFile(r) => {
                            let reason = if r == mpv_end_file_reason::Eof {
                                "EOF"
                            } else {
                                "OTHER"
                            };
                            println!("[{:5.1}s] EndFile({}) pos={:.1}", elapsed, reason, last_pos);
                            if r == mpv_end_file_reason::Eof {
                                break;
                            }
                        }
                        Event::PropertyChange {
                            reply_userdata,
                            change,
                            ..
                        } => match reply_userdata {
                            1 => {
                                if let PropertyData::Double(v) = change {
                                    let jump = (v - last_pos).abs();
                                    if jump > 5.0 {
                                        println!(
                                            "[{:5.1}s] time-pos JUMP {:.1} -> {:.1}",
                                            elapsed, last_pos, v
                                        );
                                    }
                                    last_pos = v;
                                }
                            }
                            2 => {
                                if let PropertyData::Double(v) = change {
                                    println!("[{:5.1}s] duration={:.1}", elapsed, v);
                                }
                            }
                            5 => {
                                if let PropertyData::Flag(true) = change {
                                    println!(
                                        "[{:5.1}s] core-idle (playback stalled?) pos={:.1}",
                                        elapsed, last_pos
                                    );
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
        }
        // silence watchdog: if no event for 30s, report
        if last_event_t.elapsed() > Duration::from_secs(30) {
            println!("[{:5.1}s] NO EVENT for 30s — pos={:.1}", elapsed, last_pos);
            last_event_t = Instant::now();
        }
    }
}
