//! Edits a scene through the Studio crate against an installed OBS.app (macOS): add, read and
//! change settings, hide, fit, remove; records 2 s so the result can be looked at.
//!
//!   cargo run -p ferrocast-studio --example scene -- <out dir>

use std::path::PathBuf;
use std::time::Duration;

use ferrocast_studio::{Canvas, Paths, Studio};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;
    let paths = Paths::from_obs_app(std::path::Path::new("/Applications/OBS.app/Contents"), false);
    let studio = Studio::start(&paths, Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    let scene = studio.scene("Edit")?;
    scene.add(
        "color_source",
        "Background",
        &json!({ "color": 0xFF_20_20_20u32, "width": 640, "height": 360 }),
        (0.0, 0.0),
    )?;
    scene.fit("Background")?;
    scene.add(
        "text_ft2_source",
        "Title",
        &json!({ "text": "first", "font": { "face": "Helvetica", "size": 80 } }),
        (100.0, 300.0),
    )?;
    println!("added: {:?}", scene.sources());
    assert!(scene.add("color_source", "Title", &json!({}), (0.0, 0.0)).is_err(), "names are unique");
    println!("title settings: {}", scene.settings("Title")?);
    for o in scene.options("Title")?.iter().filter(|o| o.kind == "list") {
        println!("list {}: {:?} = {:?}", o.key, o.choices, o.values);
    }
    scene.update("Title", &json!({ "text": "changed by the Studio" }))?;
    scene.update("Background", &json!({ "color": 0xFF_A3_3F_FFu32 }))?;
    println!("after update: {}", scene.settings("Title")?["text"]);
    scene.add(
        "color_source",
        "Hidden box",
        &json!({ "color": 0xFF_00_FF_00u32, "width": 300, "height": 300 }),
        (900.0, 50.0),
    )?;
    scene.set_visible("Hidden box", false)?;
    scene.add("color_source", "Removed box", &json!({ "color": 0xFF_FF_00_00u32 }), (0.0, 0.0))?;
    scene.remove("Removed box")?;
    println!("now: {:?}", scene.sources());
    studio.set_program(&scene);
    let rec = studio.record(&out.join("scene.mp4"), "obs_x264")?;
    std::thread::sleep(Duration::from_secs(2));
    rec.stop();
    drop(scene);
    drop(studio);
    println!("closed cleanly");
    Ok(())
}
