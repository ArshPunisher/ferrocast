//! Remove background on a real engine: a picture with a person, the background taken out and then
//! blurred, checked in screenshots kept in <out dir>. `none` for the picture checks a plain one
//! instead (no person: all of it is taken out), which needs no photo (Windows CI).
//!
//!   cargo run -p ferrocast-studio --example cutout -- <picture with a person | none> <out dir> [opengl]
//!
//! FERROCAST_OBS_DIR names the engine (macOS: its Contents, else an installed OBS.app). Where RVM
//! finds the person (Windows, or `--features onnx-matte`), FERROCAST_MATTE_MODEL may name its model.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrocast_studio::{Canvas, Collection, Paths, Studio};
use serde_json::json;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let picture = args.next().ok_or("a picture with a person, or none")?;
    let nobody = picture == "none";
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let metal = args.next().as_deref() != Some("opengl");
    std::fs::create_dir_all(&out)?;
    let paths = if cfg!(windows) {
        let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| r"C:\Program Files\obs-studio".into());
        Paths::from_obs_windows(Path::new(&dir))
    } else {
        let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into());
        Paths::from_obs_app(Path::new(&dir), metal)
    };
    let studio = Studio::start(&paths, Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    assert!(studio.kinds().filters.iter().any(|f| f.id == "ferrocast_remove_background"), "the filter is listed");
    let c = Collection::new(&studio, "Main")?;
    if nobody {
        let grey = json!({ "color": 0xFF_80_80_80u32, "width": 640, "height": 360 });
        c.add_source("Main", "color_source_v3", "Camera", &grey, false)?;
    } else {
        c.add_source("Main", "image_source", "Camera", &json!({ "file": picture }), false)?;
    }
    let plain = shot(&c, "Camera", &out.join("before.png"))?;
    c.add_filter("Camera", "ferrocast_remove_background", "Filter", &json!({ "mode": 0 }))?;
    // The first mask comes a few frames after the filter starts (RVM's first after its model
    // is made for the picture's size).
    std::thread::sleep(Duration::from_millis(if cfg!(windows) { 4000 } else { 1500 }));
    let cut = shot(&c, "Camera", &out.join("removed.png"))?;
    let (w, h) = (cut.0, cut.1);
    let alpha = |s: &(u32, u32, Vec<u8>), x: u32, y: u32| s.2[((y * s.0 + x) * 4 + 3) as usize];
    let clear = (0..w).step_by(4).map(|x| alpha(&cut, x, 2)).filter(|a| *a < 40).count();
    let middle = alpha(&cut, w / 2, h * 3 / 5);
    println!("top row clear: {clear} of {}, the middle: alpha {middle}", w.div_ceil(4));
    assert!(clear * 2 > w.div_ceil(4) as usize, "most of the top row (background) is taken out");
    if nobody {
        assert!(middle < 40, "with nobody there, the middle goes too");
    } else {
        assert!(middle > 200, "the person in the middle stays");
    }
    assert!(alpha(&plain, 2, 2) > 245, "the picture was solid before");
    // Blur instead: solid everywhere again.
    c.update_filter("Camera", "Filter", &json!({ "mode": 1, "blur": 30 }))?;
    std::thread::sleep(Duration::from_millis(500));
    let blurred = shot(&c, "Camera", &out.join("blurred.png"))?;
    assert!(alpha(&blurred, 2, 2) > 245, "a blurred background stays solid");
    let on = if cfg!(windows) {
        "D3D11"
    } else if metal {
        "Metal"
    } else {
        "OpenGL"
    };
    println!("remove background: ok on {on}");
    drop(c);
    Ok(())
}

fn shot(c: &Collection, name: &str, path: &Path) -> Result<(u32, u32, Vec<u8>)> {
    std::thread::sleep(Duration::from_millis(250));
    c.screenshot(name, path)?;
    let mut d = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut r = d.read_info()?;
    let mut px = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut px)?;
    px.truncate(info.buffer_size());
    Ok((info.width, info.height, px))
}
