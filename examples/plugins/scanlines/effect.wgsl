// An example Ferrocast plugin effect (docs/PLUGINS.md). Strength darkens every other band of
// lines; Size or speed sets how tall the bands are.
fn effect(uv: vec2<f32>) -> vec4<f32> {
    let c = pic(uv);
    let band = mix(2.0, 12.0, params().y);
    let row = floor(uv.y * size().y / band);
    let dark = select(1.0, 1.0 - params().x * 0.6, row % 2.0 == 1.0);
    return vec4<f32>(c.rgb * dark, c.a);
}
