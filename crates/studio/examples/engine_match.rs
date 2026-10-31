//! Ferrocast's own engine (E0, E2) beside libobs: scenes built in libobs, drawn by both, compared
//! pixel by pixel: placement, every scale filter, every blend mode, sRGB blending off, groups and
//! nested scenes, Ferrocast's own filters, transitions (held part way by Studio Mode's T-bar), and
//! the NV12 that encoders get. Colours come from their settings, images from their files, and anything else
//! (here a Text source) is handed over as libobs draws it, as cameras will be. Items are placed
//! with our own sums from the transform libobs keeps. Pictures are kept in <out dir>: "<scene> obs.png",
//! "<scene> ours.png" and "<scene> diff.png" (every channel's difference, times 16).
//!
//!   cargo run -p ferrocast-studio --example engine_match -- <out dir> [opengl]

use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrocast_av::{
    Blend, Bounds, Compositor, Content, Direction, Effect, Filter, Item, Picture, Placement, Scene, Transition,
    TransitionKind,
};
use ferrocast_studio::{Canvas, Collection, ItemEdit, ItemInfo, Paths, Studio, Transform, TransitionInfo};
use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const W: u32 = 1280;
const H: u32 = 720;

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
    let studio = Studio::start(&paths, Canvas { width: W, height: H, fps: 30 }, &out.join("plugin-config"))?;
    let c = Collection::new(&studio, "Main")?;

    let photo = out.join("photo.png");
    write_photo(&photo, 320, 200)?;
    let checker = out.join("checker.png");
    write_checker(&checker, 8)?;
    let image = |scene: &str, name: &str, file: &Path| {
        c.add_source(scene, "image_source", name, &json!({ "file": file.display().to_string() }), false)
    };
    let place = |scene: &str, id: i64, f: &dyn Fn(&mut Transform)| -> Result<()> {
        let mut t = c.transform(scene, id)?;
        f(&mut t);
        c.set_transform(scene, id, &t)?;
        Ok(())
    };
    let back = |scene: &str| {
        // 0xAABBGGRR, as libobs keeps colours.
        c.add_source(
            scene,
            "color_source_v3",
            &format!("Back {scene}"),
            &json!({ "color": 0xFF_40_20_10u32, "width": W, "height": H }),
            false,
        )
    };
    let text_kind = if cfg!(windows) { "text_gdiplus_v3" } else { "text_ft2_source_v2" };

    // 1. Placement: moved, scaled, turned, cropped, fitted, half clear, point scaled, text.
    back("Main")?;
    let a = image("Main", "Photo", &photo)?;
    place("Main", a.id, &|t| (t.x, t.y, t.scale_x, t.scale_y) = (100.0, 80.0, 1.5, 1.5))?;
    let b = image("Main", "Turned", &photo)?;
    place("Main", b.id, &|t| {
        (t.x, t.y, t.rotation, t.alignment) = (800.0, 260.0, 30.0, 0);
        (t.crop_left, t.crop_top, t.crop_right, t.crop_bottom) = (20, 10, 40, 30);
    })?;
    let d = image("Main", "Boxed", &photo)?;
    place("Main", d.id, &|t| {
        (t.x, t.y) = (840.0, 420.0);
        t.bounds = "fit".into();
        (t.bounds_width, t.bounds_height, t.bounds_alignment) = (400.0, 280.0, 0);
    })?;
    let e = c.add_source(
        "Main",
        "color_source_v3",
        "Glass",
        &json!({ "color": 0x80_FF_FF_FFu32, "width": 300, "height": 150 }),
        false,
    )?;
    place("Main", e.id, &|t| (t.x, t.y) = (500.0, 450.0))?;
    let f = image("Main", "Pixels", &checker)?;
    place("Main", f.id, &|t| (t.x, t.y, t.scale_x, t.scale_y) = (40.0, 420.0, 12.0, 12.0))?;
    c.edit_item("Main", f.id, ItemEdit::ScalePoint)?;
    let g = c.add_source(
        "Main",
        text_kind,
        "Words",
        &json!({ "text": "Ferrocast engine E0", "font": { "face": "Helvetica", "size": 64 } }),
        false,
    )?;
    place("Main", g.id, &|t| (t.x, t.y) = (320.0, 600.0))?;

    // 2. Scale filters: bicubic, lanczos, area (down and up), bilinear, and small (low-res).
    c.add_scene("Filters")?;
    back("Filters")?;
    for (name, edit, x, y, scale) in [
        ("Bicubic", ItemEdit::ScaleBicubic, 10.0, 10.0, 1.37),
        ("Lanczos", ItemEdit::ScaleLanczos, 460.0, 10.0, 1.21),
        ("Area down", ItemEdit::ScaleArea, 880.0, 10.0, 0.73),
        ("Area up", ItemEdit::ScaleArea, 880.0, 170.0, 1.5),
        ("Bilinear", ItemEdit::ScaleBilinear, 10.0, 300.0, 0.9),
        ("Small", ItemEdit::ScaleBicubic, 330.0, 300.0, 0.31),
    ] {
        let i = image("Filters", name, &photo)?;
        place("Filters", i.id, &|t| (t.x, t.y, t.scale_x, t.scale_y) = (x, y, scale, scale))?;
        c.edit_item("Filters", i.id, edit)?;
    }
    let p = image("Filters", "Checker up", &checker)?;
    place("Filters", p.id, &|t| (t.x, t.y, t.scale_x, t.scale_y) = (500.0, 330.0, 20.0, 20.0))?;
    c.edit_item("Filters", p.id, ItemEdit::ScaleLanczos)?;

    // 3. Blend modes over a picture, sRGB blending off, and cropped text (drawn to a texture).
    c.add_scene("Blends")?;
    back("Blends")?;
    let under = image("Blends", "Under", &photo)?;
    place("Blends", under.id, &|t| (t.x, t.y, t.scale_x, t.scale_y) = (0.0, 0.0, 4.0, 3.6))?;
    for (i, edit) in [
        ItemEdit::BlendAdditive,
        ItemEdit::BlendSubtract,
        ItemEdit::BlendScreen,
        ItemEdit::BlendMultiply,
        ItemEdit::BlendLighten,
        ItemEdit::BlendDarken,
        ItemEdit::BlendMethodSrgbOff,
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("Mode {i}");
        let colour = [0xFF_30_A0_E0u32, 0xC0_80_80_80, 0xFF_20_E0_40][i % 3];
        let m = c.add_source(
            "Blends",
            "color_source_v3",
            &name,
            &json!({ "color": colour, "width": 150, "height": 300 }),
            false,
        )?;
        place("Blends", m.id, &|t| (t.x, t.y) = (20.0 + 175.0 * i as f32, 60.0))?;
        c.edit_item("Blends", m.id, edit)?;
    }
    let cw = c.add_source(
        "Blends",
        text_kind,
        "Cut words",
        &json!({ "text": "Cropped text", "font": { "face": "Helvetica", "size": 72 } }),
        false,
    )?;
    place("Blends", cw.id, &|t| {
        (t.x, t.y) = (100.0, 500.0);
        (t.crop_left, t.crop_top) = (30, 8);
    })?;

    // 4. A group (moved and turned as one) and a nested scene, shrunk and blended.
    c.add_scene("Nest")?;
    back("Nest")?;
    let g1 = image("Nest", "In group 1", &photo)?;
    let g2 = c.add_source(
        "Nest",
        "color_source_v3",
        "In group 2",
        &json!({ "color": 0xFF_20_E0_40u32, "width": 120, "height": 80 }),
        false,
    )?;
    place("Nest", g2.id, &|t| (t.x, t.y) = (200.0, 150.0))?;
    let grp = c.group_together("Nest", &[g1.id, g2.id], "Group")?;
    place("Nest", grp.id, &|t| (t.x, t.y, t.rotation, t.scale_x, t.scale_y) = (700.0, 80.0, 12.0, 1.2, 1.2))?;
    let nest = c.add_existing("Nest", "Filters")?;
    place("Nest", nest.id, &|t| (t.x, t.y, t.scale_x, t.scale_y) = (20.0, 330.0, 0.5, 0.5))?;
    let nest2 = c.add_existing("Nest", "Main")?;
    place("Nest", nest2.id, &|t| {
        (t.x, t.y, t.scale_x, t.scale_y) = (680.0, 400.0, 0.45, 0.45);
        (t.crop_left, t.crop_top) = (80, 60);
    })?;
    c.edit_item("Nest", nest2.id, ItemEdit::BlendScreen)?;

    // 5. Ferrocast's own filters, one each and three chained.
    c.add_scene("Effects")?;
    back("Effects")?;
    let fx: [(&str, &str, Value); 7] = [
        ("Round", "ferrocast_rounded_corners", json!({ "radius": 50 })),
        ("Edged", "ferrocast_border", json!({ "width": 10, "color": 0xFF_20_C0_FFu32, "radius": 30 })),
        ("Shaded", "ferrocast_shadow", json!({ "color": 0xB3_00_00_00u32, "blur": 24, "x": 10, "y": 14 })),
        ("Star", "ferrocast_shape", json!({ "shape": 3, "feather": 4, "stretch": false })),
        (
            "Hidden",
            "ferrocast_blur_area",
            json!({ "mode": 0, "strength": 16, "area_x": 20, "area_y": 20, "area_w": 50, "area_h": 50 }),
        ),
        (
            "Blocks",
            "ferrocast_blur_area",
            json!({ "mode": 1, "strength": 12, "area_x": 0, "area_y": 0, "area_w": 60, "area_h": 100 }),
        ),
        ("Vintage", "ferrocast_look", json!({ "look": 4, "strength": 80 })),
    ];
    for (i, (name, kind, settings)) in fx.iter().enumerate() {
        let it = image("Effects", name, &photo)?;
        let (x, y) = (30.0 + 410.0 * (i % 3) as f32, 30.0 + 230.0 * (i / 3) as f32);
        place("Effects", it.id, &|t| (t.x, t.y) = (x, y))?;
        c.add_filter(name, kind, "Filter", settings)?;
    }
    c.add_filter("Vintage", "ferrocast_rounded_corners", "Round too", &json!({ "radius": 24 }))?;
    c.add_filter(
        "Vintage",
        "ferrocast_shadow",
        "Glow",
        &json!({ "color": 0xFF_40_FF_FFu32, "blur": 12, "x": 0, "y": 0 }),
    )?;
    std::thread::sleep(Duration::from_millis(700));

    let mut gpu = Compositor::new()?;
    gpu.match_libobs_metal(metal && !cfg!(windows));
    let total = (W * H) as usize;
    let mut failed = Vec::new();
    for scene in ["Main", "Filters", "Blends", "Nest", "Effects"] {
        let ours = gpu.render(&Scene { width: W, height: H, items: items_of(&c, scene, &out)? })?;
        c.screenshot(scene, &out.join(format!("{scene} obs.png")))?;
        let obs = read_png(&out.join(format!("{scene} obs.png")))?;
        write_png(&out.join(format!("{scene} ours.png")), W, H, &ours)?;
        let (off, worst) = compare(&ours, &obs.rgba, &out.join(format!("{scene} diff.png")))?;
        println!(
            "engine_match {scene} ({}): {off} of {total} pixels differ by more than 2 ({:.3}%), worst {worst}",
            if cfg!(windows) {
                "D3D11"
            } else if metal {
                "Metal"
            } else {
                "OpenGL"
            },
            off as f64 * 100.0 / total as f64
        );
        // Edges of turned and scaled pictures may round a step apart; the rest must match.
        if off * 1000 > total {
            failed.push(scene);
        }
    }

    // 6. Transitions, held by Studio Mode's T-bar at 40%: Filters going to Blends.
    // Where OBS keeps the wipe pictures: inside the plugin on macOS, under data on Windows.
    let luma_file = if cfg!(windows) {
        Path::new(&std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| r"C:\Program Files\obs-studio".into()))
            .join(r"data\obs-plugins\obs-transitions\luma_wipes\linear-h.png")
    } else {
        Path::new(&std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into()))
            .join("PlugIns/obs-transitions.plugin/Contents/Resources/luma_wipes/linear-h.png")
    };
    let luma = read_png(&luma_file)?;
    c.set_studio_mode(true);
    c.set_current("Filters")?;
    for (kind, ours_kind) in [
        ("fade_transition", TransitionKind::Fade),
        ("fade_to_color_transition", TransitionKind::FadeToColor { rgb: [0, 0, 0], switch_point: 0.5 }),
        ("slide_transition", TransitionKind::Slide(Direction::Left)),
        ("swipe_transition", TransitionKind::Swipe { direction: Direction::Left, swipe_in: false }),
        ("wipe_transition", TransitionKind::LumaWipe { luma: luma.clone(), invert: false, softness: 0.03 }),
    ] {
        c.set_preview("Blends")?;
        c.set_transition(&TransitionInfo { kind: kind.into(), ms: 300 })?;
        c.tbar(0.4, false)?;
        std::thread::sleep(Duration::from_millis(1500));
        let file = out.join(format!("{kind} obs.png"));
        c.screenshot_program(&file)?;
        let obs = read_png(&file)?;
        let tr = Transition {
            kind: ours_kind,
            t: 0.4,
            from: Scene { width: W, height: H, items: items_of(&c, "Filters", &out)? },
            to: Scene { width: W, height: H, items: items_of(&c, "Blends", &out)? },
        };
        let ours = gpu.render_transition(&tr)?;
        let (off, worst) = compare(&ours, &obs.rgba, &out.join(format!("{kind} diff.png")))?;
        write_png(&out.join(format!("{kind} ours.png")), W, H, &ours)?;
        println!(
            "engine_match {kind} at 40%: {off} of {total} pixels differ by more than 2 ({:.3}%), worst {worst}",
            off as f64 * 100.0 / total as f64
        );
        if off * 1000 > total {
            failed.push(kind);
        }
        // Back to Filters, ready for the next.
        c.tbar(0.0, true)?;
        std::thread::sleep(Duration::from_millis(300));
    }

    // 7. NV12, what encoders get: "Main" on air, libobs's own conversion beside ours.
    c.set_studio_mode(false);
    c.set_current("Main")?;
    std::thread::sleep(Duration::from_millis(500));
    let feed = studio.program_feed()?;
    // A few frames in, so the program is settled.
    let mut frame = None;
    for _ in 0..10 {
        frame = feed.video.recv_timeout(Duration::from_secs(2)).ok();
    }
    drop(feed);
    let frame = frame.ok_or("libobs sent no frames")?;
    let ours = gpu.render_nv12(&Scene { width: W, height: H, items: items_of(&c, "Main", &out)? })?;
    let plane_diff = |a: &[u8], a_stride: usize, b: &[u8], w: usize, h: usize| {
        let (mut off, mut worst) = (0usize, 0u8);
        for y in 0..h {
            for x in 0..w {
                let d = a[y * a_stride + x].abs_diff(b[y * w + x]);
                worst = worst.max(d);
                if d > 2 {
                    off += 1;
                }
            }
        }
        (off, worst)
    };
    let (yo, yw) = plane_diff(&frame.y, frame.y_stride, &ours.y, W as usize, H as usize);
    let (uo, uw) = plane_diff(&frame.uv, frame.uv_stride, &ours.uv, W as usize, H as usize / 2);
    println!(
        "engine_match NV12: Y {yo} of {total} values differ by more than 2 (worst {yw}); CbCr {uo} of {total} (worst {uw})"
    );
    if (yo + uo) * 1000 > total {
        failed.push("nv12");
    }

    drop(c);
    if !failed.is_empty() {
        return Err(format!(
            "more than 0.1% of pixels differ in {failed:?}; see the diff pictures in {}",
            out.display()
        )
        .into());
    }
    Ok(())
}

/// A source's Ferrocast filters (on, in order) as our engine's effects.
fn effects_of(c: &Collection, name: &str) -> Result<Vec<Effect>> {
    let mut out = Vec::new();
    for f in c.filters(name)? {
        if !f.enabled {
            continue;
        }
        let s = c.filter_settings(name, &f.name)?;
        let n = |k: &str, d: f64| s[k].as_f64().unwrap_or(d) as f32;
        let colour = |d: u32| Effect::rgba(s["color"].as_u64().map_or(d, |v| v as u32));
        out.push(match f.kind.as_str() {
            "ferrocast_rounded_corners" => Effect::RoundedCorners { radius: n("radius", 40.0) },
            "ferrocast_border" => {
                Effect::Border { width: n("width", 8.0), rgba: colour(0xFFFF_FFFF), radius: n("radius", 0.0) }
            }
            "ferrocast_shadow" => {
                Effect::Shadow { rgba: colour(0xB300_0000), blur: n("blur", 24.0), x: n("x", 0.0), y: n("y", 12.0) }
            }
            "ferrocast_shape" => Effect::Shape {
                shape: n("shape", 0.0) as u32,
                feather: n("feather", 0.0),
                stretch: s["stretch"].as_bool().unwrap_or(false),
            },
            "ferrocast_blur_area" => Effect::BlurArea {
                pixelate: n("mode", 0.0) > 0.5,
                strength: n("strength", 24.0),
                area: [n("area_x", 25.0), n("area_y", 25.0), n("area_w", 50.0), n("area_h", 50.0)],
            },
            "ferrocast_look" => Effect::Look { look: n("look", 0.0) as u32, strength: n("strength", 100.0) },
            other => return Err(format!("\"{name}\" has a filter our engine doesn't run yet: {other}").into()),
        });
    }
    Ok(out)
}

/// Pixels more than 2 apart and the worst difference; the difference picture (times 16) is saved.
fn compare(ours: &[u8], obs: &[u8], diff_file: &Path) -> Result<(usize, u8)> {
    let (mut worst, mut off, mut diff) = (0u8, 0usize, Vec::with_capacity(ours.len()));
    for (px_o, px_l) in ours.chunks(4).zip(obs.chunks(4)) {
        let d = px_o.iter().zip(px_l).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
        worst = worst.max(d);
        if d > 2 {
            off += 1;
        }
        for (a, b) in px_o.iter().zip(px_l).take(3) {
            diff.push(a.abs_diff(*b).saturating_mul(16));
        }
        diff.push(255);
    }
    write_png(diff_file, W, H, &diff)?;
    Ok((off, worst))
}

/// A scene's items as our engine takes them, bottom first, from what libobs keeps.
fn items_of(c: &Collection, scene: &str, out: &Path) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for info in c.items(scene)?.into_iter().rev() {
        if let Some(i) = item_of(c, scene, &info, out)? {
            items.push(i);
        }
    }
    Ok(items)
}

fn item_of(c: &Collection, scene: &str, info: &ItemInfo, out: &Path) -> Result<Option<Item>> {
    if !info.visible {
        return Ok(None);
    }
    let t = c.transform(scene, info.id)?;
    let content = if info.group {
        let mut inner = Vec::new();
        for ch in info.children.iter().rev() {
            if let Some(i) = item_of(c, scene, ch, out)? {
                inner.push(i);
            }
        }
        Content::Group { width: t.width, height: t.height, items: inner }
    } else {
        match info.kind.as_str() {
            "scene" => Content::Scene(Scene { width: W, height: H, items: items_of(c, &info.name, out)? }),
            "color_source_v3" => {
                let s = c.settings(&info.name)?;
                let v = s["color"].as_u64().unwrap_or(0) as u32;
                Content::Picture(Picture::solid(t.width, t.height, v.to_le_bytes()))
            }
            "image_source" => {
                let p = read_png(Path::new(c.settings(&info.name)?["file"].as_str().unwrap_or_default()))?;
                Content::Picture(Picture::image(p.width, p.height, p.rgba))
            }
            _ => {
                // Handed over: libobs draws the source on its own.
                let p = out.join(format!("{}.png", info.name));
                c.screenshot(&info.name, &p)?;
                Content::Picture(Picture { premultiplied: true, ..read_png(&p)? })
            }
        }
    };
    let linear = info.group || info.kind == "scene" || c.linear_light(&info.name)?;
    Ok(Some(Item {
        content,
        effects: if info.group || info.kind == "scene" { Vec::new() } else { effects_of(c, &info.name)? },
        placement: placement(&t),
        filter: Filter::from_name(&info.scale_filter),
        blend: Blend::from_name(&info.blend),
        srgb_off: info.srgb_off,
        linear,
    }))
}

fn placement(t: &Transform) -> Placement {
    let crop = |v: i32| v.max(0) as u32;
    Placement {
        x: t.x,
        y: t.y,
        rotation: t.rotation,
        scale_x: t.scale_x,
        scale_y: t.scale_y,
        alignment: t.alignment,
        bounds: Bounds::from_name(&t.bounds),
        bounds_alignment: t.bounds_alignment,
        bounds_width: t.bounds_width,
        bounds_height: t.bounds_height,
        crop: [crop(t.crop_left), crop(t.crop_top), crop(t.crop_right), crop(t.crop_bottom)],
    }
}

fn read_png(path: &Path) -> Result<Picture> {
    let mut d = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut r = d.read_info()?;
    let mut px = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut px)?;
    px.truncate(info.buffer_size());
    if info.color_type == png::ColorType::GrayscaleAlpha {
        px = px.chunks(2).flat_map(|g| [g[0], g[0], g[0], g[1]]).collect();
    }
    Ok(Picture { width: info.width, height: info.height, rgba: px, premultiplied: false })
}

fn write_png(path: &Path, w: u32, h: u32, px: &[u8]) -> Result<()> {
    let mut e = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(path)?), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    e.write_header()?.write_image_data(px)?;
    Ok(())
}

/// Colour ramps across and down, the right quarter fading out: scaling, turning and alpha show.
fn write_photo(path: &Path, w: u32, h: u32) -> Result<()> {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let a = if x < w * 3 / 4 { 255 } else { 255 - (x - w * 3 / 4) * 255 / (w / 4) };
            px.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 160, a as u8]);
        }
    }
    write_png(path, w, h, &px)
}

/// A tiny black and white checker, for point scaling.
fn write_checker(path: &Path, n: u32) -> Result<()> {
    let mut px = Vec::new();
    for y in 0..n {
        for x in 0..n {
            let v = if (x + y) % 2 == 0 { 0 } else { 255 };
            px.extend_from_slice(&[v, v, v, 255]);
        }
    }
    write_png(path, n, n, &px)
}
