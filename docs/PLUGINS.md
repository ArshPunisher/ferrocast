# Making a Ferrocast plugin

A plugin is a `.zip` that streamers add in **Settings › Plugins**. Ferrocast plugins are safe
by design: an **effect** runs on the graphics card with only the picture and two sliders. It
can't read files, keys or the network. (OBS plugins are programs with full access to the
computer; Ferrocast doesn't load those.)

## What's in the zip

```
plugin.json
effect.wgsl
README.md        (optional)
icon.png         (optional)
```

Up to 20 files, 512 KB each, 2 MB in all. Only `.json`, `.wgsl`, `.md`, `.txt` and `.png`.
The files may sit in one folder inside the zip.

`plugin.json`:

```json
{
  "id": "com.yourname.retro-crt",
  "name": "Retro CRT",
  "version": "1.0.0",
  "author": "Your name",
  "description": "Scanlines and a soft glow.",
  "kind": "effect",
  "effect": "effect.wgsl"
}
```

- `id`: 3 to 64 small letters, digits, `.`, `-`, `_`, starting with a letter. A reverse domain
  you own is a good id. Installing a plugin with the same id updates it.
- `name` up to 60 characters, `version` up to 20, `author` up to 60, `description` up to 200.

## The effect

One WGSL function that returns the colour of each point of the picture:

```wgsl
fn effect(uv: vec2<f32>) -> vec4<f32> {
    let c = pic(uv);
    let strength = params().x;
    return vec4<f32>(mix(c.rgb, 1.0 - c.rgb, strength), c.a);
}
```

What it can use:

| | |
|---|---|
| `pic(uv)` | The picture at `uv` (0 to 1 each way), straight colour |
| `size()` | The picture's size in pixels |
| `time()` | Seconds since the scene came on |
| `params()` | The sliders: `x` is **Strength**, `y` is **Size or speed** (0 to 1) |

You may add your own helper functions and `for` loops with a fixed count. `loop` and `while`
aren't allowed (an effect must finish every frame), nor `@group`, `@binding`, `@vertex`,
`@fragment` and `@compute`: Ferrocast provides those. The effect is compiled when the plugin is
added; one that doesn't compile is refused with the first problem.

## Trying it

Write the effect in a shader filter first: add **Effects** to a source, pick **Your own**, and
paste it in. When it looks right, zip it with its `plugin.json` and add it in
**Settings › Plugins**.
