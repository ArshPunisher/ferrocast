//! How fast our compositor makes frames: a 1080p scene like a stream's (a background, a game
//! picture, a camera updated every frame with rounded corners and a border, a turned and
//! cropped logo with bicubic scaling, a half-clear lower third), drawn and read back as NV12
//! for the encoder, `frames` times.
//!
//!   cargo run --release -p ferrocore --example bench -- [frames]

use std::time::Instant;

use ferrocore::{Blend, Bounds, Compositor, Content, Effect, Filter, Item, Picture, Placement, Scene};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let frames: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(300);
    let c = Compositor::new()?;
    let gradient = |w: u32, h: u32, k: u32| {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, (k * 40) as u8, 255]);
            }
        }
        Picture { width: w, height: h, rgba: px, premultiplied: false }
    };
    c.upload(1, &Picture::solid(1920, 1080, [16, 20, 40, 255]))?;
    c.upload(2, &gradient(1920, 1080, 1))?;
    c.upload(4, &gradient(400, 400, 3))?;
    let up = |id: u64, w: u32, h: u32| Content::Uploaded { id, width: w, height: h, premultiplied: false };
    let plain = |content: Content, placement: Placement| Item {
        content,
        effects: Vec::new(),
        placement,
        filter: Filter::Disable,
        blend: Blend::Normal,
        srgb_off: false,
        linear: true,
        opacity: 1.0,
    };
    let camera_frames: Vec<Picture> = (0..2).map(|k| gradient(1280, 720, k + 4)).collect();
    let mut total = 0.0f64;
    let mut worst = 0.0f64;
    for i in 0..frames {
        let t = Instant::now();
        // A camera's new frame each time, as a real one would.
        c.upload(3, &camera_frames[(i % 2) as usize])?;
        let scene = Scene {
            width: 1920,
            height: 1080,
            items: vec![
                plain(up(1, 1920, 1080), Placement::default()),
                plain(
                    up(2, 1920, 1080),
                    Placement {
                        bounds: Bounds::Fit,
                        bounds_width: 1920.0,
                        bounds_height: 1080.0,
                        ..Default::default()
                    },
                ),
                Item {
                    effects: vec![
                        Effect::RoundedCorners { radius: 40.0 },
                        Effect::Border { width: 6.0, rgba: [1.0, 0.4, 0.1, 1.0], radius: 40.0 },
                    ],
                    ..plain(
                        up(3, 1280, 720),
                        Placement { x: 1380.0, y: 700.0, scale_x: 0.4, scale_y: 0.4, ..Default::default() },
                    )
                },
                Item {
                    filter: Filter::Bicubic,
                    ..plain(
                        up(4, 400, 400),
                        Placement {
                            x: 120.0,
                            y: 120.0,
                            rotation: 8.0,
                            scale_x: 0.6,
                            scale_y: 0.6,
                            crop: [20, 20, 20, 20],
                            ..Default::default()
                        },
                    )
                },
                plain(
                    Content::Picture(Picture::solid(1200, 120, [0, 0, 0, 160])),
                    Placement { x: 60.0, y: 900.0, ..Default::default() },
                ),
            ],
        };
        let f = c.render_nv12(&scene)?;
        std::hint::black_box(&f);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if i > 5 {
            total += ms;
            worst = worst.max(ms);
        }
    }
    let n = f64::from(frames.saturating_sub(6).max(1));
    println!(
        "1080p NV12, {frames} frames: {:.2} ms a frame on average, slowest {worst:.2} ms ({:.0} fps possible)",
        total / n,
        1000.0 / (total / n)
    );
    Ok(())
}
