// A gentle wave across the picture. Slider 1: strength, slider 2: speed.
fn effect(uv: vec2<f32>) -> vec4<f32> {
    let p = params();
    let shift = sin(uv.y * 25.0 + time() * (1.0 + p.y * 8.0)) * p.x * 0.03;
    return pic(vec2<f32>(uv.x + shift, uv.y));
}
