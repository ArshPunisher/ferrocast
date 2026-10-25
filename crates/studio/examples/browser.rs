//! Checks OBS's Browser source (Chromium) runs inside the Studio: shows a page and records
//! 4 s of it (macOS, against an installed OBS.app).
//!
//!   cargo run -p ferrocast-studio --example browser -- <out dir> [url]

use std::path::PathBuf;
use std::time::Duration;

use ferrocast_studio::{Canvas, Paths, Studio};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let url = args.next().unwrap_or_else(|| "https://ferrocast.clearcraftlabs.com/".into());
    std::fs::create_dir_all(&out)?;
    let paths = Paths::from_obs_app(std::path::Path::new("/Applications/OBS.app/Contents"), false);
    let studio = Studio::start(&paths, Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    if !studio.kinds().inputs.iter().any(|k| k.id == "browser_source") {
        // obs-browser looks for Chromium next to this program (STUDIO-PARITY, Browser source).
        return Err("no Browser source: obs-browser couldn't start Chromium here".into());
    }
    let scene = studio.scene("Browser")?;
    scene.add("browser_source", "Page", &json!({ "url": url, "width": 1280, "height": 720 }), (0.0, 0.0))?;
    studio.set_program(&scene);
    // Chromium takes a moment to start and draw.
    std::thread::sleep(Duration::from_secs(3));
    let rec = studio.record(&out.join("browser.mp4"), &ferrocast_studio::EncodeSettings::with_encoder("obs_x264"))?;
    std::thread::sleep(Duration::from_secs(4));
    rec.stop();
    drop(scene);
    drop(studio);
    println!("recorded {}", out.join("browser.mp4").display());
    Ok(())
}
