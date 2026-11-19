// Scanlines and a slight colour split, like an old screen. Slider 1: lines, slider 2: split.
fn effect(uv: vec2<f32>) -> vec4<f32> {
    let p = params();
    let split = p.y * 0.006;
    let r = pic(uv + vec2<f32>(split, 0.0)).r;
    let g = pic(uv).g;
    let b = pic(uv - vec2<f32>(split, 0.0)).b;
    let a = pic(uv).a;
    let line = 1.0 - p.x * 0.35 * (0.5 + 0.5 * sin(uv.y * size().y * 3.14159));
    return vec4<f32>(vec3<f32>(r, g, b) * line, a);
}
