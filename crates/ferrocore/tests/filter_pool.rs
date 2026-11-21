//! Filter chains on the graphics card: the texture pool and the sharing of one chain between items
//! with the same picture and the same filters. Without a card (some CI machines) these say so and
//! pass. Run with `--nocapture` for the numbers:
//! `cargo test -p ferrocore --release --test filter_pool -- --nocapture`.

use std::time::Instant;

use ferrocore::{Blend, Compositor, Content, Effect, Filter, Item, Picture, Placement, Scene};

fn gpu() -> Option<Compositor> {
    match Compositor::new() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn effects() -> Vec<Effect> {
    vec![
        Effect::RoundedCorners { radius: 24.0 },
        Effect::Adjust {
            exposure: 0.3,
            contrast: 10.0,
            highlights: 0.0,
            shadows: 0.0,
            saturation: 20.0,
            vibrance: 0.0,
            temperature: 5.0,
            tint: 0.0,
        },
        Effect::Look { look: 3, strength: 40.0 },
        Effect::Border { width: 4.0, rgba: [1.0, 0.5, 0.0, 1.0], radius: 24.0 },
    ]
}

fn item(x: f32, fx: Vec<Effect>) -> Item {
    Item {
        content: Content::Uploaded { id: 1, width: 640, height: 360, premultiplied: false },
        effects: fx,
        placement: Placement { x, y: 20.0, ..Default::default() },
        filter: Filter::Disable,
        blend: Blend::Normal,
        srgb_off: false,
        linear: true,
        opacity: 1.0,
    }
}

/// A camera in three scenes (Main, Gameplay, Vertical) with identical copied filter lists, and
/// Studio Mode's preview and the program drawing two of them again: five scenes a frame.
fn scenes() -> Vec<Scene> {
    let one = |n: usize| Scene {
        width: 1280,
        height: 720,
        items: (0..n).map(|i| item(10.0 + 300.0 * i as f32, effects())).collect(),
    };
    vec![one(3), one(2), one(1), one(3), one(2)]
}

fn picture() -> Picture {
    let mut rgba = Vec::with_capacity(640 * 360 * 4);
    for y in 0..360u32 {
        for x in 0..640u32 {
            rgba.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]);
        }
    }
    Picture { width: 640, height: 360, rgba, premultiplied: false }
}

#[test]
fn pooling_and_sharing_change_nothing_on_screen_and_cost_less() {
    let Some(c) = gpu() else { return };
    c.upload(1, &picture()).unwrap();
    let list = scenes();
    let frame = |c: &Compositor| -> Vec<Vec<u8>> { list.iter().map(|s| c.render(s).unwrap()).collect() };

    // Before: no pool, no sharing.
    c.set_filter_savings_off(true);
    let reference = frame(&c);
    let (made0, ..) = c.texture_stats();
    let t = Instant::now();
    for _ in 0..200 {
        let _ = frame(&c);
    }
    let before_ms = t.elapsed().as_secs_f64() * 1000.0 / 200.0;
    let (made1, reused1, shared1) = c.texture_stats();
    let before_made = (made1 - made0) as f64 / 200.0;
    assert_eq!((reused1, shared1), (0, 0));

    // After.
    c.set_filter_savings_off(false);
    let first = frame(&c);
    assert!(first == reference, "pictures differ with the pool and shared chains");
    let (m0, r0, s0) = c.texture_stats();
    let t = Instant::now();
    for _ in 0..200 {
        let again = frame(&c);
        assert!(again == reference, "a later frame differs (a reused texture wasn't cleared?)");
    }
    let after_ms = t.elapsed().as_secs_f64() * 1000.0 / 200.0;
    let (m1, r1, s1) = c.texture_stats();
    eprintln!(
        "5 scenes a frame, 4 filters each: before {before_ms:.2} ms and {before_made:.0} new textures a frame; \
         after {after_ms:.2} ms and {:.1} new, {:.1} reused, {:.1} shared chains a frame",
        (m1 - m0) as f64 / 200.0,
        (r1 - r0) as f64 / 200.0,
        (s1 - s0) as f64 / 200.0
    );
    assert!(m1 - m0 <= 8, "a steady frame still makes {} new textures", m1 - m0);
    assert!(s1 - s0 > 0, "identical chains were never shared");
    assert!(r1 > r0);
}
