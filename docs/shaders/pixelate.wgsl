// Big pixels. Slider 1: pixel size.
fn effect(uv: vec2<f32>) -> vec4<f32> {
    let px = max(2.0, params().x * 64.0);
    let cells = size() / px;
    let at = (floor(uv * cells) + 0.5) / cells;
    return pic(at);
}
