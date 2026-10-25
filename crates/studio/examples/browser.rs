//! Checks OBS's own Browser source (Chromium, obs-browser) runs inside the Studio: shows a
//! page and records 4 s of it. On macOS against OBS.app (Chromium can't start outside OBS's
//! bundle there, so Ferrocast uses its WebKit source instead); on Windows against the folder
//! in `FERROCAST_OBS_DIR` (CI: OBS's portable zip).
//!
//!   cargo run -p ferrocast-studio --example browser -- <out dir> [url]

use std::path::PathBuf;
use std::time::Duration;

use ferrocast_studio::{Canvas, Paths, Studio};
use serde_json::json;

/// A page that's plainly there in a recording: orange, with a white box in the middle.
const PAGE: &str = "data:text/html,<html><body style='margin:0;background:%23ff7a00'><div style='position:absolute;left:480px;top:260px;width:320px;height:200px;background:white'></div></body></html>";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("FERROCAST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let url = args.next().unwrap_or_else(|| PAGE.into());
    std::fs::create_dir_all(&out)?;
    let paths = if cfg!(windows) {
        let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| r"C:\Program Files\obs-studio".into());
        Paths::from_obs_windows(std::path::Path::new(&dir))
    } else {
        Paths::from_obs_app(std::path::Path::new("/Applications/OBS.app/Contents"), false)
    };
    let studio = Studio::start(&paths, Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    if !studio.kinds().inputs.iter().any(|k| k.id == "browser_source") {
        // obs-browser looks for Chromium next to this program (STUDIO-PARITY, Browser source).
        return Err("no Browser source: obs-browser couldn't start Chromium here".into());
    }
    let scene = studio.scene("Browser")?;
    scene.add("browser_source", "Page", &json!({ "url": url, "width": 1280, "height": 720 }), (0.0, 0.0))?;
    studio.set_program(&scene);
    // Chromium takes a moment to start and draw.
    std::thread::sleep(Duration::from_secs(5));
    let rec = studio.record(&out.join("browser.mp4"), &ferrocast_studio::EncodeSettings::with_encoder("obs_x264"))?;
    std::thread::sleep(Duration::from_secs(4));
    rec.stop();
    drop(scene);
    drop(studio);
    println!("recorded {}", out.join("browser.mp4").display());
    Ok(())
}
