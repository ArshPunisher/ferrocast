//! The Move transition on a real engine (macOS: FERROCAST_OBS_DIR, else an installed OBS.app):
//! a camera both scenes show starts where it was, passes half way, and ends in its own place;
//! a source only the new scene has is just there; the saved collection never holds a place
//! half way.
//!
//!   cargo run -p ferrocast-studio --example move_transition -- <out dir>

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferrocast_studio::{Canvas, Collection, Paths, Studio, Transform, TransitionInfo};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;
    let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into());
    let studio = Studio::start(
        &Paths::from_obs_app(Path::new(&dir), true),
        Canvas { width: 1280, height: 720, fps: 30 },
        &out.join("plugin-config"),
    )?;
    let c = Collection::new(&studio, "Talking")?;
    c.add_scene("Gaming")?;
    let cam = json!({ "color": 0xFF_00_A0_FFu32, "width": 320, "height": 180 });
    let talking = c.add_source("Talking", "color_source_v3", "Cam", &cam, false)?;
    let gaming = c.add_existing("Gaming", "Cam")?;
    let place = |scene: &str, id: i64, x: f32, y: f32| -> Result<(), Box<dyn std::error::Error>> {
        let t = Transform { x, y, ..c.transform(scene, id)? };
        c.set_transform(scene, id, &t)?;
        Ok(())
    };
    place("Talking", talking.id, 400.0, 300.0)?;
    place("Gaming", gaming.id, 900.0, 500.0)?;
    c.add_source(
        "Gaming",
        "color_source_v3",
        "Game",
        &json!({ "color": 0xFF_20_20_20u32, "width": 64, "height": 64 }),
        false,
    )?;
    c.set_transition(&TransitionInfo { kind: "ferrocast_move".into(), ms: 600 })?;
    assert!(studio.kinds().transitions.iter().any(|k| k.id == "ferrocast_move" && k.name == "Move"), "Move is offered");

    let started = Instant::now();
    c.set_current("Gaming")?;
    assert!(c.moving(), "a Move is under way");
    let at = || c.transform("Gaming", gaming.id).map(|t| (t.x, t.y));
    assert_eq!(at()?, (400.0, 300.0), "the camera starts where it was");
    // Half way in time is half way in place (eased: slow, fast, slow).
    while started.elapsed() < Duration::from_millis(300) {
        c.tick_moves();
        std::thread::sleep(Duration::from_millis(16));
    }
    c.tick_moves();
    let (x, y) = at()?;
    assert!((550.0..750.0).contains(&x) && (350.0..450.0).contains(&y), "about half way: ({x}, {y})");
    // Saved now, the camera is in its own place in the file, and still gliding on screen.
    let saved = c.save("Test");
    assert!(c.moving(), "saving doesn't end the glide");
    let (sx, _) = at()?;
    assert!(sx < 900.0, "still on its way after saving ({sx})");
    let in_file = saved.to_string();
    assert!(in_file.contains("900.0") || in_file.contains("\"x\":900"), "saved in its own place");
    while c.tick_moves() {
        std::thread::sleep(Duration::from_millis(16));
    }
    assert_eq!(at()?, (900.0, 500.0));

    // Back again: a fresh glide, run to its end by ticks.
    c.set_current("Talking")?;
    let back = || c.transform("Talking", talking.id).map(|t| (t.x, t.y));
    assert_eq!(back()?, (900.0, 500.0), "starts where it was in Gaming");
    let t0 = Instant::now();
    while c.tick_moves() {
        std::thread::sleep(Duration::from_millis(16));
        assert!(t0.elapsed() < Duration::from_secs(2), "it ends");
    }
    assert_eq!(back()?, (400.0, 300.0), "ends in its own place");
    println!("move transition: ok");
    drop(c);
    Ok(())
}
