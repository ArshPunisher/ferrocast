//! The replay buffer (keep the last seconds, save on request) and the virtual camera, against
//! an installed OBS.app (macOS). The replay buffer needs `obs-ffmpeg-mux` beside this program.
//! The virtual camera needs OBS's camera extension approved; without it, it must fail with a
//! reason.
//!
//!   cargo run -p ferrocast-studio --example replay -- <out dir>

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferrocast_studio::{CameraShows, Canvas, Collection, EncodeSettings, Paths, ReplayPlan, Studio};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    let _ = std::fs::remove_dir_all(out.join("replays"));
    std::fs::create_dir_all(out.join("replays"))?;
    let paths = Paths::from_obs_app(Path::new("/Applications/OBS.app/Contents"), false);
    let studio = Studio::start(&paths, Canvas { width: 640, height: 360, fps: 30 }, &out.join("plugin-config"))?;
    let c = Collection::new(&studio, "Live")?;
    c.add_source("Live", "color_source_v3", "Green", &json!({ "color": 0xFF_20_E0_20u32 }), true)?;
    let mut enc = EncodeSettings::with_encoder("obs_x264");
    enc.video_kbps = 1500;
    let plan = ReplayPlan {
        folder: out.join("replays"),
        name: "Replay %CCYY-%MM-%DD %hh-%mm-%ss".into(),
        format: "mkv".into(),
        spaces: true,
        seconds: 10,
        mb: 0,
        tracks: vec![1],
        encode: enc,
    };
    assert!(studio.replay_buffer(&ReplayPlan { seconds: 2, ..plan.clone() }).is_err(), "at least 5 s");
    let rb = studio.replay_buffer(&plan)?;
    assert!(rb.active());
    assert!(rb.last_replay().is_none(), "nothing saved yet");
    std::thread::sleep(Duration::from_secs(4));
    rb.save()?;
    let t0 = Instant::now();
    let file = loop {
        if let Some(f) = rb.last_replay().filter(|f| f.exists()) {
            break f;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "the replay is saved");
        std::thread::sleep(Duration::from_millis(100));
    };
    std::thread::sleep(Duration::from_millis(500));
    let probe = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(&file)
        .output()?;
    let secs: f64 = String::from_utf8_lossy(&probe.stdout).trim().parse().unwrap_or(0.0);
    println!("replay {file:?}: {secs:.1} s");
    assert!(secs > 2.5, "it holds the last seconds");
    drop(rb);

    let cam = studio.virtual_camera(&CameraShows::Program);
    println!("virtual camera (program): {:?}", cam.as_ref().map(|c| c.active()).map_err(|e| e.to_string()));
    drop(cam);
    let cam = studio.virtual_camera(&CameraShows::Source("Green".into()));
    println!("virtual camera (one source): {:?}", cam.as_ref().map(|c| c.active()).map_err(|e| e.to_string()));
    drop(cam);
    assert!(studio.virtual_camera(&CameraShows::Source("Nope".into())).is_err());
    drop(c);
    drop(studio);
    println!("replay buffer saved the last seconds; virtual camera answered");
    Ok(())
}
