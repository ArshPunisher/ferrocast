//! The engine plays a clip (made here with FFmpeg: a white flash and a beep at the start of
//! every second) through our Media source and records it; then checks that every beep meets its
//! flash. The clip loops, so each loop's join is checked too.
//!
//!   cargo run --release -p ferrocast-av --example media_sync -- <out dir> [seconds]

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Duration;

    use ferrocast_av::audio::AudioConfig;
    use ferrocast_av::engine::{Engine, EngineConfig, OutputKind, SceneItem, SourceKind};
    use ferrocast_av::video::{PixelFormat, RateControl, VideoCodec, VideoConfig};
    use ferrocast_av::{Bounds, Placement};

    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(25);
    std::fs::create_dir_all(&out)?;
    let clip = out.join("flash-beep-10s.mp4");
    if !clip.exists() {
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=640x360:r=30:d=10,format=yuv420p,geq=lum='if(lt(mod(T\\,1)\\,0.1)\\,235\\,16)':cb=128:cr=128")
            .args(["-f", "lavfi", "-i"])
            .arg("aevalsrc='if(lt(mod(t\\,1)\\,0.1)\\,0.5*sin(2*PI*1000*t)\\,0)':s=48000:c=stereo:d=10")
            .args(["-c:v", "libx264", "-g", "30", "-c:a", "aac", "-shortest"])
            .arg(&clip)
            .status()?
            .success();
        if !ok {
            return Err("ffmpeg couldn't make the clip".into());
        }
    }
    let video = VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width: 640,
        height: 360,
        fps_num: 30,
        fps_den: 1,
        bitrate_kbps: 1500,
        rate: RateControl::Cbr,
        keyframe_secs: 2,
        software: false,
    };
    let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
    let e = Engine::new(EngineConfig { width: 640, height: 360, fps: 30, ..Default::default() }, video, audio)?;
    e.set_source(1, SourceKind::Media { path: clip.clone(), looping: true })?;
    e.set_scene(
        "Main",
        vec![SceneItem::new(
            1,
            Placement { bounds: Bounds::Stretch, bounds_width: 640.0, bounds_height: 360.0, ..Default::default() },
        )],
    );
    e.go("Main")?;
    let file = out.join("media-sync.mp4");
    std::thread::sleep(Duration::from_millis(500));
    let id = e.start_output(OutputKind::Record(file.clone()))?;
    std::thread::sleep(Duration::from_secs(seconds));
    e.stop_output(id)?;
    let st = e.stats();
    println!(
        "{} frames, {} skipped, slowest {:.1} ms{}",
        st.frames,
        st.skipped,
        st.worst_ms,
        e.error().map(|x| format!(", error: {x}")).unwrap_or_default()
    );

    let raw = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&file)
        .args(["-map", "0:v:0", "-vf", "scale=8:8,format=gray", "-f", "rawvideo", "-"])
        .output()?
        .stdout;
    let bright: Vec<f64> = raw.chunks(64).map(|c| c.iter().map(|v| f64::from(*v)).sum::<f64>() / 64.0).collect();
    let pcm: Vec<f32> = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&file)
        .args(["-map", "0:a:0", "-ac", "1", "-ar", "8000", "-f", "f32le", "-"])
        .output()?
        .stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let loud: Vec<f64> = pcm.chunks(8).map(|c| c.iter().map(|v| f64::from(v.abs())).fold(0.0, f64::max)).collect();
    let onsets = |v: &[f64], over: f64, step: f64| -> Vec<f64> {
        let mut prev = false;
        let mut o = Vec::new();
        for (i, x) in v.iter().enumerate() {
            let now = *x > over;
            if now && !prev {
                o.push(i as f64 * step);
            }
            prev = now;
        }
        o
    };
    let flashes = onsets(&bright, 128.0, 1000.0 / 30.0);
    let beeps = onsets(&loud, 0.1, 1.0);
    let apart: Vec<f64> = flashes
        .iter()
        .filter_map(|f| beeps.iter().map(|b| b - f).min_by(|a, b| a.abs().total_cmp(&b.abs())))
        .filter(|d| d.abs() < 400.0)
        .collect();
    let worst = apart.iter().map(|d| d.abs()).fold(0.0, f64::max);
    println!(
        "{} flashes, {} beeps; beep minus flash (ms): {apart:.0?}; worst {worst:.0} ms",
        flashes.len(),
        beeps.len()
    );
    // A frame is 33 ms: the flash can only start on one.
    if apart.len() + 3 < seconds as usize || worst > 34.0 {
        return Err("out of sync, or flashes missing".into());
    }
    println!("media_sync: OK");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("the Media source here is macOS only so far");
}
