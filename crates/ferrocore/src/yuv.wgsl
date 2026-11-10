// A file's picture as FFmpeg decodes it (NV12: Y, then U and V at half size) → RGB on the card,
// into the source's own RGBA texture, the same gamma-encoded bytes an RGBA upload would be. No
// RGBA copy is made on the CPU (`Compositor::upload_nv12`).

struct Yuv {
    // Each colour as dot(row.xyz, vec3(y, u, v)) + row.w: the matrix and the range, worked out
    // on the CPU for BT.709 or BT.601, TV or full range.
    r: vec4<f32>,
    g: vec4<f32>,
    b: vec4<f32>,
};

@group(0) @binding(0) var<uniform> m: Yuv;
@group(0) @binding(1) var luma: texture_2d<f32>;
@group(0) @binding(2) var chroma: texture_2d<f32>;
@group(0) @binding(3) var pick: sampler;

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
fn fs_rgb(i: PosOnly) -> @location(0) vec4<f32> {
    let y = textureLoad(luma, vec2<i32>(i.pos.xy), 0).r;
    // Chroma at this pixel's place in the half-size plane. Each chroma sample sits on the left
    // pixel of its pair across (H.264's and MPEG-2's 4:2:0, as FFmpeg reads it) and between the
    // rows: half a pixel right of the centre-sited place. Centred, a sharp edge in a picture
    // took a fringe of the colour beside it (10 Nov: colour bars, one pixel at each edge).
    let size = vec2<f32>(textureDimensions(luma));
    let c = textureSampleLevel(chroma, pick, vec2<f32>(i.pos.x + 0.5, i.pos.y) / size, 0.0).rg;
    let yuv = vec3<f32>(y, c.x, c.y);
    let rgb = vec3<f32>(dot(m.r.xyz, yuv) + m.r.w, dot(m.g.xyz, yuv) + m.g.w, dot(m.b.xyz, yuv) + m.b.w);
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
