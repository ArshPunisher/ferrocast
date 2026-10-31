//! Recording a source on its own (OBS's "Source Record") beside what's on air, on a real engine
//! (macOS: FERROCAST_OBS_DIR, else an installed OBS.app): the source's file is its own size,
//! the program's the canvas's, both with sound. Checked with ffprobe.
//!
//!   cargo run -p ferrocast-studio --example source_record -- <out dir>

use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrocast_studio::{Canvas, Collection, EncodeSettings, Paths, RecordPlan, Studio};
use serde_json::json;

fn probe(file: &Path) -> serde_json::Value {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_streams", "-show_format"])
        .arg(file)
        .output();
    out.ok().and_then(|o| serde_json::from_slice(&o.stdout).ok()).unwrap_or_default()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    let _ = std::fs::remove_dir_all(out.join("rec"));
    std::fs::create_dir_all(out.join("rec"))?;
    let tone = out.join("tone.wav");
    if !tone.exists() {
        std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=500:duration=10", "-ac", "2"])
            .arg(&tone)
            .status()?;
    }
    let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into());
    let paths = Paths::from_obs_app(Path::new(&dir), true);
    let studio = Studio::start(&paths, Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    let c = Collection::new(&studio, "Live")?;
    c.add_source("Live", "color_source_v3", "Back", &json!({ "color": 0xFF_30_30_30u32 }), true)?;
    c.add_source(
        "Live",
        "color_source_v3",
        "Camera",
        &json!({ "color": 0xFF_00_A0_FFu32, "width": 640, "height": 360 }),
        false,
    )?;
    c.add_source(
        "Live",
        "ffmpeg_source",
        "Tone",
        &json!({ "local_file": tone.display().to_string(), "looping": true }),
        false,
    )?;
    let mut enc = EncodeSettings::with_encoder("obs_x264");
    enc.video_kbps = 1500;
    let plan = |name: &str| RecordPlan {
        folder: out.join("rec"),
        name: name.into(),
        format: "mp4".into(),
        tracks: vec![1],
        encode: enc.clone(),
        ..RecordPlan::default()
    };
    let (all, cam) = (plan("Program"), plan("Camera only"));
    let program = studio.record_to(&studio.record_path(&all), &all)?;
    let camera = studio.record_source_to("Camera", &studio.record_path(&cam), &cam)?;
    assert!(studio.record_source_to("Nobody", &out.join("rec/x.mp4"), &cam).is_err(), "an unknown source is refused");
    std::thread::sleep(Duration::from_secs(3));
    drop(camera);
    drop(program);
    for (file, w, h) in [("Program.mp4", 1280, 720), ("Camera only.mp4", 640, 360)] {
        let p = probe(&out.join("rec").join(file));
        let streams = p["streams"].as_array().cloned().unwrap_or_default();
        let video = streams.iter().find(|s| s["codec_type"] == "video").ok_or(format!("{file}: no video"))?;
        assert_eq!((video["width"].as_u64(), video["height"].as_u64()), (Some(w), Some(h)), "{file}'s size");
        assert!(streams.iter().any(|s| s["codec_type"] == "audio"), "{file} has sound");
        let secs: f64 = p["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0);
        assert!(secs > 2.0, "{file} is about 3 s long ({secs})");
        println!("{file}: {w}x{h}, {secs:.1} s, with sound");
    }
    println!("source record: ok");
    drop(c);
    Ok(())
}
