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

/// Uploads whose rows are a multiple of 256 bytes go through buffers kept between frames: a
/// run of them with no frame drawn between (all buffers busy, the plain way used after the
/// first few) must still end on the newest picture, and each picture must show whole.
#[test]
fn uploads_through_kept_buffers_always_show_the_newest_picture() {
    use ferrocore::Content;
    let Some(c) = gpu() else { return };
    let (w, h) = (64u32, 6u32);
    let scene = |id: u64| Scene {
        width: w,
        height: h,
        items: vec![Item {
            content: Content::Uploaded { id, width: w, height: h, premultiplied: false },
            ..item(Picture::solid(1, 1, [0; 4]), Placement::default())
        }],
    };
    let colour = |k: u8| [k * 20, 255 - k * 20, k, 255];
    for k in 0..12 {
        c.upload(9, &Picture::solid(w, h, colour(k))).unwrap_or_else(|e| panic!("{e}"));
    }
    let px = c.render(&scene(9)).unwrap_or_default();
    for (x, y) in [(0, 0), (63, 5), (31, 3)] {
        assert_eq!(at(&px, w, x, y), colour(11), "the newest of twelve uploads in a row, at {x},{y}");
    }
    // A frame drawn after each: every picture comes out as sent, for far more than the
    // buffers kept (each is mapped again once the card is done with it).
    for k in 0..40 {
        c.upload(9, &Picture::solid(w, h, colour(k % 12))).unwrap_or_else(|e| panic!("{e}"));
        let px = c.render(&scene(9)).unwrap_or_default();
        assert_eq!(at(&px, w, 17, 2), colour(k % 12), "upload {k}");
    }
    // A picture of another size takes its own texture and buffers.
    c.upload(9, &Picture::solid(128, 2, [1, 2, 3, 255])).unwrap_or_else(|e| panic!("{e}"));
    let s2 = Scene {
        width: 128,
        height: 2,
        items: vec![Item {
            content: Content::Uploaded { id: 9, width: 128, height: 2, premultiplied: false },
            ..item(Picture::solid(1, 1, [0; 4]), Placement::default())
        }],
    };
    assert_eq!(at(&c.render(&s2).unwrap_or_default(), 128, 100, 1), [1, 2, 3, 255]);
}

/// NV12 frames reuse the canvas, planes and readback buffers of the frame before: what one
/// frame drew must never show in the next, whatever it drew or didn't.
#[test]
fn frames_that_reuse_their_resources_carry_nothing_over() {
    let Some(c) = gpu() else { return };
    let solid = |rgba: [u8; 4], size: u32| Scene {
        width: 16,
        height: 16,
        items: vec![item(Picture::solid(size, size, rgba), Placement::default())],
    };
    let draw = |s: &Scene| c.render_nv12(s).unwrap_or_else(|e| panic!("{e}"));
    let white = draw(&solid([255, 255, 255, 255], 16));
    let corner = draw(&solid([255, 0, 0, 255], 4));
    let empty = draw(&Scene { width: 16, height: 16, items: Vec::new() });
    let white_again = draw(&solid([255, 255, 255, 255], 16));
    assert_eq!(white, white_again, "the same scene draws the same after others");
    assert_eq!((corner.y[0], corner.y[15 * 16 + 15]), (63, 16), "red in the corner, black elsewhere");
    assert!(empty.y.iter().all(|y| *y == 16), "an empty frame is black, not what came before");
    assert!(empty.uv.iter().all(|v| *v == 128));
    // Another size alongside (an output at its own size) keeps its own.
    let small = c
        .render_nv12(&Scene {
            width: 8,
            height: 8,
            items: vec![item(Picture::solid(8, 8, [0, 0, 0, 255]), Placement::default())],
        })
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((small.y.len(), small.uv.len()), (64, 32));
    assert_eq!(draw(&solid([255, 255, 255, 255], 16)), white);
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

#[test]
fn a_shader_effect_of_ones_own_runs_and_a_broken_one_is_left_out() {
    use ferrocore::Effect;
    let Some(c) = gpu() else { return };
    let fx = |code: &str, params: [f32; 4]| Effect::Custom { code: code.into(), params, elapsed_ms: 0 };
    let invert = "fn effect(uv: vec2<f32>) -> vec4<f32> { let c = pic(uv); return vec4<f32>(1.0 - c.rgb, c.a); }";
    let mut red = item(Picture::solid(20, 20, [255, 0, 0, 255]), Placement::default());
    red.effects = vec![fx(invert, [0.0; 4])];
    let px = c.render(&Scene { width: 20, height: 20, items: vec![red.clone()] }).unwrap_or_default();
    assert_eq!(at(&px, 20, 10, 10), [0, 255, 255, 255], "red inverted is cyan");
    // A slider reaches the shader: its first value is the green.
    let slider = "fn effect(uv: vec2<f32>) -> vec4<f32> { return vec4<f32>(0.0, params().x, 0.0, 1.0); }";
    red.effects = vec![fx(slider, [1.0, 0.0, 0.0, 0.0])];
    let px = c.render(&Scene { width: 20, height: 20, items: vec![red.clone()] }).unwrap_or_default();
    assert_eq!(at(&px, 20, 10, 10), [0, 255, 0, 255]);
    // A shader that doesn't compile: the picture as it was, and the next frame still draws.
    red.effects = vec![fx("fn effect(uv: vec2<f32>) -> vec4<f32> { return oops; }", [0.0; 4])];
    for _ in 0..2 {
        let px = c.render(&Scene { width: 20, height: 20, items: vec![red.clone()] }).unwrap_or_default();
        assert_eq!(at(&px, 20, 10, 10), [255, 0, 0, 255]);
    }
}

/// One pixel of `rgb` through `effect` → the pixel out.
fn through(c: &Compositor, rgb: [u8; 3], effect: ferrocore::Effect) -> [u8; 4] {
    let mut it = item(Picture::solid(4, 4, [rgb[0], rgb[1], rgb[2], 255]), Placement::default());
    it.effects = vec![effect];
    let px = c.render(&Scene { width: 4, height: 4, items: vec![it] }).unwrap_or_default();
    at(&px, 4, 2, 2)
}

fn adjust() -> ferrocore::Effect {
    ferrocore::Effect::Adjust {
        exposure: 0.0,
        contrast: 0.0,
        highlights: 0.0,
        shadows: 0.0,
        saturation: 0.0,
        vibrance: 0.0,
        temperature: 0.0,
        tint: 0.0,
    }
}

fn with(f: impl FnOnce(&mut [f32; 8])) -> ferrocore::Effect {
    let mut v = [0.0f32; 8];
    f(&mut v);
    ferrocore::Effect::Adjust {
        exposure: v[0],
        contrast: v[1],
        highlights: v[2],
        shadows: v[3],
        saturation: v[4],
        vibrance: v[5],
        temperature: v[6],
        tint: v[7],
    }
}

fn near(a: [u8; 4], b: [u8; 3], by: u8) -> bool {
    (0..3).all(|i| a[i].abs_diff(b[i]) <= by)
}

/// Adjust at 0 leaves a picture as it is (a photo app's sliders in the middle).
#[test]
fn adjust_at_zero_changes_nothing() {
    let Some(c) = gpu() else { return };
    for rgb in [[0, 0, 0], [255, 255, 255], [200, 120, 40], [30, 90, 200], [128, 128, 128]] {
        let got = through(&c, rgb, adjust());
        assert!(near(got, rgb, 2), "{rgb:?} came out {got:?}");
    }
}

/// Each slider does what its name says, the way a photo app's does.
#[test]
fn adjust_sliders_do_what_they_say() {
    let Some(c) = gpu() else { return };
    let grey = [118, 118, 118];
    // Exposure: one stop is twice the light (sRGB 118 → about 161).
    let up = through(&c, grey, with(|v| v[0] = 1.0));
    assert!((155..=167).contains(&up[0]), "{up:?}");
    // Contrast: dark goes darker and light lighter; mid-grey stays; nothing clips.
    let (dark, light, mid) = ([60, 60, 60], [190, 190, 190], [128, 128, 128]);
    let more = |rgb| through(&c, rgb, with(|v| v[1] = 60.0));
    assert!(more(dark)[0] < 56 && more(light)[0] > 194, "{:?} {:?}", more(dark), more(light));
    assert!(near(more(mid), [128, 128, 128], 4), "{:?}", more(mid));
    assert!(more([250, 250, 250])[0] < 255 && more([6, 6, 6])[0] > 0, "an S-curve, not a clip");
    let less = through(&c, dark, with(|v| v[1] = -60.0));
    assert!(less[0] > 64, "less contrast lifts the dark: {less:?}");
    // Shadows lift dark parts and leave light ones; highlights pull light parts back.
    let lifted = through(&c, [40, 40, 40], with(|v| v[3] = 80.0));
    assert!(lifted[0] > 50, "{lifted:?}");
    assert!(near(through(&c, [235, 235, 235], with(|v| v[3] = 80.0)), [235, 235, 235], 3));
    let back = through(&c, [235, 235, 235], with(|v| v[2] = -80.0));
    assert!(back[0] < 228, "{back:?}");
    assert!(near(through(&c, [30, 30, 30], with(|v| v[2] = -80.0)), [30, 30, 30], 3));
    // Saturation: -100 is grey; more pulls the colours apart, hue kept.
    let orange = [200, 120, 40];
    let g = through(&c, orange, with(|v| v[4] = -100.0));
    assert!(g[0].abs_diff(g[1]) <= 2 && g[1].abs_diff(g[2]) <= 2, "grey: {g:?}");
    let s = through(&c, orange, with(|v| v[4] = 50.0));
    assert!(s[0] > orange[0] && s[2] < orange[2] && s[0] > s[1] && s[1] > s[2], "{s:?}");
    // Vibrance moves a dull colour more than a vivid one.
    let dull = [140, 120, 110];
    let vivid = [230, 60, 20];
    let spread = |p: [u8; 4]| i32::from(p[0]) - i32::from(p[2]);
    let dull_gain = spread(through(&c, dull, with(|v| v[5] = 80.0))) - spread([dull[0], 0, dull[2], 0]);
    let vivid_gain = spread(through(&c, vivid, with(|v| v[5] = 80.0))) - spread([vivid[0], 0, vivid[2], 0]);
    assert!(dull_gain > 0 && dull_gain * 2 > vivid_gain.max(0), "dull {dull_gain}, vivid {vivid_gain}");
    // Temperature: warmer is redder and less blue; cooler the other way. Tint + is magenta.
    let warm = through(&c, grey, with(|v| v[6] = 60.0));
    assert!(warm[0] > warm[2] + 10, "{warm:?}");
    let cool = through(&c, grey, with(|v| v[6] = -60.0));
    assert!(cool[2] > cool[0] + 10, "{cool:?}");
    let magenta = through(&c, grey, with(|v| v[7] = 60.0));
    assert!(magenta[1] + 6 < magenta[0], "{magenta:?}");
}

/// OBS's v2 Color Correction contrast (`color_correction_filter_update_v2`): the colour times
/// `1/(1 - c)` below 0 and `c + 1` above, no pivot, in the light the filter works in (linear, as
/// the engine composes; the 8-bit values below are its sRGB encoding). An imported OBS scene looks
/// as it does in OBS: v2 contrast -0.5 turns a linear 0.8 pixel (sRGB 231) into 0.533 (193) and a
/// linear 0.2 pixel (124) into 0.133 (102).
#[test]
fn color_correction_contrast_is_obs_v2_a_plain_multiply() {
    let Some(c) = gpu() else { return };
    let cc = |contrast: f32| ferrocore::Effect::ColorCorrection {
        gamma: 0.0,
        contrast,
        brightness: 0.0,
        saturation: 0.0,
        hue_shift: 0.0,
        opacity: 1.0,
        multiply: 0x00FF_FFFF,
        add: 0,
    };
    assert!(near(through(&c, [150, 150, 150], cc(0.0)), [150, 150, 150], 2), "0 changes nothing");
    // +0.5 multiplies the light by 1.5: sRGB 60 → 74, 100 → 121 (every grey brighter, as OBS).
    let (a, b) = (through(&c, [60, 60, 60], cc(0.5)), through(&c, [100, 100, 100], cc(0.5)));
    assert!(near(a, [74, 74, 74], 2), "{a:?}");
    assert!(near(b, [121, 121, 121], 2), "{b:?}");
    // -0.5 multiplies it by 1/1.5: linear 0.8 → 0.533 (231 → 193), 0.2 → 0.133 (124 → 102).
    let (l, d) = (through(&c, [231, 231, 231], cc(-0.5)), through(&c, [124, 124, 124], cc(-0.5)));
    assert!(near(l, [193, 193, 193], 2), "0.8 → 0.533: {l:?}");
    assert!(near(d, [102, 102, 102], 2), "0.2 → 0.133: {d:?}");
    // Down to OBS's -4.0 (multiplier 0.2) it only darkens, never inverts.
    for neg in [-2.0f32, -4.0] {
        let (d, l) = (through(&c, [40, 40, 40], cc(neg)), through(&c, [220, 220, 220], cc(neg)));
        assert!(d[0] < 40 && l[0] < 220 && d[0] < l[0], "contrast={neg}: d={d:?}, l={l:?}");
    }
}

/// ChromaKey and ColorKey with smoothness/spill at 0 (avoiding 0/0 NaN) key out the key colour
/// and preserve foreground pixels cleanly.
#[test]
fn chroma_and_color_key_zero_smoothness_stay_clean() {
    let Some(c) = gpu() else { return };
    let ck = ferrocore::Effect::ChromaKey {
        key: [0, 255, 0],
        similarity: 400.0,
        smoothness: 0.0,
        spill: 0.0,
        opacity: 1.0,
        contrast: 0.0,
        brightness: 0.0,
        gamma: 0.0,
    };
    let keyed = through(&c, [0, 255, 0], ck.clone());
    let kept = through(&c, [200, 40, 40], ck);
    assert_eq!(keyed[3], 0, "green keyed out cleanly at smoothness=0: {keyed:?}");
    assert_eq!(kept[3], 255, "red foreground stays opaque at smoothness=0: {kept:?}");
    assert!(kept[0] > 180, "red foreground stays clean (no NaN black): {kept:?}");

    let col = ferrocore::Effect::ColorKey {
        key: [0, 255, 0],
        similarity: 80.0,
        smoothness: 0.0,
        opacity: 1.0,
        contrast: 0.0,
        brightness: 0.0,
        gamma: 0.0,
    };
    let keyed2 = through(&c, [0, 255, 0], col.clone());
    let kept2 = through(&c, [200, 40, 40], col);
    assert_eq!(keyed2[3], 0, "ColorKey green keyed out at smoothness=0: {keyed2:?}");
    assert_eq!(kept2[3], 255, "ColorKey red stays opaque at smoothness=0: {kept2:?}");
    assert!(kept2[0] > 180, "ColorKey red stays clean: {kept2:?}");
}

/// Lifting the shadows on near-black camera noise brightens it as grey, not as a coloured
/// speck (the brightness ratio is capped there).
#[test]
fn adjust_lifts_dark_noise_as_grey() {
    let Some(c) = gpu() else { return };
    let got = through(&c, [3, 0, 0], with(|v| v[3] = 100.0));
    let spread = got[0].max(got[1]).max(got[2]) - got[0].min(got[1]).min(got[2]);
    assert!(spread <= 24, "a dark speck stays near grey: {got:?}");
}

/// The first use of each transition kind costs no more than the next ones: its shader pipeline is
/// built when the Compositor is (a swipe and a slide used to build theirs mid-transition, a late
/// frame and an 83 ms / 50 ms hole in the stream the first time each was used, 20 Nov). Checked
/// as what that is, not by a clock (a slow or virtual graphics card, or a busy runner, stretches
/// every time but not this): no transition pipeline is built by the first use of any kind.
#[test]
fn the_first_use_of_each_transition_costs_no_more_than_the_next() {
    use ferrocore::{Direction, Transition, TransitionKind};
    use std::time::Instant;
    let luma = Picture::solid(16, 16, [128, 128, 128, 255]);
    let kinds: Vec<(&str, TransitionKind)> = vec![
        ("swipe", TransitionKind::Swipe { direction: Direction::Left, swipe_in: false }),
        ("slide", TransitionKind::Slide(Direction::Left)),
        ("fade", TransitionKind::Fade),
        ("fade to colour", TransitionKind::FadeToColor { rgb: [0, 0, 0], switch_point: 0.5 }),
        ("luma wipe", TransitionKind::LumaWipe { luma, invert: false, softness: 0.1 }),
    ];
    for (name, kind) in kinds {
        let Some(c) = gpu() else { return };
        let scene = |rgba| Scene {
            width: 1280,
            height: 720,
            items: vec![item(Picture::solid(1280, 720, rgba), Placement::default())],
        };
        let mk =
            |t: f32| Transition { kind: kind.clone(), t, from: scene([255, 0, 0, 255]), to: scene([0, 0, 255, 255]) };
        // The engine has drawn frames before a transition: warm the scene drawing and the readback.
        for _ in 0..3 {
            let _ = c.render_transition_nv12(&Transition { kind: TransitionKind::Cut, t: 0.0, ..mk(0.0) });
        }
        let built = c.transition_pipeline_count();
        assert!(built > 0, "{name}: the compositor builds the transitions' pipelines when it is made");
        let t0 = Instant::now();
        let _ = c.render_transition_nv12(&mk(0.3)).unwrap_or_else(|e| panic!("{e}"));
        let first = t0.elapsed();
        for i in 0..8 {
            let _ = c.render_transition_nv12(&mk(0.35 + i as f32 * 0.05)).unwrap_or_else(|e| panic!("{e}"));
        }
        // For the log only: how long the first and a later one took here.
        eprintln!("{name}: first {first:?}");
        assert_eq!(
            c.transition_pipeline_count(),
            built,
            "{name}: its first use built a pipeline (mid-transition, that is a late frame)"
        );
    }
}
