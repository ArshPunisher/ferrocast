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
    if o.picture > 0.5 {
        // The canvas is premultiplied over nothing: over black (libobs's preview backdrop)
        // its colour is its premultiplied colour.
        let c = textureSampleLevel(canvas, pick, o.uv, 0.0);
        return vec4<f32>(c.rgb, 1.0);
    }
    return o.color;
}
