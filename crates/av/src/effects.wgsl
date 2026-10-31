// Ferrocast's own picture filters (ferrocast-studio's ffi/shaders.rs, which libobs runs), on
// our engine. Each samples the picture before it (raw values: libobs runs these filters in
// gamma space) and returns a straight colour, multiplied by its alpha on the way out as
// libobs's filter blending (source alpha) does. A filter may be bigger than its picture by
// `pad` on every side (a shadow).

struct F {
    size: vec2<f32>,
    out_size: vec2<f32>,
    pad: f32,
    p0: f32,
    p1: f32,
    p2: f32,
    p3: f32,
    p4: f32,
    p5: f32,
    _pad: f32,
    color: vec4<f32>,
    // OBS's own filters (libobs's obs-filters effects) need more.
    q: vec4<f32>,
    r: vec4<f32>,
    m: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> f: F;
@group(0) @binding(1) var image: texture_2d<f32>;
@group(0) @binding(2) var pick: sampler;
// A second picture (Image Mask/Blend's).
@group(0) @binding(3) var second: texture_2d<f32>;

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    let uv = vec2<f32>(f32(i & 1u), f32(i >> 1u));
    var o: Out;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}

fn premultiplied(c: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(c.rgb * c.a, c.a);
}

fn src_px(uv: vec2<f32>) -> vec2<f32> {
    return uv * f.out_size - vec2<f32>(f.pad, f.pad);
}

fn src_at(px: vec2<f32>) -> vec4<f32> {
    let uv = px / f.size;
    let inside = step(0.0, uv.x) * step(0.0, uv.y) * step(uv.x, 1.0) * step(uv.y, 1.0);
    return textureSampleLevel(image, pick, uv, 0.0) * inside;
}

fn rbox(px: vec2<f32>, half_size: vec2<f32>, r: f32) -> f32 {
    let q = abs(px - half_size) - (half_size - vec2<f32>(r, r));
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

// Rounded corners: p0 radius.
@fragment
fn fs_rounded(i: Out) -> @location(0) vec4<f32> {
    let p = src_px(i.uv);
    let c = src_at(p);
    let h = f.size * 0.5;
    let d = rbox(p, h, min(f.p0, min(h.x, h.y)));
    return premultiplied(vec4<f32>(c.rgb, c.a * saturate(0.5 - d)));
}

// Border: p0 width, color, p1 radius.
@fragment
fn fs_border(i: Out) -> @location(0) vec4<f32> {
    let p = src_px(i.uv);
    let c = src_at(p);
    let h = f.size * 0.5;
    let d = rbox(p, h, min(f.p1, min(h.x, h.y)));
    let shape = saturate(0.5 - d);
    let band = shape * saturate(d + f.p0 + 0.5);
    let k = band * f.color.a;
    return premultiplied(vec4<f32>(mix(c.rgb, f.color.rgb, k), mix(c.a, 1.0, k) * shape));
}

// Shadow and glow: color, p0 blur, p1 x, p2 y.
@fragment
fn fs_shadow(i: Out) -> @location(0) vec4<f32> {
    let p = src_px(i.uv);
    let c = src_at(p);
    let q = p - vec2<f32>(f.p1, f.p2);
    var acc = 0.0;
    var total = 0.0;
    for (var n = 0; n < 48; n++) {
        let t = (f32(n) + 0.5) / 48.0;
        let ang = f32(n) * 2.39996323;
        let o = vec2<f32>(cos(ang), sin(ang)) * (sqrt(t) * f.p0);
        let w = 1.0 - t * 0.5;
        acc += src_at(q + o).a * w;
        total += w;
    }
    let sa = acc / total * f.color.a;
    let a = c.a + sa * (1.0 - c.a);
    let rgb = (c.rgb * c.a + f.color.rgb * sa * (1.0 - c.a)) / max(a, 0.0001);
    return premultiplied(vec4<f32>(rgb, a));
}

fn sd_hexagon(p_in: vec2<f32>) -> f32 {
    var p = abs(p_in);
    let k = vec2<f32>(-0.866025404, 0.5);
    p -= 2.0 * min(dot(k, p), 0.0) * k;
    p -= vec2<f32>(clamp(p.x, -0.577350269, 0.577350269), 1.0);
    return length(p) * sign(p.y);
}

fn sd_star(p_in: vec2<f32>) -> f32 {
    let k1 = vec2<f32>(0.809016994375, -0.587785252292);
    let k2 = vec2<f32>(-k1.x, k1.y);
    var p = p_in;
    p.x = abs(p.x);
    p -= 2.0 * max(dot(k1, p), 0.0) * k1;
    p -= 2.0 * max(dot(k2, p), 0.0) * k2;
    p.x = abs(p.x);
    p.y -= 1.0;
    let ba = 0.45 * vec2<f32>(-k1.y, k1.x) - vec2<f32>(0.0, 1.0);
    let h = clamp(dot(p, ba) / dot(ba, ba), 0.0, 1.0);
    return length(p - ba * h) * sign(p.y * ba.x - p.x * ba.y);
}

fn sd_heart(p_in: vec2<f32>) -> f32 {
    var p = p_in;
    p.x = abs(p.x);
    if p.y + p.x > 1.0 {
        let a = p - vec2<f32>(0.25, 0.75);
        return sqrt(dot(a, a)) - 0.353553391;
    }
    let b = p - vec2<f32>(0.0, 1.0);
    let m = p - 0.5 * max(p.x + p.y, 0.0);
    return sqrt(min(dot(b, b), dot(m, m))) * sign(p.x - p.y);
}

// Shape: p0 shape, p1 feather, p2 stretch.
@fragment
fn fs_shape(i: Out) -> @location(0) vec4<f32> {
    let p = src_px(i.uv);
    let c = src_at(p);
    let h = f.size * 0.5;
    var r = vec2<f32>(min(h.x, h.y), min(h.x, h.y));
    if f.p2 > 0.5 {
        r = h;
    }
    let n = (p - h) / r;
    let s = min(r.x, r.y);
    var d: f32;
    if f.p0 < 0.5 {
        d = length(n) - 1.0;
    } else if f.p0 < 1.5 {
        d = sd_hexagon(n * 1.1547) / 1.1547;
    } else if f.p0 < 2.5 {
        d = (abs(n.x) + abs(n.y) - 1.0) * 0.707106781;
    } else if f.p0 < 3.5 {
        d = sd_star(vec2<f32>(n.x, -n.y) * 1.1);
    } else {
        d = sd_heart(vec2<f32>(n.x * 0.6, (1.0 - n.y) * 0.55)) / 0.58;
    }
    let edge = max(f.p1, 1.0);
    return premultiplied(vec4<f32>(c.rgb, c.a * saturate(0.5 - d * s / edge)));
}

// Blur or pixelate an area: p0 mode, p1 strength, p2..p5 area x, y, w, h (percent).
@fragment
fn fs_blur_area(i: Out) -> @location(0) vec4<f32> {
    let p = src_px(i.uv);
    let c = src_at(p);
    let lo = vec2<f32>(f.p2, f.p3) * 0.01 * f.size;
    let hi = lo + vec2<f32>(f.p4, f.p5) * 0.01 * f.size;
    let inside = step(lo.x, p.x) * step(lo.y, p.y) * step(p.x, hi.x) * step(p.y, hi.y);
    if inside < 0.5 {
        return premultiplied(c);
    }
    if f.p0 > 0.5 {
        let block = max(f.p1, 1.0);
        return premultiplied(src_at((floor(p / block) + 0.5) * block));
    }
    var acc = vec4<f32>(0.0);
    for (var n = 0; n < 64; n++) {
        let t = (f32(n) + 0.5) / 64.0;
        let ang = f32(n) * 2.39996323;
        acc += src_at(clamp(p + vec2<f32>(cos(ang), sin(ang)) * (sqrt(t) * f.p1), vec2<f32>(0.0, 0.0), f.size - 1.0));
    }
    return premultiplied(acc / 64.0);
}

// Colour look: p0 look, p1 strength (percent).
@fragment
fn fs_look(i: Out) -> @location(0) vec4<f32> {
    let p = src_px(i.uv);
    let c = src_at(p);
    let x = c.rgb;
    let l = dot(x, vec3<f32>(0.2126, 0.7152, 0.0722));
    let ll = vec3<f32>(l, l, l);
    var g = x;
    if f.p0 < 0.5 {
        g = x * vec3<f32>(1.08, 1.0, 0.86);
    } else if f.p0 < 1.5 {
        g = x * vec3<f32>(0.88, 1.0, 1.1);
    } else if f.p0 < 2.5 {
        g = mix(ll, x, 1.15) + (l - 0.5) * vec3<f32>(0.16, 0.02, -0.16);
    } else if f.p0 < 3.5 {
        g = ll;
    } else if f.p0 < 4.5 {
        g = vec3<f32>(dot(x, vec3<f32>(0.393, 0.769, 0.189)), dot(x, vec3<f32>(0.349, 0.686, 0.168)), dot(x, vec3<f32>(0.272, 0.534, 0.131)));
        g = g * 0.82 + 0.08;
    } else if f.p0 < 5.5 {
        g = (x - 0.5) * 1.2 + 0.5;
        g = mix(ll, g, 1.3);
    } else {
        g = mix(ll, x, 0.8) * 0.85 + 0.1;
    }
    return premultiplied(vec4<f32>(mix(x, saturate(g), f.p1 * 0.01), c.a));
}

// ---- OBS's own filters (plugins/obs-filters/data), run in linear light like their v2 kinds ----

fn nonlinear(u: f32) -> f32 {
    if u <= 0.0031308 {
        return 12.92 * u;
    }
    return 1.055 * pow(u, 1.0 / 2.4) - 0.055;
}

fn nonlinear3(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(nonlinear(c.r), nonlinear(c.g), nonlinear(c.b));
}

const CB: vec4<f32> = vec4<f32>(-0.100644, -0.338572, 0.439216, 0.501961);
const CR: vec4<f32> = vec4<f32>(0.439216, -0.398942, -0.040274, 0.501961);

fn chroma_dist(rgb: vec3<f32>) -> f32 {
    let cb = dot(rgb, CB.xyz) + CB.w;
    let cr = dot(rgb, CR.xyz) + CR.w;
    return distance(f.q.xy, vec2<f32>(cb, cr));
}

fn sample_nl(uv: vec2<f32>) -> vec3<f32> {
    return nonlinear3(textureSampleLevel(image, pick, uv, 0.0).rgb);
}

// chroma_key_filter_v2.effect: q.xy key (Cb, Cr), q.zw pixel size; r similarity, smoothness,
// spill, opacity; p0 contrast, p1 brightness, p2 gamma.
@fragment
fn fs_chroma_key(i: Out) -> @location(0) vec4<f32> {
    var rgba = textureSampleLevel(image, pick, i.uv, 0.0);
    if rgba.a > 0.0 {
        rgba = vec4<f32>(rgba.rgb / rgba.a, rgba.a);
    } else {
        rgba = vec4<f32>(0.0, 0.0, 0.0, rgba.a);
    }
    let ps = f.q.zw;
    let hps = ps / 2.0;
    let p0 = vec2<f32>(ps.x, hps.y);
    let p1 = vec2<f32>(hps.x, -ps.y);
    var d = chroma_dist(sample_nl(i.uv - p0));
    d += chroma_dist(sample_nl(i.uv + p0));
    d += chroma_dist(sample_nl(i.uv - p1));
    d += chroma_dist(sample_nl(i.uv + p1));
    d *= 2.0;
    d += chroma_dist(nonlinear3(rgba.rgb));
    d /= 9.0;
    let base = d - f.r.x;
    let full = pow(saturate(base / f.r.y), 1.5);
    let spill = pow(saturate(base / f.r.z), 1.5);
    rgba.a *= f.r.w;
    rgba.a *= full;
    let desat = dot(rgba.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    rgba = vec4<f32>(mix(vec3<f32>(desat), rgba.rgb, spill), rgba.a);
    rgba = vec4<f32>(pow(rgba.rgb, vec3<f32>(f.p2)) * f.p0 + f.p1, rgba.a);
    return vec4<f32>(rgba.rgb * rgba.a, rgba.a);
}

// color_correction_filter.effect: p0 gamma, m the final colour matrix (libobs's rows).
@fragment
fn fs_color_correction(i: Out) -> @location(0) vec4<f32> {
    var c = textureSampleLevel(image, pick, i.uv, 0.0);
    if c.a > 0.0 {
        c = vec4<f32>(c.rgb / c.a, c.a);
    } else {
        c = vec4<f32>(0.0, 0.0, 0.0, c.a);
    }
    c = vec4<f32>(pow(c.rgb, vec3<f32>(f.p0)), c.a);
    c = f.m * c;
    return vec4<f32>(c.rgb * c.a, c.a);
}

fn unpremultiplied(c: vec4<f32>) -> vec4<f32> {
    if c.a > 0.0 {
        return vec4<f32>(c.rgb / c.a, c.a);
    }
    return vec4<f32>(0.0, 0.0, 0.0, c.a);
}

// color_key_filter_v2.effect: q.rgb the key (as stored, non-linear); r similarity, smoothness,
// opacity; p0 contrast, p1 brightness, p2 gamma.
@fragment
fn fs_color_key(i: Out) -> @location(0) vec4<f32> {
    var rgba = unpremultiplied(textureSampleLevel(image, pick, i.uv, 0.0));
    rgba.a *= f.r.z;
    let dist = distance(f.q.rgb, nonlinear3(rgba.rgb));
    rgba.a *= saturate(max(dist - f.r.x, 0.0) / f.r.y);
    rgba = vec4<f32>(pow(rgba.rgb, vec3<f32>(f.p2)) * f.p0 + f.p1, rgba.a);
    return vec4<f32>(rgba.rgb * rgba.a, rgba.a);
}

// luma_key_filter_v2.effect: r max, min, max smoothness, min smoothness.
@fragment
fn fs_luma_key(i: Out) -> @location(0) vec4<f32> {
    var rgba = unpremultiplied(textureSampleLevel(image, pick, i.uv, 0.0));
    let l = dot(rgba.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let lo = smoothstep(f.r.y, f.r.y + f.r.w, l);
    let hi = 1.0 - smoothstep(f.r.x - f.r.z, f.r.x, l);
    rgba.a *= lo * hi;
    return vec4<f32>(rgba.rgb * rgba.a, rgba.a);
}

// sharpness.effect: p0 sharpness; the picture's own pixel size.
@fragment
fn fs_sharpen(i: Out) -> @location(0) vec4<f32> {
    let d = 1.0 / f.size;
    let at = i.uv;
    let a = textureSampleLevel(image, pick, at + vec2<f32>(-d.x, -d.y), 0.0);
    let b = textureSampleLevel(image, pick, at + vec2<f32>(0.0, -d.y), 0.0);
    let c = textureSampleLevel(image, pick, at + vec2<f32>(d.x, -d.y), 0.0);
    let dd = textureSampleLevel(image, pick, at + vec2<f32>(-d.x, 0.0), 0.0);
    let e = textureSampleLevel(image, pick, at, 0.0);
    let ff = textureSampleLevel(image, pick, at + vec2<f32>(d.x, 0.0), 0.0);
    let g = textureSampleLevel(image, pick, at + vec2<f32>(-d.x, d.y), 0.0);
    let h = textureSampleLevel(image, pick, at + vec2<f32>(0.0, d.y), 0.0);
    let ii = textureSampleLevel(image, pick, at + vec2<f32>(d.x, d.y), 0.0);
    let color = 8.0 * e - a - b - c - dd - ff - g - h - ii;
    let edge = (distance(e, ff) != 0.0 && distance(e, dd) != 0.0) || (distance(e, b) != 0.0 && distance(e, h) != 0.0);
    if edge {
        return saturate(e + color * f.p0);
    }
    return e;
}

// crop_filter.effect: the output's pixels from the picture's, moved by the left and top crop
// (p0, p1); outside the picture is transparent (its sampler's border).
@fragment
fn fs_crop(i: Out) -> @location(0) vec4<f32> {
    return src_at(i.uv * f.out_size + vec2<f32>(f.p0, f.p1));
}

// mask_*_filter.effect and blend_*_filter.effect (mask-filter.c): p0 which (0 alpha mask,
// 1 colour mask, 2 multiply, 3 add, 4 subtract); color the tint; q the picture's mapping
// (multiplier, offset).
@fragment
fn fs_mask(i: Out) -> @location(0) vec4<f32> {
    var rgba = unpremultiplied(textureSampleLevel(image, pick, i.uv, 0.0)) * f.color;
    let t = textureSampleLevel(second, pick, i.uv * f.q.xy + f.q.zw, 0.0);
    let k = u32(f.p0 + 0.5);
    if k == 0u {
        rgba.a *= t.a;
    } else if k == 1u {
        rgba.a *= (t.r + t.g + t.b) / 3.0;
    } else if k == 2u {
        rgba = vec4<f32>(saturate(rgba.rgb * t.rgb), rgba.a);
    } else if k == 3u {
        rgba = vec4<f32>(saturate(rgba.rgb + t.rgb), rgba.a);
    } else {
        rgba = vec4<f32>(saturate(rgba.rgb - t.rgb), rgba.a);
    }
    return vec4<f32>(rgba.rgb * rgba.a, rgba.a);
}
