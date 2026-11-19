//! Records the computer's sound through the engine (capture, mixer, AAC) for `seconds` to
//! `out` (an .mp4), to check it comes out whole: play a steady tone meanwhile and count the
//! breaks in it with `sound_breaks`.
//!
//!   cargo run --release -p ferrocore --example sound_record -- <out.mp4> [seconds] [fps]

#![allow(unsafe_code)]

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ferrocore::audio::AudioConfig;
    use ferrocore::engine::{Engine, EngineConfig, OutputKind, SceneItem, SourceKind};
    use ferrocore::video::{PixelFormat, RateControl, VideoCodec, VideoConfig};
    use ferrocore::{Placement, mix::SourceSettings};
    use std::time::Duration;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
    }
    // SAFETY: a plain CoreGraphics query (opens the window-server connection).
    let _ = unsafe { CGMainDisplayID() };
    let mut args = std::env::args().skip(1);
    let out = std::path::PathBuf::from(args.next().ok_or("an output file")?);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    let fps: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let video = VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width: 640,
        height: 360,
        fps_num: fps,
        fps_den: 1,
        bitrate_kbps: 1000,
        rate: RateControl::Cbr,
        keyframe_secs: 2,
        software: false,
    };
    let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 160 };
    let e = Engine::new(EngineConfig { width: 640, height: 360, fps, ..Default::default() }, video, audio)?;
    e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [0, 0, 0, 255] })?;
    e.set_source(2, SourceKind::DesktopSound)?;
    e.set_sound(2, SourceSettings::default());
    e.set_global(2, true);
    e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
    e.go("Main")?;
    let id = e.start_output(OutputKind::Record(out))?;
    std::thread::sleep(Duration::from_secs(secs));
    e.stop_output(id)?;
    println!("recorded; error {:?}", e.error());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {}
