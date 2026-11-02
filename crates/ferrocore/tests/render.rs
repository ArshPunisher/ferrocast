//! Draws small scenes on this computer's graphics card. Without one (some CI machines) the
//! tests say so and pass: libobs's own check (`engine_match`) needs a GPU anyway.

use ferrocore::{Blend, Bounds, Compositor, Item, Picture, Placement, Scene};

fn gpu() -> Option<Compositor> {
    match Compositor::new() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn at(px: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [px[i], px[i + 1], px[i + 2], px[i + 3]]
}

fn item(picture: Picture, placement: Placement) -> Item {
    Item::picture(picture, placement)
}

#[test]
fn a_colour_lands_where_placed() {
    let Some(c) = gpu() else { return };
    let red = item(Picture::solid(40, 30, [255, 0, 0, 255]), Placement { x: 10.0, y: 20.0, ..Default::default() });
    let px = c.render(&Scene { width: 100, height: 80, items: vec![red] }).unwrap_or_default();
    assert_eq!(at(&px, 100, 10, 20), [255, 0, 0, 255]);
    assert_eq!(at(&px, 100, 49, 49), [255, 0, 0, 255]);
    assert_eq!(at(&px, 100, 50, 20), [0, 0, 0, 0], "right of it is clear");
    assert_eq!(at(&px, 100, 9, 20), [0, 0, 0, 0], "left of it is clear");
}

#[test]
fn later_items_cover_earlier_ones() {
    let Some(c) = gpu() else { return };
    let under = item(Picture::solid(100, 80, [0, 0, 255, 255]), Placement::default());
    let over = item(Picture::solid(10, 10, [0, 255, 0, 255]), Placement { x: 45.0, y: 35.0, ..Default::default() });
    let px = c.render(&Scene { width: 100, height: 80, items: vec![under, over] }).unwrap_or_default();
    assert_eq!(at(&px, 100, 50, 40), [0, 255, 0, 255]);
    assert_eq!(at(&px, 100, 5, 5), [0, 0, 255, 255]);
}

#[test]
fn crop_shows_only_the_kept_part() {
    let Some(c) = gpu() else { return };
    // Left half black, right half white; cropping the left half off leaves white only.
    let mut rgba = Vec::new();
    for _ in 0..20 {
        for x in 0..40 {
            let v = if x < 20 { 0 } else { 255 };
            rgba.extend_from_slice(&[v, v, v, 255]);
        }
    }
    let pic = Picture { width: 40, height: 20, rgba, premultiplied: false };
    let p = Placement {
        crop: [20, 0, 0, 0],
        bounds: Bounds::Stretch,
        bounds_width: 100.0,
        bounds_height: 80.0,
        ..Default::default()
    };
    let px = c.render(&Scene { width: 100, height: 80, items: vec![item(pic, p)] }).unwrap_or_default();
    for x in [0, 50, 99] {
        assert_eq!(at(&px, 100, x, 40), [255, 255, 255, 255], "x {x}");
    }
}

#[test]
fn half_clear_mixes_in_linear_light() {
    let Some(c) = gpu() else { return };
    let black = item(Picture::solid(10, 10, [0, 0, 0, 255]), Placement::default());
    let white = item(Picture::solid(10, 10, [255, 255, 255, 128]), Placement::default());
    let px = c.render(&Scene { width: 10, height: 10, items: vec![black, white] }).unwrap_or_default();
    // Half of white's light is sRGB ~188, not 128.
    let v = at(&px, 10, 5, 5)[0];
    assert!((185..=190).contains(&v), "got {v}");
}

#[test]
fn older_sources_mix_in_gamma_space() {
    let Some(c) = gpu() else { return };
    let black = item(Picture::solid(10, 10, [0, 0, 0, 255]), Placement::default());
    let white = Item { linear: false, ..item(Picture::solid(10, 10, [255, 255, 255, 128]), Placement::default()) };
    let px = c.render(&Scene { width: 10, height: 10, items: vec![black, white] }).unwrap_or_default();
    let v = at(&px, 10, 5, 5)[0];
    assert!((127..=129).contains(&v), "got {v}");
}

#[test]
fn blend_modes_mix_as_libobs_does() {
    let Some(c) = gpu() else { return };
    // 50% grey under each mode with a 25% grey on top (opaque), mixed in linear light.
    let check = |blend: Blend| -> u8 {
        let under = item(Picture::solid(4, 4, [188, 188, 188, 255]), Placement::default());
        let over = Item { blend, ..item(Picture::solid(4, 4, [137, 137, 137, 255]), Placement::default()) };
        let px = c.render(&Scene { width: 4, height: 4, items: vec![under, over] }).unwrap_or_default();
        at(&px, 4, 2, 2)[0]
    };
    // Linear values: under 0.5, over 0.25.
    assert_eq!(check(Blend::Normal), 137);
    assert_eq!(check(Blend::Lighten), 188);
    assert_eq!(check(Blend::Darken), 137);
    assert!((224..=226).contains(&check(Blend::Additive)), "0.75 → 225");
    assert!((136..=138).contains(&check(Blend::Subtract)), "0.25 → 137");
    assert!((99..=101).contains(&check(Blend::Multiply)), "0.125 → 99");
    assert!((206..=208).contains(&check(Blend::Screen)), "0.625 → 207");
}

#[test]
fn groups_place_their_items_inside_their_box() {
    use ferrocore::Content;
    let Some(c) = gpu() else { return };
    let dot = item(Picture::solid(2, 2, [255, 0, 0, 255]), Placement { x: 3.0, y: 4.0, ..Default::default() });
    let group = Item {
        content: Content::Group { width: 10, height: 10, items: vec![dot] },
        ..item(Picture::solid(1, 1, [0; 4]), Placement { x: 20.0, y: 10.0, ..Default::default() })
    };
    let px = c.render(&Scene { width: 40, height: 40, items: vec![group] }).unwrap_or_default();
    assert_eq!(at(&px, 40, 23, 14), [255, 0, 0, 255]);
    assert_eq!(at(&px, 40, 3, 4), [0, 0, 0, 0]);
}

#[test]
fn an_empty_scene_is_clear() {
    let Some(c) = gpu() else { return };
    let px = c.render(&Scene { width: 4, height: 4, items: vec![] }).unwrap_or_default();
    assert_eq!(px, vec![0; 64]);
}

#[test]
fn bad_pictures_are_refused() {
    let Some(c) = gpu() else { return };
    let short = Picture { width: 10, height: 10, rgba: vec![0; 12], premultiplied: false };
    let s = Scene { width: 10, height: 10, items: vec![item(short, Placement::default())] };
    assert!(c.render(&s).is_err());
    assert!(c.render(&Scene { width: 0, height: 10, items: vec![] }).is_err());
}

#[test]
fn uploaded_pictures_draw_and_update_in_place() {
    use ferrocore::Content;
    let Some(c) = gpu() else { return };
    c.upload(7, &Picture::solid(4, 4, [255, 0, 0, 255])).unwrap_or_else(|e| panic!("{e}"));
    let it = Item {
        content: Content::Uploaded { id: 7, width: 4, height: 4, premultiplied: false },
        ..item(Picture::solid(1, 1, [0; 4]), Placement::default())
    };
    let scene = Scene { width: 4, height: 4, items: vec![it] };
    assert_eq!(at(&c.render(&scene).unwrap_or_default(), 4, 1, 1), [255, 0, 0, 255]);
    // The next "camera frame", same size: drawn without a new texture.
    c.upload(7, &Picture::solid(4, 4, [0, 0, 255, 255])).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(at(&c.render(&scene).unwrap_or_default(), 4, 1, 1), [0, 0, 255, 255]);
    c.forget(7);
    assert!(c.render(&scene).is_err(), "forgotten pictures are refused, not drawn as garbage");
}

#[test]
fn nv12_is_bt709_limited() {
    let Some(c) = gpu() else { return };
    let check = |rgba: [u8; 4]| {
        let s = Scene { width: 8, height: 8, items: vec![item(Picture::solid(8, 8, rgba), Placement::default())] };
        let f = c.render_nv12(&s).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((f.y.len(), f.uv.len()), (64, 32));
        (f.y[27], f.uv[10], f.uv[11])
    };
    assert_eq!(check([255, 255, 255, 255]), (235, 128, 128));
    assert_eq!(check([0, 0, 0, 255]), (16, 128, 128));
    // Pure red in BT.709 limited: Y 63, Cb 102, Cr 240.
    let (y, cb, cr) = check([255, 0, 0, 255]);
    assert!(y.abs_diff(63) <= 1 && cb.abs_diff(102) <= 1 && cr.abs_diff(240) <= 1, "{y} {cb} {cr}");
}

#[test]
fn transitions_refuse_bad_sizes_instead_of_crashing() {
    use ferrocore::{Transition, TransitionKind};
    let Some(c) = gpu() else { return };
    let empty = Scene { width: 0, height: 10, items: vec![] };
    let tr = Transition { kind: TransitionKind::Fade, t: 0.5, from: empty.clone(), to: empty };
    assert!(c.render_transition(&tr).is_err());
    assert!(c.render_transition_nv12(&tr).is_err());
}

#[test]
fn opacity_fades_an_item_and_a_group_as_one() {
    use ferrocore::Content;
    let Some(c) = gpu() else { return };
    let black = item(Picture::solid(10, 10, [0, 0, 0, 255]), Placement::default());
    let white = Item { opacity: 0.5, ..item(Picture::solid(10, 10, [255, 255, 255, 255]), Placement::default()) };
    let px = c.render(&Scene { width: 10, height: 10, items: vec![black.clone(), white.clone()] }).unwrap_or_default();
    // Half of white's light: sRGB about 188.
    assert!((185..=190).contains(&at(&px, 10, 5, 5)[0]), "{}", at(&px, 10, 5, 5)[0]);
    // Two white squares overlapping in a half-faded group: the overlap is no brighter.
    let a = item(Picture::solid(6, 6, [255, 255, 255, 255]), Placement::default());
    let b = item(Picture::solid(6, 6, [255, 255, 255, 255]), Placement { x: 4.0, ..Default::default() });
    let group = Item { content: Content::Group { width: 10, height: 6, items: vec![a, b] }, opacity: 0.5, ..white };
    let px = c.render(&Scene { width: 10, height: 10, items: vec![black, group] }).unwrap_or_default();
    assert_eq!(at(&px, 10, 5, 2)[0], at(&px, 10, 1, 2)[0], "the overlap fades with the rest");
}
