// OBS's scene transitions (plugins/obs-transitions/data/*.effect), drawn over the whole canvas
// from the two scenes' pictures. Which views (raw or sRGB-decoded) the textures and target are
// seen through is chosen per transition, as the plugins choose it.

struct T {
    // Fade amount, or swipe/slide progress, or luma progress.
    t: f32,
    softness: f32,
    invert: f32,
    _pad: f32,
    a_dir: vec2<f32>,
    b_dir: vec2<f32>,
    color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: T;
@group(0) @binding(1) var tex_a: texture_2d<f32>;
@group(0) @binding(2) var tex_b: texture_2d<f32>;
@group(0) @binding(3) var tex_l: texture_2d<f32>;
@group(0) @binding(4) var pick: sampler;

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    // A strip over the canvas: top left, top right, bottom left, bottom right.
    let uv = vec2<f32>(f32(i & 1u), f32(i >> 1u));
    var o: Out;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}

fn outside(uv: vec2<f32>) -> bool {
    return (uv.x - clamp(uv.x, 0.0, 1.0) != 0.0) || (uv.y - clamp(uv.y, 0.0, 1.0) != 0.0);
}

// fade_transition.effect (Fade, on raw values: OBS's sRGB canvas fades non-linearly).
@fragment
fn fs_fade(i: Out) -> @location(0) vec4<f32> {
    let a = textureSample(tex_a, pick, i.uv);
    let b = textureSample(tex_b, pick, i.uv);
    return mix(a, b, u.t);
}

// fade_to_color_transition.effect (non-linear: raw values and a raw colour).
@fragment
fn fs_fade_to_color(i: Out) -> @location(0) vec4<f32> {
    return mix(textureSample(tex_a, pick, i.uv), u.color, u.t);
}

// slide_transition.effect.
@fragment
fn fs_slide(i: Out) -> @location(0) vec4<f32> {
    let a_uv = i.uv + u.a_dir;
    let b_uv = i.uv - u.b_dir;
    let a = textureSample(tex_a, pick, a_uv);
    let b = textureSample(tex_b, pick, b_uv);
    return select(a, b, outside(a_uv));
}

// swipe_transition.effect (`a_dir` is the swipe value).
@fragment
fn fs_swipe(i: Out) -> @location(0) vec4<f32> {
    let s_uv = i.uv + u.a_dir;
    let a = textureSample(tex_a, pick, s_uv);
    let b = textureSample(tex_b, pick, i.uv);
    return select(a, b, outside(s_uv));
}

// luma_wipe_transition.effect.
@fragment
fn fs_luma(i: Out) -> @location(0) vec4<f32> {
    let a = textureSample(tex_a, pick, i.uv);
    let b = textureSample(tex_b, pick, i.uv);
    var luma = textureSample(tex_l, pick, i.uv).x;
    if u.invert > 0.5 {
        luma = 1.0 - luma;
    }
    let time = mix(0.0, 1.0 + u.softness, u.t);
    if luma <= time - u.softness {
        return b;
    }
    if luma >= time {
        return a;
    }
    return mix(a, b, (time - luma) / u.softness);
}
