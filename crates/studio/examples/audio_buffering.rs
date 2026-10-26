//! OBS's Low Latency Audio Buffering on the real engine: starts libobs with it on and reads
//! the setup back (`obs_get_audio_info2`), which also checks the struct matches libobs's.
//!
//!   cargo run -p ferrocast-studio --example audio_buffering -- <out dir>

use std::path::PathBuf;

use ferrocast_studio::{AudioSetup, Canvas, Paths, Studio};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;
    let paths = Paths::from_obs_app(std::path::Path::new("/Applications/OBS.app/Contents"), true);
    let canvas = Canvas { width: 1280, height: 720, fps: 30 };
    let audio = AudioSetup { low_latency: true, ..AudioSetup::default() };
    let studio = Studio::start_with(&paths, canvas, audio, &out.join("plugin-config"))?;
    let (ms, fixed) = studio.audio_buffering();
    println!("libobs {}: audio buffering {ms} ms, fixed {fixed}", studio.version());
    // libobs rounds 20 ms up to whole audio frames (1024 samples: 21 ms at 48 kHz).
    assert!(fixed && (20..=25).contains(&ms), "low latency audio buffering as OBS sets it");
    println!("ok");
    Ok(())
}
