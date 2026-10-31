//! Remove background on a real engine (macOS): a picture with a person, the background taken out
//! and then blurred, checked in screenshots kept in <out dir>.
//!
//!   cargo run -p ferrocast-studio --example cutout -- <picture with a person> <out dir> [opengl]
//!
//! FERROCAST_OBS_DIR may name the engine's Contents (else an installed OBS.app).

use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrocast_studio::{Canvas, Collection, Paths, Studio};
use serde_json::json;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let picture = PathBuf::from(args.next().ok_or("a picture with a person")?);
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let metal = args.next().as_deref() != Some("opengl");
    std::fs::create_dir_all(&out)?;
    let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into());
    let paths = Paths::from_obs_app(Path::new(&dir), metal);
    let studio = Studio::start(&paths, Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    assert!(studio.kinds().filters.iter().any(|f| f.id == "ferrocast_remove_background"), "the filter is listed");
    let c = Collection::new(&studio, "Main")?;
    c.add_source("Main", "image_source", "Camera", &json!({ "file": picture.display().to_string() }), false)?;
    let plain = shot(&c, "Camera", &out.join("before.png"))?;
    c.add_filter("Camera", "ferrocast_remove_background", "Filter", &json!({ "mode": 0 }))?;
    // Vision's first mask comes a few frames after the filter starts.
    std::thread::sleep(Duration::from_millis(1500));
    let cut = shot(&c, "Camera", &out.join("removed.png"))?;
    let (w, h) = (cut.0, cut.1);
    let alpha = |s: &(u32, u32, Vec<u8>), x: u32, y: u32| s.2[((y * s.0 + x) * 4 + 3) as usize];
    let clear = (0..w).step_by(4).map(|x| alpha(&cut, x, 2)).filter(|a| *a < 40).count();
    let middle = alpha(&cut, w / 2, h * 3 / 5);
    println!("top row clear: {clear} of {}, the middle: alpha {middle}", w.div_ceil(4));
    assert!(clear * 2 > w.div_ceil(4) as usize, "most of the top row (background) is taken out");
    assert!(middle > 200, "the person in the middle stays");
    assert!(alpha(&plain, 2, 2) > 245, "the picture was solid before");
    // Blur instead: solid everywhere again.
    c.update_filter("Camera", "Filter", &json!({ "mode": 1, "blur": 30 }))?;
    std::thread::sleep(Duration::from_millis(500));
    let blurred = shot(&c, "Camera", &out.join("blurred.png"))?;
    assert!(alpha(&blurred, 2, 2) > 245, "a blurred background stays solid");
    println!("remove background: ok on {}", if metal { "Metal" } else { "OpenGL" });
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
