//! Our engine opening an OBS scene collection (ENGINE.md E7): scenes built in libobs, saved as
//! OBS saves them, opened by our engine, and every scene drawn by both and compared.
//!
//!   cargo run -p ferrocast-studio --example obs_import -- <out dir>
//!
//! Text isn't compared (the two engines lay fonts out their own ways).

use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrocast_av::audio::AudioConfig;
use ferrocast_av::engine::{Engine, EngineConfig};
use ferrocast_av::video::{PixelFormat, RateControl, VideoCodec, VideoConfig};
use ferrocast_studio::{Canvas, Collection, ItemEdit, Paths, Studio};
use serde_json::json;

const W: u32 = 1280;
const H: u32 = 720;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;
    let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into());
    let studio = Studio::start(
        &Paths::from_obs_app(Path::new(&dir), true),
        Canvas { width: W, height: H, fps: 30 },
        &out.join("plugin-config"),
    )?;
    let c = Collection::new(&studio, "Main")?;
    c.add_source(
        "Main",
        "color_source_v3",
        "Back",
        &json!({ "color": 0xFF_40_20_10u32, "width": 1280, "height": 720 }),
        false,
    )?;
    let shot = out.join("shot.png");
    image::save_buffer(&shot, &[200u8, 60, 30, 255].repeat(64 * 64), 64, 64, image::ExtendedColorType::Rgba8)?;
    let i = c.add_source("Main", "image_source", "Pic", &json!({ "file": shot.display().to_string() }), false)?;
    let mut t = c.transform("Main", i.id)?;
    (t.x, t.y, t.rotation, t.scale_x, t.scale_y) = (300.0, 200.0, 15.0, 3.0, 2.0);
    c.set_transform("Main", i.id, &t)?;
    c.edit_item("Main", i.id, ItemEdit::BlendScreen)?;
    c.add_filter("Pic", "ferrocast_rounded_corners", "Round", &json!({ "radius": 12 }))?;

    // A second scene: a ramp picture cropped and fitted in a box, colour-corrected and keyed.
    c.add_scene("Keyed")?;
    c.add_source(
        "Keyed",
        "color_source_v3",
        "Floor",
        &json!({ "color": 0xFF_20_60_20u32, "width": 1280, "height": 720 }),
        false,
    )?;
    let ramp = out.join("ramp.png");
    let px: Vec<u8> = (0..240u32)
        .flat_map(|y| {
            (0..320u32)
                .flat_map(move |x| [(x * 255 / 319) as u8, (y * 255 / 239) as u8, 128, if x > 240 { 128 } else { 255 }])
        })
        .collect();
    image::save_buffer(&ramp, &px, 320, 240, image::ExtendedColorType::Rgba8)?;
    let r = c.add_source("Keyed", "image_source", "Ramp", &json!({ "file": ramp.display().to_string() }), false)?;
    let mut t = c.transform("Keyed", r.id)?;
    (t.x, t.y, t.bounds, t.bounds_width, t.bounds_height) = (100.0, 80.0, "fit".into(), 900.0, 500.0);
    (t.crop_left, t.crop_top, t.crop_right) = (20, 10, 30);
    c.set_transform("Keyed", r.id, &t)?;
    c.add_filter("Ramp", "color_filter_v2", "Grade", &json!({ "gamma": 0.3, "contrast": 0.2, "saturation": -0.4 }))?;
    c.add_filter("Ramp", "luma_key_filter_v2", "Luma", &json!({ "luma_min": 0.1, "luma_min_smooth": 0.05 }))?;

    // A third: Main inside it at half size, next to a copy of the picture.
    c.add_scene("Nest")?;
    c.add_source(
        "Nest",
        "color_source_v3",
        "Night",
        &json!({ "color": 0xFF_10_10_10u32, "width": 1280, "height": 720 }),
        false,
    )?;
    let n = c.add_existing("Nest", "Main")?;
    let mut t = c.transform("Nest", n.id)?;
    (t.x, t.y, t.scale_x, t.scale_y) = (40.0, 40.0, 0.5, 0.5);
    c.set_transform("Nest", n.id, &t)?;
    let p = c.add_existing("Nest", "Pic")?;
    let mut t = c.transform("Nest", p.id)?;
    (t.x, t.y) = (900.0, 400.0);
    c.set_transform("Nest", p.id, &t)?;
    // A group of two pictures, moved and turned as one, in a fourth scene.
    c.add_scene("Grouped")?;
    c.add_source(
        "Grouped",
        "color_source_v3",
        "Sky",
        &json!({ "color": 0xFF_60_30_00u32, "width": 1280, "height": 720 }),
        false,
    )?;
    let a = c.add_existing("Grouped", "Pic")?;
    let b = c.add_existing("Grouped", "Ramp")?;
    let mut t = c.transform("Grouped", b.id)?;
    (t.x, t.y) = (200.0, 120.0);
    c.set_transform("Grouped", b.id, &t)?;
    let g = c.group_together("Grouped", &[a.id, b.id], "Pair")?;
    let mut t = c.transform("Grouped", g.id)?;
    (t.x, t.y, t.rotation, t.scale_x, t.scale_y) = (500.0, 150.0, -10.0, 1.2, 1.2);
    c.set_transform("Grouped", g.id, &t)?;
    std::thread::sleep(Duration::from_millis(500));

    let doc = c.save("Test");
    std::fs::write(out.join("collection.json"), serde_json::to_string_pretty(&doc)?)?;

    // Our engine opens it, and each scene is drawn by both.
    let video = VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width: W,
        height: H,
        fps_num: 30,
        fps_den: 1,
        bitrate_kbps: 4000,
        rate: RateControl::Cbr,
        keyframe_secs: 2,
    };
    let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
    let engine = Engine::new(EngineConfig { width: W, height: H, fps: 30, ..Default::default() }, video, audio)?;
    let skipped = engine.load_obs(&doc);
    println!("opened: scenes {:?}; couldn't bring over: {skipped:?}", engine.scene_names());
    std::thread::sleep(Duration::from_millis(500));
    let total = (W * H) as usize;
    let mut failed = Vec::new();
    for scene in ["Main", "Keyed", "Nest", "Grouped"] {
        let ours = engine.snapshot_scene(scene)?;
        let file = out.join(format!("{scene} obs.png"));
        c.screenshot(scene, &file)?;
        let obs = image::open(&file)?.to_rgba8();
        image::save_buffer(out.join(format!("{scene} ours.png")), &ours, W, H, image::ExtendedColorType::Rgba8)?;
        let (mut off, mut worst) = (0usize, 0u8);
        for (a, b) in ours.chunks(4).zip(obs.as_raw().chunks(4)) {
            let d = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
            worst = worst.max(d);
            off += usize::from(d > 2);
        }
        println!(
            "obs_import {scene}: {off} of {total} pixels differ by more than 2 ({:.3}%), worst {worst}",
            off as f64 * 100.0 / total as f64
        );
        if off * 1000 > total {
            failed.push(scene);
        }
    }
    if failed.is_empty() { Ok(()) } else { Err(format!("scenes that came over differently: {failed:?}").into()) }
}
