# Making a Ferrocast plugin

A plugin is a `.zip` that streamers add in **Settings › Plugins**. Ferrocast plugins are safe
by design. (OBS plugins are programs with full access to the computer; Ferrocast doesn't load
those.) There are three kinds:

| Kind | What it is | What it can reach |
|---|---|---|
| `effect` | A WGSL effect on the picture | The picture and two sliders, on the graphics card |
| `logic` | A WebAssembly module told about events (follows, chat, raids…) | Only the steps its `plugin.json` lists |
| `panel` | A page shown in the dashboard | Only the steps its `plugin.json` lists |

None of them can read files, keys, sign-ins or the network.

## What's in the zip

```
plugin.json
effect.wgsl
README.md        (optional)
icon.png         (optional)
```

Up to 20 files, 512 KB each, 2 MB in all. Only `.json`, `.md`, `.txt` and `.png`, plus the
kind's own file: `.wgsl` (effect), `.wasm` (logic) or `.html` (panel).
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

## Logic plugins (WebAssembly)

```json
{
  "id": "com.yourname.raid-scene",
  "name": "Raid scene",
  "version": "1.0.0",
  "kind": "logic",
  "module": "plugin.wasm",
  "actions": ["scene", "chat"]
}
```

`actions` is what the plugin may ask for; the streamer sees it before adding the plugin, and
anything else it asks for is ignored. They are the steps of **Settings › Automations**:
`scene`, `show`, `hide`, `filter`, `save_replay`, `record`, `chat`.

The module (any language that builds to `wasm32-unknown-unknown`: Rust, C, Zig,
AssemblyScript…):

- exports `memory`, `alloc(len: i32) -> i32` (where Ferrocast writes an event) and
  `on_event(ptr: i32, len: i32)`;
- may import only `ferrocast.act(ptr: i32, len: i32)` (one step as JSON) and
  `ferrocast.log(ptr: i32, len: i32)` (a line for Ferrocast's log). No WASI: a module that
  imports anything else is refused.

An event, as JSON: `{"type":"raid","platform":"twitch","name":"Ana","viewers":12}`. `type` is
one of `chat_message`, `follow`, `raid`, `subscription`, `gift_batch`, `tip`, `paid_message`,
`platform_gift`, `reward_redemption`, `stream_online`, `stream_offline`…; `text`, `amount`,
`months`, `count` and `reward` come when they apply. Never ids or tokens.

A step: `{"type":"scene","name":"Raid party"}`, `{"type":"chat","text":"Welcome {name}!"}`.

Limits: each event may run about 20 million instructions and the module may use 16 MB; over
either, that call stops. Three stops in a row and the plugin is paused (it says why in
Settings). Up to 10 steps per event and 30 a minute.

In Rust:

```rust
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: i32) -> *mut u8 {
    Box::into_raw(vec![0u8; len as usize].into_boxed_slice()) as *mut u8
}

#[link(wasm_import_module = "ferrocast")]
unsafe extern "C" {
    fn act(ptr: *const u8, len: usize);
}

#[unsafe(no_mangle)]
pub extern "C" fn on_event(ptr: *const u8, len: usize) {
    // Taken back, so it's freed when this call ends (memory never shrinks otherwise).
    let owned = unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr as *mut u8, len)) };
    let event: &[u8] = &owned;
    if event.windows(6).any(|w| w == b"\"raid\"") {
        let step = br#"{"type":"scene","name":"Raid party"}"#;
        unsafe { act(step.as_ptr(), step.len()) };
    }
}
```

`cargo build --release --target wasm32-unknown-unknown` with `crate-type = ["cdylib"]`, then
zip `target/wasm32-unknown-unknown/release/<name>.wasm` as `plugin.wasm`.

## Panels

```json
{ "id": "com.yourname.buttons", "name": "Big buttons", "version": "1.0.0",
  "kind": "panel", "panel": "panel.html", "actions": ["scene", "save_replay"] }
```

`panel.html` is one page with its styles and scripts inline. It opens from **Settings ›
Plugins** in a sandboxed frame with no network: no fetches, no outside scripts, fonts or
images (use `data:` pictures). It asks for a step with

```js
parent.postMessage({ type: 'act', step: { type: 'save_replay' } }, '*')
```

and hears back `{ type: 'done' }` or `{ type: 'error', message }`. On load it's sent
`{ type: 'hello', actions }`.
