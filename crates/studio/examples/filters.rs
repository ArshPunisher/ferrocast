//! Ferrocast's own picture filters on a real engine (macOS: the one in FERROCAST_OBS_DIR, else an
//! installed OBS.app; Windows: FERROCAST_OBS_DIR), each checked pixel by pixel in a screenshot:
//! Rounded corners, Border, Shadow and glow, Shape, Blur or pixelate an area, Colour looks. On a
//! Mac on Metal, or OpenGL with `opengl`. Screenshots are kept in <out dir>.
//!
//!   cargo run -p ferrocast-studio --example filters -- <out dir> [opengl]

use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrocast_studio::{Canvas, Collection, Paths, Studio};
use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// A screenshot: width, height and RGBA pixels.
struct Shot {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Shot {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.w + x) * 4) as usize;
        [self.px[i], self.px[i + 1], self.px[i + 2], self.px[i + 3]]
    }
}

fn main() -> Result<()> {
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
    let kinds = studio.kinds();
    for id in [
        "ferrocast_rounded_corners",
        "ferrocast_border",
        "ferrocast_shadow",
        "ferrocast_shape",
        "ferrocast_blur_area",
        "ferrocast_look",
    ] {
        assert!(kinds.filters.iter().any(|f| f.id == id), "{id} is listed");
    }
    // Stripes 4 px wide, black and white, to see blur and pixelate act.
    let stripes = out.join("stripes.png");
    write_stripes(&stripes, 400, 300)?;
    let c = Collection::new(&studio, "Main")?;
    let white = json!({ "color": 0xFF_FF_FF_FFu32, "width": 400, "height": 300 });
    // 0xAABBGGRR, as libobs keeps colours: pure red.
    let red = json!({ "color": 0xFF_00_00_FFu32, "width": 400, "height": 300 });
    let add = |name: &str, settings: &Value, kind: &str, filter: &Value| -> Result<Shot> {
        if name == "Stripes" {
            c.add_source("Main", "image_source", name, &json!({ "file": stripes.display().to_string() }), false)?;
        } else {
            c.add_source("Main", "color_source_v3", name, settings, false)?;
        }
        c.add_filter(name, kind, "Filter", filter)?;
        shot(&c, name, &out)
    };

    // Rounded corners: the corners clear, the edges' middles and the middle solid.
    let s = add("Rounded", &white, "ferrocast_rounded_corners", &json!({ "radius": 60 }))?;
    assert_eq!((s.w, s.h), (400, 300));
    for (x, y) in [(0, 0), (399, 0), (0, 299), (399, 299), (6, 6)] {
        assert!(s.at(x, y)[3] < 10, "rounded: corner ({x}, {y}) is clear");
    }
    for (x, y) in [(200, 0), (0, 150), (200, 150), (40, 40)] {
        assert!(s.at(x, y)[3] > 245, "rounded: ({x}, {y}) is solid");
    }

    // Border: red, 10 px inside the edge; white inside it.
    let s = add("Bordered", &white, "ferrocast_border", &json!({ "width": 10, "color": 0xFF_00_00_FFu32 }))?;
    let [r, g, b, a] = s.at(4, 150);
    assert!(r > 220 && g < 40 && b < 40 && a > 245, "border: the edge is red ({r}, {g}, {b}, {a})");
    let [r, g, b, _] = s.at(200, 150);
    assert!(r > 245 && g > 245 && b > 245, "border: the middle stays white");

    // Shadow: the picture grows by softness + offset on every side; dark below the box,
    // nothing far from it, the box itself still white.
    let s = add(
        "Shadowed",
        &white,
        "ferrocast_shadow",
        &json!({ "blur": 20, "x": 0, "y": 12, "color": 0xB3_00_00_00u32 }),
    )?;
    let pad = 32;
    assert_eq!((s.w, s.h), (400 + 2 * pad, 300 + 2 * pad), "shadow: room for the shadow");
    let [r, _, _, a] = s.at(s.w / 2, pad + 300 + 6);
    assert!(a > 60 && r < 40, "shadow: dark under the box (r {r}, a {a})");
    assert!(s.at(1, 1)[3] < 10, "shadow: nothing in the far corner");
    let [r, g, b, a] = s.at(s.w / 2, s.h / 2);
    assert!(r > 245 && g > 245 && b > 245 && a > 245, "shadow: the box is untouched");

    // Shape: a circle fitted in the middle of 400×300 (radius 150 round 200,150).
    let s = add("Circle", &white, "ferrocast_shape", &json!({ "shape": 0 }))?;
    assert!(s.at(200, 150)[3] > 245 && s.at(60, 150)[3] > 245, "shape: inside the circle");
    assert!(s.at(5, 150)[3] < 10 && s.at(5, 5)[3] < 10 && s.at(340, 40)[3] < 10, "shape: outside it");
    // Stretched, it's an ellipse to the edges.
    c.update_filter("Circle", "Filter", &json!({ "shape": 0, "stretch": true }))?;
    let s = shot(&c, "Circle", &out)?;
    assert!(s.at(5, 150)[3] > 245, "shape: stretched to the left edge");
    // Every other shape: solid in the middle, clear in the corner.
    for shape in 1..=4 {
        c.update_filter("Circle", "Filter", &json!({ "shape": shape, "stretch": false }))?;
        let s = shot(&c, "Circle", &out)?;
        std::fs::copy(out.join("Circle.png"), out.join(format!("Shape {shape}.png")))?;
        assert!(s.at(200, 160)[3] > 245, "shape {shape}: solid in the middle");
        assert!(s.at(2, 2)[3] < 10, "shape {shape}: clear in the corner");
    }

    // Pixelate all of it, 40 px blocks: one colour per block, where stripes were 4 px.
    let s = add(
        "Stripes",
        &Value::Null,
        "ferrocast_blur_area",
        &json!({ "mode": 1, "strength": 40, "area_x": 0, "area_y": 0, "area_w": 100, "area_h": 100 }),
    )?;
    assert_eq!(s.at(2, 2), s.at(38, 38), "pixelate: a block is one colour");
    // Blur the left half: no hard stripes there, sharp ones on the right.
    c.update_filter(
        "Stripes",
        "Filter",
        &json!({ "mode": 0, "strength": 24, "area_x": 0, "area_y": 0, "area_w": 50, "area_h": 100 }),
    )?;
    let s = shot(&c, "Stripes", &out)?;
    let spread =
        |x0: u32| (x0..x0 + 8).map(|x| s.at(x, 150)[0]).fold((255u8, 0u8), |(lo, hi), v| (lo.min(v), hi.max(v)));
    let (lo, hi) = spread(100);
    assert!(hi - lo < 90, "blur: the left half is smooth ({lo}..{hi})");
    // Its edges don't darken (mid grey up to the picture's edge, not fading to clear).
    let [_, _, _, a] = s.at(1, 150);
    assert!(a > 245, "blur: solid up to the edge (alpha {a})");
    let (lo, hi) = spread(300);
    assert!(hi - lo > 200, "blur: the right half is still sharp ({lo}..{hi})");

    // Colour look: black and white turns red grey; strength 0 leaves it red.
    let s = add("Look", &red, "ferrocast_look", &json!({ "look": 3, "strength": 100 }))?;
    let [r, g, b, _] = s.at(200, 150);
    assert!(r.abs_diff(g) < 4 && g.abs_diff(b) < 4, "look: grey ({r}, {g}, {b})");
    c.update_filter("Look", "Filter", &json!({ "look": 3, "strength": 0 }))?;
    let [r, g, _, _] = shot(&c, "Look", &out)?.at(200, 150);
    assert!(r > 245 && g < 10, "look: strength 0 leaves it as it was");
    for look in [0, 1, 2, 4, 5, 6] {
        c.update_filter("Look", "Filter", &json!({ "look": look, "strength": 100 }))?;
        assert!(shot(&c, "Look", &out)?.at(200, 150)[3] > 245, "look {look} draws");
    }

    println!(
        "filters: ok on {}",
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

/// A source's screenshot (libobs draws a frame first), kept as <name>.png.
fn shot(c: &Collection, name: &str, out: &Path) -> Result<Shot> {
    std::thread::sleep(Duration::from_millis(250));
    let path = out.join(format!("{name}.png"));
    c.screenshot(name, &path)?;
    let mut d = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path)?));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut r = d.read_info()?;
    let mut px = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut px)?;
    px.truncate(info.buffer_size());
    Ok(Shot { w: info.width, h: info.height, px })
}

fn write_stripes(path: &Path, w: u32, h: u32) -> Result<()> {
    let mut e = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(path)?), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..h {
        for x in 0..w {
            let v = if (x / 4) % 2 == 0 { 0 } else { 255 };
            px.extend_from_slice(&[v, v, v, 255]);
        }
    }
    e.write_header()?.write_image_data(&px)?;
    Ok(())
}
