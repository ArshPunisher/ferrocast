// Darker corners. Slider 1: strength, slider 2: size of the bright middle.
fn effect(uv: vec2<f32>) -> vec4<f32> {
    let p = params();
    let c = pic(uv);
    let d = distance(uv, vec2<f32>(0.5, 0.5));
    let dark = smoothstep(0.2 + p.y * 0.5, 0.9, d) * p.x;
    return vec4<f32>(c.rgb * (1.0 - dark), c.a);
}
