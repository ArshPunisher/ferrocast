# Shader effects

Ferrocast's **Shader Effect** filter runs a small WGSL function on each point of a source's
picture, as OBS's Shaderfilter plugin does. Pick one of these files in the filter (Shader File),
or paste your own in its box.

An effect is one function:

```wgsl
fn effect(uv: vec2<f32>) -> vec4<f32> {
    return pic(uv);
}
```

- `uv` is the point, 0 to 1 across and down.
- `pic(uv)` reads the picture there (red, green, blue, alpha, 0 to 1).
- `time()` is the seconds since the scene came on, `size()` the picture's size in pixels.
- `params()` holds the filter's four sliders (`.x`, `.y`, `.z`, `.w`, 0 to 1).

A shader that doesn't compile is left out (the picture shows as it is) and the log says why.

| File | What it does | Sliders |
|---|---|---|
| `invert.wgsl` | Inverts the colours | — |
| `wave.wgsl` | A gentle wave across the picture | 1 strength, 2 speed |
| `vignette.wgsl` | Darker corners | 1 strength, 2 size |
| `pixelate.wgsl` | Big pixels | 1 pixel size |
| `crt.wgsl` | Scanlines and a slight colour split, like an old screen | 1 lines, 2 split |
