//! Draws a few text styles onto a dark canvas and saves it, to look at.
//!
//!   cargo run -p ferrocast-av --example text -- <out.png>

use ferrocast_av::source::text::{TextAlign, TextRenderer, TextStyle};
use ferrocast_av::{Compositor, Item, Picture, Placement, Scene};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args().nth(1).unwrap_or_else(|| "text.png".into());
    let mut r = TextRenderer::new();
    let styles = [
        ("Ferrocast engine — any font", TextStyle { size: 64.0, bold: true, ..Default::default() }),
        (
            "Outline, shadow, gradient",
            TextStyle {
                size: 56.0,
                color: [255, 200, 60, 255],
                gradient: Some([255, 80, 30, 255]),
                outline: Some((4.0, [20, 20, 20, 255])),
                shadow: Some(((6, 6), [0, 0, 0, 160])),
                ..Default::default()
            },
        ),
        ("नमस्ते · مرحبا · こんにちは · 🎮🔥", TextStyle { size: 48.0, ..Default::default() }),
        (
            "Wrapped and centred in a box with a background, as a lower third would be.",
            TextStyle {
                size: 36.0,
                wrap_width: Some(700),
                align: TextAlign::Center,
                background: Some([255, 90, 31, 220]),
                padding: 16,
                ..Default::default()
            },
        ),
    ];
    let mut items = vec![Item::picture(Picture::solid(1280, 720, [16, 20, 40, 255]), Placement::default())];
    let mut y = 40.0;
    for (text, style) in &styles {
        let p = r.render(text, style);
        let h = p.height as f32;
        items.push(Item::picture(p, Placement { x: 40.0, y, ..Default::default() }));
        y += h + 30.0;
    }
    let c = Compositor::new()?;
    let px = c.render(&Scene { width: 1280, height: 720, items })?;
    image::save_buffer(&out, &px, 1280, 720, image::ExtendedColorType::Rgba8)?;
    println!("saved {out}");
    Ok(())
}
