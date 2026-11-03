//! How steady the engine's CPU is: every display captured at 1080p30 on the canvas, recorded
//! as H.264 for `seconds`, with this process's CPU read each second (100 = one core).
//!
//!   cargo run --release -p ferrocore --example cpu_soak -- [seconds] [fps] [software|color]

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use ferrocore::audio::AudioConfig;
    use ferrocore::engine::{Engine, EngineConfig, OutputKind, SceneItem, SourceKind};
    use ferrocore::screen::shareable;
    use ferrocore::video::{PixelFormat, RateControl, VideoCodec, VideoConfig};
    use ferrocore::{Bounds, Placement};

    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(40);
    let fps: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let mode = args.next().unwrap_or_default();
    let software = mode == "software";
    // "color": no capture at all, to see what the engine and encoder cost alone.
    let screens = mode != "color";

    let (displays, _) = shareable()?;
    let video = VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width: 1920,
        height: 1080,
        fps_num: fps,
        fps_den: 1,
        bitrate_kbps: 6000,
        rate: RateControl::Cbr,
        keyframe_secs: 2,
        software,
    };
    let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 160 };
    let e = Engine::new(EngineConfig { width: 1920, height: 1080, fps, ..Default::default() }, video, audio)?;
    let mut items = Vec::new();
    if !screens {
        e.set_source(1, SourceKind::Color { width: 1920, height: 1080, rgba: [20, 40, 60, 255] })?;
        items.push(SceneItem::new(1, Placement::default()));
    }
    for (i, d) in displays.iter().enumerate().filter(|_| screens) {
        let id = i as u64 + 1;
        e.set_source(id, SourceKind::Screen { display: d.id, sound: false, hide: Vec::new() })?;
        items.push(SceneItem::new(
            id,
            Placement { bounds: Bounds::Fit, bounds_width: 1920.0, bounds_height: 1080.0, ..Default::default() },
        ));
    }
    e.set_scene("Main", items);
    e.go("Main")?;
    println!("{} display(s), {fps} fps, {}", displays.len(), if software { "x264" } else { "hardware" });

    let pid = std::process::id().to_string();
    let cpu_secs = || -> f64 {
        let out = Command::new("ps").args(["-o", "time=", "-p", &pid]).output().ok();
        let t = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        // mm:ss.cc or h:mm:ss.cc
        t.split(':').fold(0.0, |a, p| a * 60.0 + p.parse::<f64>().unwrap_or(0.0))
    };
    let file = std::env::temp_dir().join("ferrocore-soak.mp4");
    let id = e.start_output(OutputKind::Record(file.clone()))?;
    std::thread::sleep(Duration::from_secs(3));
    let mut samples = Vec::new();
    let (mut last_cpu, mut last_t) = (cpu_secs(), Instant::now());
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_secs(1));
        let (c, t) = (cpu_secs(), Instant::now());
        let pct = (c - last_cpu) / t.duration_since(last_t).as_secs_f64() * 100.0;
        (last_cpu, last_t) = (c, t);
        let s = e.stats();
        println!("cpu {pct:6.1}%  frame {:5.1} ms  skipped {}  frames {}", s.frame_ms, s.skipped, s.frames);
        samples.push(pct);
    }
    e.stop_output(id)?;
    samples.sort_by(f64::total_cmp);
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let s = e.stats();
    println!(
        "CPU min {:.1} median {:.1} mean {mean:.1} p95 {:.1} max {:.1}; worst frame {:.1} ms; skipped {}; error {:?}",
        samples[0],
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
        samples[samples.len() - 1],
        s.worst_ms,
        s.skipped,
        e.error()
    );
    // FERROCORE_KEEP=<path>: keep the recording there to look at (it holds what was on screen).
    if let Some(to) = std::env::var_os("FERROCORE_KEEP") {
        let _ = std::fs::copy(&file, to);
    }
    let _ = std::fs::remove_file(&file);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {}
