//! Ferrocast's Rounded corners filter on a real engine (macOS: the one in FERROCAST_OBS_DIR, else
//! an installed OBS.app; Windows: FERROCAST_OBS_DIR): a solid box with its corners rounded,
//! checked pixel by pixel in a screenshot (corners clear, edges and middle solid), on Metal and
//! OpenGL on a Mac.
//!
//!   cargo run -p ferrocast-studio --example rounded -- <out dir> [opengl]

use std::path::{Path, PathBuf};

use ferrocast_studio::{Canvas, Collection, Paths, Studio};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
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
    assert!(
        studio.kinds().filters.iter().any(|f| f.id == "ferrocast_rounded_corners" && f.name == "Rounded corners"),
        "the filter is listed"
    );
    let c = Collection::new(&studio, "Main")?;
    c.add_source(
        "Main",
        "color_source_v3",
        "Box",
        &json!({ "color": 0xFF_FF_FF_FFu32, "width": 400, "height": 300 }),
        false,
    )?;
    c.add_filter("Box", "ferrocast_rounded_corners", "Rounded corners", &json!({ "radius": 60 }))?;
    // A source's screenshot waits for libobs to draw a frame.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let shot = out.join("rounded.png");
    c.screenshot("Box", &shot)?;
    let (w, h, px) = read_png(&shot)?;
    assert_eq!((w, h), (400, 300), "the source keeps its size");
    let alpha = |x: u32, y: u32| px[((y * w + x) * 4 + 3) as usize];
    // The corner pixel is far outside the 60 px curve; a pixel 60 px in from a corner on the
    // diagonal's inside, and the middles of the edges, are inside.
    for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1), (6, 6)] {
        assert!(alpha(x, y) < 10, "corner ({x}, {y}) is clear: alpha {}", alpha(x, y));
    }
    for (x, y) in [(w / 2, 0), (0, h / 2), (w / 2, h / 2), (w - 1, h / 2), (40, 40)] {
        assert!(alpha(x, y) > 245, "({x}, {y}) is solid: alpha {}", alpha(x, y));
    }
    // Radius 0 leaves the picture as it was.
    c.update_filter("Box", "Rounded corners", &json!({ "radius": 0 }))?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    let plain = out.join("plain.png");
    c.screenshot("Box", &plain)?;
    let (_, _, px) = read_png(&plain)?;
    assert!(px[3] > 245, "radius 0 leaves the corner solid");
    println!(
        "rounded corners: ok on {}",
        if cfg!(windows) {
            "D3D11"
        } else if metal {
            "Metal"
        } else {
            "OpenGL"
        }
    );
    drop(c);
    Ok(())
}

fn read_png(path: &Path) -> Result<(u32, u32, Vec<u8>), Box<dyn std::error::Error>> {
    let mut d = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut r = d.read_info()?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    assert_eq!(info.color_type, png::ColorType::Rgba, "RGBA");
    Ok((info.width, info.height, buf))
}
