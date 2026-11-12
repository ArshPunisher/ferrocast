// The Studio's preview: the canvas placed in the view, then outlines, handles and a dimming
// layer over it. Every vertex says where it is (clip space), and either a colour (straight
// alpha, already in the view's gamma) or a place on the canvas picture.

struct In {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    // 1: the canvas picture; 0: the colour.
    @location(3) picture: f32,
}

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) picture: f32,
}

@group(0) @binding(0) var canvas: texture_2d<f32>;
@group(0) @binding(1) var pick: sampler;

@vertex
fn vs(v: In) -> Out {
    var o: Out;
    o.pos = vec4<f32>(v.pos, 0.0, 1.0);
    o.uv = v.uv;
    o.color = v.color;
    o.picture = v.picture;
    return o;
}

@fragment
fn fs(o: Out) -> @location(0) vec4<f32> {
    // How much of the canvas one pixel of the view covers (taken here, where every pixel of
    // the quad runs it).
    let d = vec2<f32>(abs(dpdx(o.uv.x)), abs(dpdy(o.uv.y)));
    if o.picture > 0.5 {
        // The canvas is premultiplied over nothing: over black (libobs's preview backdrop)
        // its colour is its premultiplied colour.
        let span = d * vec2<f32>(textureDimensions(canvas));
        if span.x <= 1.0 && span.y <= 1.0 {
            let c = textureSampleLevel(canvas, pick, o.uv, 0.0);
            return vec4<f32>(c.rgb, 1.0);
        }
        // Shown smaller than it is (a 2560 canvas in a 1100 view): every canvas pixel under
        // this one, averaged, so small text stays crisp and steady; one sample left most of
        // them out and the text shimmered and broke up (owner, 12 Nov: OBS's looked clearer).
        // Even samples, one more than the canvas pixels covered so none falls between two
        // samples (up to 8 x 8), each read bilinearly.
        let n = vec2<i32>(min(ceil(span) + 1.0, vec2<f32>(8.0)));
        let step = d / vec2<f32>(n);
        let first = o.uv - 0.5 * d + 0.5 * step;
        var c = vec3<f32>(0.0);
        for (var y = 0; y < n.y; y++) {
            for (var x = 0; x < n.x; x++) {
                c += textureSampleLevel(canvas, pick, first + step * vec2<f32>(f32(x), f32(y)), 0.0).rgb;
            }
        }
        return vec4<f32>(c / f32(n.x * n.y), 1.0);
    }
    return o.color;
}
