// Drawing one item, as libobs does. The vertex stage places the quad on the canvas; each
// fragment entry point is one of libobs's effects: its default draw (bilinear or point,
// premultiplying straight alpha), and its scale filters (bicubic, lanczos, area, area upscale,
// low-res bilinear), ported from libobs/data/*.effect without their undistort and HDR variants.
// Colours come out premultiplied; the pipeline's blend state is the item's blend mode.

struct Item {
    canvas: vec2<f32>,
    // 1 when the picture's alpha is straight (the default draw multiplies it in).
    straight: f32,
    _pad: f32,
    // The picture's size in pixels, and one over it.
    base_dimension: vec2<f32>,
    base_dimension_i: vec2<f32>,
};

@group(0) @binding(0) var<uniform> item: Item;
@group(0) @binding(1) var image: texture_2d<f32>;
@group(0) @binding(2) var pick: sampler;

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@location(0) at: vec2<f32>, @location(1) uv: vec2<f32>) -> Out {
    var o: Out;
    o.pos = vec4<f32>(at.x / item.canvas.x * 2.0 - 1.0, 1.0 - at.y / item.canvas.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}

@fragment
fn fs_default(i: Out) -> @location(0) vec4<f32> {
    let c = textureSample(image, pick, i.uv);
    if item.straight > 0.5 {
        return vec4<f32>(c.rgb * c.a, c.a);
    }
    return c;
}

fn load(p: vec2<i32>) -> vec4<f32> {
    return textureLoad(image, p, 0);
}

// ---- bicubic_scale.effect (B=0, C=0.75) ----

fn weight4(x: f32) -> vec4<f32> {
    return vec4<f32>(
        ((-0.75 * x + 1.5) * x - 0.75) * x,
        (1.25 * x - 2.25) * x * x + 1.0,
        ((-1.25 * x + 1.5) * x + 0.75) * x,
        (0.75 * x - 0.75) * x * x);
}

@fragment
fn fs_bicubic(i: Out) -> @location(0) vec4<f32> {
    let bd = item.base_dimension;
    let bdi = item.base_dimension_i;
    let pos = i.uv * bd;
    let pos1 = floor(pos - 0.5) + 0.5;
    let f = pos - pos1;
    let rowtaps = weight4(f.x);
    let coltaps = weight4(f.y);
    let uv1 = pos1 * bdi;
    let uv0 = uv1 - bdi;
    let uv2 = uv1 + bdi;
    let uv3 = uv2 + bdi;

    let u_weight_sum = rowtaps.y + rowtaps.z;
    let u_middle = uv1.x + rowtaps.z * bdi.x / u_weight_sum;
    let v_weight_sum = coltaps.y + coltaps.z;
    let v_middle = uv1.y + coltaps.z * bdi.y / v_weight_sum;

    let tl = vec2<i32>(max(uv0 * bd, vec2<f32>(0.5)));
    let br = vec2<i32>(min(uv3 * bd, bd - 0.5));

    let s_top = textureSample(image, pick, vec2<f32>(u_middle, uv0.y));
    let s_ml = textureSample(image, pick, vec2<f32>(uv0.x, v_middle));
    let s_mm = textureSample(image, pick, vec2<f32>(u_middle, v_middle));
    let s_mr = textureSample(image, pick, vec2<f32>(uv3.x, v_middle));
    let s_bot = textureSample(image, pick, vec2<f32>(u_middle, uv3.y));

    var top = load(tl) * rowtaps.x;
    top += s_top * u_weight_sum;
    top += load(vec2<i32>(br.x, tl.y)) * rowtaps.w;
    var total = top * coltaps.x;

    var middle = s_ml * rowtaps.x;
    middle += s_mm * u_weight_sum;
    middle += s_mr * rowtaps.w;
    total += middle * v_weight_sum;

    var bottom = load(vec2<i32>(tl.x, br.y)) * rowtaps.x;
    bottom += s_bot * u_weight_sum;
    bottom += load(br) * rowtaps.w;
    total += bottom * coltaps.w;
    return total;
}

// ---- lanczos_scale.effect (a = 3) ----

fn lanczos_weight(x: f32) -> f32 {
    let x_pi = x * 3.141592654;
    return 3.0 * sin(x_pi) * sin(x_pi * (1.0 / 3.0)) / (x_pi * x_pi);
}

struct Taps {
    t012: vec3<f32>,
    t345: vec3<f32>,
};

fn weight6(f_neg: f32) -> Taps {
    var t: Taps;
    // min(1, NaN) is 1 on the GPUs libobs runs on; WGSL's min keeps NaN, so x = 0 is set apart.
    var w2 = 1.0;
    if f_neg != 0.0 {
        w2 = min(1.0, lanczos_weight(f_neg));
    }
    t.t012 = vec3<f32>(lanczos_weight(f_neg - 2.0), lanczos_weight(f_neg - 1.0), w2);
    t.t345 = vec3<f32>(lanczos_weight(f_neg + 1.0), lanczos_weight(f_neg + 2.0), lanczos_weight(f_neg + 3.0));
    let sum = t.t012.x + t.t012.y + t.t012.z + t.t345.x + t.t345.y + t.t345.z;
    let sum_i = 1.0 / sum;
    t.t012 = t.t012 * sum_i;
    t.t345 = t.t345 * sum_i;
    return t;
}

@fragment
fn fs_lanczos(i: Out) -> @location(0) vec4<f32> {
    let bd = item.base_dimension;
    let bdi = item.base_dimension_i;
    let pos = i.uv * bd;
    let pos2 = floor(pos - 0.5) + 0.5;
    let f_neg = pos2 - pos;
    let row = weight6(f_neg.x);
    let col = weight6(f_neg.y);

    let uv2 = pos2 * bdi;
    let uv1 = uv2 - bdi;
    let uv0 = uv1 - bdi;
    let uv3 = uv2 + bdi;
    let uv4 = uv3 + bdi;
    let uv5 = uv4 + bdi;

    let u_weight_sum = row.t012.z + row.t345.x;
    let u_middle = uv2.x + row.t345.x * bdi.x / u_weight_sum;
    let v_weight_sum = col.t012.z + col.t345.x;
    let v_middle = uv2.y + col.t345.x * bdi.y / v_weight_sum;

    let limit = bd - 0.5;
    let c0 = vec2<i32>(max(uv0 * bd, vec2<f32>(0.5)));
    let c1 = vec2<i32>(max(uv1 * bd, vec2<f32>(0.5)));
    let c4 = vec2<i32>(min(uv4 * bd, limit));
    let c5 = vec2<i32>(min(uv5 * bd, limit));

    let s0 = textureSample(image, pick, vec2<f32>(u_middle, uv0.y));
    let s1 = textureSample(image, pick, vec2<f32>(u_middle, uv1.y));
    let s23a = textureSample(image, pick, vec2<f32>(uv0.x, v_middle));
    let s23b = textureSample(image, pick, vec2<f32>(uv1.x, v_middle));
    let s23c = textureSample(image, pick, vec2<f32>(u_middle, v_middle));
    let s23d = textureSample(image, pick, vec2<f32>(uv4.x, v_middle));
    let s23e = textureSample(image, pick, vec2<f32>(uv5.x, v_middle));
    let s4 = textureSample(image, pick, vec2<f32>(u_middle, uv4.y));
    let s5 = textureSample(image, pick, vec2<f32>(u_middle, uv5.y));

    var row0 = load(c0) * row.t012.x;
    row0 += load(vec2<i32>(c1.x, c0.y)) * row.t012.y;
    row0 += s0 * u_weight_sum;
    row0 += load(vec2<i32>(c4.x, c0.y)) * row.t345.y;
    row0 += load(vec2<i32>(c5.x, c0.y)) * row.t345.z;
    var total = row0 * col.t012.x;

    var row1 = load(vec2<i32>(c0.x, c1.y)) * row.t012.x;
    row1 += load(vec2<i32>(c1.x, c1.y)) * row.t012.y;
    row1 += s1 * u_weight_sum;
    row1 += load(vec2<i32>(c4.x, c1.y)) * row.t345.y;
    row1 += load(vec2<i32>(c5.x, c1.y)) * row.t345.z;
    total += row1 * col.t012.y;

    var row23 = s23a * row.t012.x;
    row23 += s23b * row.t012.y;
    row23 += s23c * u_weight_sum;
    row23 += s23d * row.t345.y;
    row23 += s23e * row.t345.z;
    total += row23 * v_weight_sum;

    var row4 = load(vec2<i32>(c0.x, c4.y)) * row.t012.x;
    row4 += load(vec2<i32>(c1.x, c4.y)) * row.t012.y;
    row4 += s4 * u_weight_sum;
    row4 += load(vec2<i32>(c4.x, c4.y)) * row.t345.y;
    row4 += load(vec2<i32>(c5.x, c4.y)) * row.t345.z;
    total += row4 * col.t345.y;

    var row5 = load(vec2<i32>(c0.x, c5.y)) * row.t012.x;
    row5 += load(vec2<i32>(c1.x, c5.y)) * row.t012.y;
    row5 += s5 * u_weight_sum;
    row5 += load(vec2<i32>(c4.x, c5.y)) * row.t345.y;
    row5 += load(c5) * row.t345.z;
    total += row5 * col.t345.z;
    return total;
}

// ---- area.effect ----

@fragment
fn fs_area(i: Out) -> @location(0) vec4<f32> {
    let bd = item.base_dimension;
    let uv = i.uv;
    let uv_delta = vec2<f32>(dpdx(uv.x), dpdy(uv.y));
    let uv_min = uv - 0.5 * uv_delta;
    let uv_max = uv_min + uv_delta;
    let begin = floor(uv_min * bd);
    let end = ceil(uv_max * bd);
    let target_dimension = 1.0 / uv_delta;
    let target_pos = uv * target_dimension;
    let tmin = target_pos - 0.5;
    let tmax = target_pos + 0.5;
    let scale = item.base_dimension_i * target_dimension;
    var total = vec4<f32>(0.0);
    var y = begin.y;
    loop {
        let sy_min = y * scale.y;
        let y_min = max(sy_min, tmin.y);
        let y_max = min(sy_min + scale.y, tmax.y);
        let height = y_max - y_min;
        var x = begin.x;
        loop {
            let sx_min = x * scale.x;
            let x_min = max(sx_min, tmin.x);
            let x_max = min(sx_min + scale.x, tmax.x);
            total += (x_max - x_min) * height * load(vec2<i32>(i32(x), i32(y)));
            x += 1.0;
            if x >= end.x { break; }
        }
        y += 1.0;
        if y >= end.y { break; }
    }
    return total;
}

@fragment
fn fs_area_upscale(i: Out) -> @location(0) vec4<f32> {
    let bd = item.base_dimension;
    let bdi = item.base_dimension_i;
    var uv = i.uv;
    let uv_delta = vec2<f32>(dpdx(uv.x), dpdy(uv.y));
    let uv_min = uv - 0.5 * uv_delta;
    let uv_max = uv_min + uv_delta;
    let first = floor(uv_min * bd);
    let last = ceil(uv_max * bd) - 1.0;
    if first.x < last.x {
        let b = last.x * bdi.x;
        uv.x = ((uv.x - b) / uv_delta.x) * bdi.x + b;
    } else {
        uv.x = (first.x + 0.5) * bdi.x;
    }
    if first.y < last.y {
        let b = last.y * bdi.y;
        uv.y = ((uv.y - b) / uv_delta.y) * bdi.y + b;
    } else {
        uv.y = (first.y + 0.5) * bdi.y;
    }
    return textureSampleLevel(image, pick, uv, 0.0);
}

// ---- bilinear_lowres_scale.effect: 8 samples, Direct3D's pattern ----

@fragment
fn fs_lowres(i: Out) -> @location(0) vec4<f32> {
    let uv = i.uv;
    let s = vec2<f32>(dpdx(uv.x), dpdy(uv.y));
    let s1 = s * 0.0625;
    let s3 = s * 0.1875;
    let s5 = s * 0.3125;
    let s7 = s * 0.4375;
    var c = textureSample(image, pick, uv + vec2<f32>(s1.x, -s3.y));
    c += textureSample(image, pick, uv + vec2<f32>(-s1.x, s3.y));
    c += textureSample(image, pick, uv + vec2<f32>(s5.x, s1.y));
    c += textureSample(image, pick, uv + vec2<f32>(-s3.x, -s5.y));
    c += textureSample(image, pick, uv + vec2<f32>(-s5.x, s5.y));
    c += textureSample(image, pick, uv + vec2<f32>(-s7.x, -s1.y));
    c += textureSample(image, pick, uv + vec2<f32>(s3.x, s7.y));
    c += textureSample(image, pick, uv + vec2<f32>(s7.x, -s7.y));
    return c * 0.125;
}

// ---- NV12 out (libobs's format_conversion.effect, NV12_Y and NV12_UV): BT.709, limited
// range, from the canvas's stored (sRGB-encoded) values; chroma sited left, as libobs does. ----

struct Conv {
    // Rows of the RGB → YUV matrix, each with its offset: Y, U (Cb), V (Cr).
    y: vec4<f32>,
    u: vec4<f32>,
    v: vec4<f32>,
    // 1 / the canvas's width, and the UV plane's size.
    width_i: f32,
    _pad: f32,
    uv_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> conv: Conv;
@group(0) @binding(1) var canvas: texture_2d<f32>;
@group(0) @binding(2) var canvas_pick: sampler;

struct PosOnly {
    @builtin(position) pos: vec4<f32>,
};

@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> PosOnly {
    // One triangle over the whole target.
    let x = f32(i >> 1u) * 4.0 - 1.0;
    let y = f32(i & 1u) * 4.0 - 1.0;
    var o: PosOnly;
    o.pos = vec4<f32>(x, y, 0.0, 1.0);
    return o;
}

@fragment
fn fs_y(i: PosOnly) -> @location(0) vec4<f32> {
    let rgb = textureLoad(canvas, vec2<i32>(i.pos.xy), 0).rgb;
    return vec4<f32>(dot(conv.y.xyz, rgb) + conv.y.w, 0.0, 0.0, 1.0);
}

@fragment
fn fs_uv(i: PosOnly) -> @location(0) vec4<f32> {
    // libobs's VSTexPos_Left: between the two source pixels across, and half a pixel left.
    let uv = i.pos.xy / conv.uv_size;
    let right = textureSampleLevel(canvas, canvas_pick, uv, 0.0).rgb;
    let left = textureSampleLevel(canvas, canvas_pick, vec2<f32>(uv.x - conv.width_i, uv.y), 0.0).rgb;
    let rgb = (left + right) * 0.5;
    return vec4<f32>(dot(conv.u.xyz, rgb) + conv.u.w, dot(conv.v.xyz, rgb) + conv.v.w, 0.0, 1.0);
}
