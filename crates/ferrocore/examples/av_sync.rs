//! Sound against picture over a long recording: a beep every second (the engine's tone, on its
//! clock) while the picture turns white for 100 ms at the same moments; recorded for `seconds`
//! to `out`. `av_sync.py` then finds each beep and each flash and says how far apart they are
//! at the start and at the end (a drift between sound and picture shows as a change).
//!
//!   cargo run --release -p ferrocore --example av_sync -- <out.mp4> [seconds]

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ferrocore::Placement;
    use ferrocore::audio::AudioConfig;
    use ferrocore::engine::{Engine, EngineConfig, OutputKind, SceneItem, SourceKind, now_ns};
    use ferrocore::video::{PixelFormat, RateControl, VideoCodec, VideoConfig};
    use std::time::Duration;
    let mut args = std::env::args().skip(1);
    let out = std::path::PathBuf::from(args.next().ok_or("an output file")?);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let video = VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width: 320,
        height: 180,
        fps_num: 30,
        fps_den: 1,
        bitrate_kbps: 500,
        rate: RateControl::Cbr,
        keyframe_secs: 2,
        software: false,
    };
    let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
    let e = Engine::new(EngineConfig { width: 320, height: 180, fps: 30, ..Default::default() }, video, audio)?;
    let color = |v: u8| SourceKind::Color { width: 320, height: 180, rgba: [v, v, v, 255] };
    e.set_source(1, color(0))?;
    e.set_source(2, SourceKind::Tone { hz: 1000.0, every_ms: 1000, beep_ms: 100 })?;
    e.set_global(2, true);
    e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
    e.go("Main")?;
    let id = e.start_output(OutputKind::Record(out))?;
    let sleep_until = |t: u64| {
        let now = now_ns();
        if t > now {
            std::thread::sleep(Duration::from_nanos(t - now));
        }
    };
    let end = now_ns() + secs * 1_000_000_000;
    // The tone beeps at whole seconds of the engine's clock: white then, black 100 ms later.
    let mut next = (now_ns() / 1_000_000_000 + 1) * 1_000_000_000;
    while next < end {
        sleep_until(next);
        e.set_source(1, color(255))?;
        sleep_until(next + 100_000_000);
        e.set_source(1, color(0))?;
        next += 1_000_000_000;
    }
    e.stop_output(id)?;
    let s = e.stats();
    println!("recorded {} frames, {} skipped; error {:?}", s.frames, s.skipped, e.error());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {}
