// Inverts the colours.
fn effect(uv: vec2<f32>) -> vec4<f32> {
    let c = pic(uv);
    return vec4<f32>(1.0 - c.rgb, c.a);
}
