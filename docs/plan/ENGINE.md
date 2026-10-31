# Ferrocast's own engine: plan

Written 31 Oct 2026 at the owner's request. Nothing here is built yet; it's the plan to pick from.

Two parts:

1. **Now, on OBS's engine (libobs):** what more we can add without leaving it.
2. **Later, our own engine in Rust:** how to build one that does more than OBS, step by step,
   without users ever losing a feature on the way.

---

## Where we are

- The Studio runs OBS's engine (libobs 32.2.2) **inside** Ferrocast, on macOS and (branch
  `windows-studio`) Windows. Users don't install OBS.
- Our Rust code already talks to libobs directly (`crates/studio`, ~13,000 lines) and already
  **adds things to it from Rust**: the Web page source on macOS (WebKit), the Rounded corners
  filter (a shader), the vertical canvas, face follow, live captions, projectors, Studio Mode.
- So we know both sides: how libobs works inside, and how to extend it from Rust.

## Part 1: more on OBS's engine (no new engine needed)

libobs lets any program register its **own sources, filters, transitions and outputs**. We did
this twice already (Web page source, Rounded corners). Everything below fits that pattern, so it
ships months before an own engine and keeps working after it (the new engine reuses the ideas).

### Picture (filters and sources we write ourselves)

| Feature | What the user gets | How |
|---|---|---|
| Border | A coloured or gradient outline round a camera, rounded to match | Shader filter, beside Rounded corners |
| Drop shadow and glow | The camera "floats" over the game | Shader filter (blur of the alpha) |
| Background blur / removal | Webcam without green screen | On-device model (Core ML on Mac, DirectML/ONNX on Windows) as a filter |
| Blur or pixelate a region | Hide a phone number, a chat, a face | Shader filter with a box, or the face finder we have |
| Shape masks | Circle, hexagon, any SVG shape, no mask image to make | Shader filter (signed distance shapes) |
| LUT presets | One-click looks ("warm", "cinematic") | Ships LUT files with OBS's own LUT filter |
| Animated scene moves | Sources slide and scale between scenes (the "Move" plugin, built in) | Our code animates item transforms each frame |
| Native alerts | Alerts drawn by the engine, not a browser page: less CPU, never a white flash | A source that draws Ferrocast's alert into a texture |
| Guest cameras | A friend joins from a link in their browser, appears as a source | WebRTC (WHEP) receiver as a source |
| Per-platform pictures | Twitch gets the 16:9 scene, Shorts the vertical, Kick a version without a sponsor | More libobs canvases (we did the vertical one) |

### Sound

| Feature | What the user gets | How |
|---|---|---|
| Better noise removal | DeepFilterNet (better than RNNoise, free, Rust) | Our audio filter |
| Auto-ducking | Music drops when you talk | Our filter reading the mic's meter |
| Voice presets | "Radio voice", "Podcast" in one click | A chain of OBS's own filters, set up for them |
| VOD-safe music track | Music left out of the Twitch VOD in one switch | Audio tracks (libobs has 6) |

### Recording and outputs

| Feature | What the user gets | How |
|---|---|---|
| Record each source on its own (ISO) | Camera and game as separate files, for editing | libobs views + an output per source |
| Clips with a reason | "Clip the last 30 s" from chat, a hotkey or a hype moment | Replay buffer (we have it) + Moments |
| Bitrate that adapts per platform | One slow platform doesn't drop the others | Go live's relay already sends per platform; add per-destination bitrate steps |
| Twitch Enhanced Broadcasting | Several qualities sent at once | Multitrack output (libobs 31+) |

### Help and automation

| Feature | What the user gets | How |
|---|---|---|
| "When X, do Y" | Switch scene when a game starts, at a raid, on a timer | Ferrocast events + Studio actions (no libobs change) |
| Stream doctor | Plain words for dropped frames, overloaded encoder, a busy disk, and the fix | Stats libobs gives + our rules |
| Auto setup | Speed test and GPU check choose encoder, bitrate, size | Encoder list + upload test (Go live has one) |

### Where libobs stops us (why an own engine later)

1. **One process.** A crashing plugin, browser page or capture takes the whole app and the
   stream down. libobs can't run sources apart.
2. **Old graphics.** D3D11 and OpenGL (Metal on Mac is new and young). No compute shaders, no
   Vulkan/D3D12, effects in libobs's own shader language only.
3. **One picture size and frame rate per canvas.** Different frame rates per platform, or 4K
   recording beside a 1080p stream at no extra cost, are awkward.
4. **No animation system.** Nothing moves over time unless code moves it every frame.
5. **Fixed audio buffering** (latency of 30–100+ ms that we can't shape).
6. **Size and moving parts.** 96 MB on Mac, about 380 MB on Windows (Chromium alone ~290 MB), unpacked,
   and every OBS update can change the C structures our code mirrors.
7. **C plugins only.** Unsafe, per-platform DLLs; one bad plugin crashes everything.

---

## Part 2: our own engine in Rust

### Goal

A Rust engine that does **everything people use OBS for, plus what OBS can't**, smaller and
crash-proof, built so that **users never lose a feature** while it replaces libobs piece by piece.

Working name: **Ferrocast Engine** (crate `crates/av`; name to pick).

### The rule that makes it safe: replace piece by piece

We don't stop and rewrite. libobs keeps running and **hands its pictures to our engine** as
textures while our engine takes over one job at a time:

```
Today:        libobs does capture → mix → encode → send
Step 1:       libobs captures and mixes  →  OUR engine encodes and sends
Step 2:       libobs captures            →  OUR engine mixes, encodes, sends
Step 3:       OUR engine captures most things; libobs only for exotic sources/plugins
Step 4:       OUR engine does everything; libobs is an optional download for old OBS plugins
```

At every step the Studio looks and works the same, and every step ships on its own.

### How it's built (the parts)

| Part | What it does | Built on (to evaluate) |
|---|---|---|
| **GPU compositor** | Draws every scene: transforms, crop, blend, filters, transitions | `wgpu` (Metal, D3D12, Vulkan) and WGSL shaders, incl. compute |
| **Render graph** | Scenes become a graph; hidden things cost nothing; many canvases share sources | Our code |
| **Capture: Mac** | Screen, window, app audio, cameras | ScreenCaptureKit, AVFoundation (`objc2-*`) |
| **Capture: Windows** | Screen, window, app audio, cameras, **games** | Windows.Graphics.Capture, DXGI duplication, WASAPI process loopback, Media Foundation; games via OBS's graphics-hook protocol (GPL, compatible) |
| **Zero-copy** | Captured frames stay on the GPU from capture to encoder | IOSurface (Mac), shared D3D textures (Windows) |
| **Audio engine** | Sample-accurate mixer, tracks, monitoring, sync | `cpal`, our graph, `rubato` resampling |
| **Audio filters** | Noise removal, gate, compressor, EQ, limiter, ducking | DeepFilterNet / `nnnoiseless`, our DSP |
| **Audio plugins** | VST3 and CLAP effects, each in its own process | `clack`, `vst3` crates |
| **Encoders** | Hardware first, software as backup | VideoToolbox, NVENC, AMF, Quick Sync (direct), x264, SVT-AV1 / rav1e |
| **Outputs** | Stream and record | RTMP(S) incl. Enhanced RTMP (HEVC/AV1, multitrack), SRT, WHIP (WebRTC), crash-safe MP4, MKV, HLS |
| **Web pages** | Overlays, alerts, widgets | WKWebView (Mac, done); Windows: CEF or WebView2, chosen after a test |
| **Media** | Video and sound files | `ffmpeg` (LGPL build) or platform players |
| **Text** | Any font, emoji, right-to-left, animated text | `cosmic-text` / `parley` + glyph atlas |
| **Sandbox** | Web pages, plugins, risky captures run in helper processes | Our IPC + shared GPU textures |
| **Compatibility** | Opens OBS scene collections; old OBS plugins through the libobs bridge | Our importer (exists) + Step 1–4 bridge |

### What it does that OBS can't

**Never goes down**
- Each web page, plugin and fragile capture runs in its own process. If one crashes, its picture
  freezes for a second and comes back; **the stream never stops**.
- Undo and redo for everything, autosave, and a recovery that puts the show back after a crash
  (we have the start of this).

**Many outputs, each its own**
- Any number of canvases from the same sources: 16:9 for Twitch, 9:16 for Shorts/TikTok-style,
  a "clean" one for recording; each with its own size, **frame rate** and scenes.
- Record in 4K while streaming 1080p, for free (one capture, two encodes).
- Per-platform bitrate that adapts on its own to each platform's network.

**Movement and design built in**
- Keyframes on any property (position, size, colour, filter values) with easing; scene
  transitions that move things (Move plugin), stingers, and motion presets.
- Modern shaders (compute): real blur, glow, shadows, colour grading, HDR end to end.
- Ferrocast's alerts, chat and widgets drawn natively (no browser page needed), animated with
  the same keyframes.

**Smart, on the device**
- Background removal, auto-framing (face follow, done today on libobs), live captions and
  translation, auto-ducking, scene switch when a game starts, "clip that" from chat.
- Stream doctor that fixes things itself (lowers bitrate, swaps encoder) and says why.

**People**
- Guests join from a browser link (WebRTC), each as a source with their own sound.
- A producer can run the show from a phone or another computer (Ferrocast's relay already
  connects phones).

**Safe plugins**
- New plugins as **WebAssembly** (sandboxed, one build for every platform, can't crash the app),
  and shaders as plain WGSL files. Old OBS plugins still work through the libobs bridge.

**Small**
- Target: 30–60 MB on Mac; Windows depends on the web page choice (Chromium adds ~290 MB; WebView2
  is already on Windows and adds nothing if its off-screen capture is good enough).

### Steps (each one ships)

| Step | What | Done when |
|---|---|---|
| **E0. Spike** (2–3 weeks) | `wgpu` compositor draws one libobs scene's sources (colour, image, a camera frame handed over from libobs) | The same scene from both engines matches pixel for pixel (golden-image test, like the rounded corners check) |
| **E1. Our outputs** | libobs mixes; our engine encodes (VideoToolbox, NVENC/AMF/QSV, x264) and sends (RTMP, SRT, WHIP) and records (MP4/MKV) | A 2-hour stream and recording with no drift, on Mac and Windows |
| **E2. Our compositor** | Scenes, transforms, crop, blend, our filters (rounded, border, shadow, LUT), transitions, many canvases | Every scene in the parity list renders the same as libobs |
| **E3. Our audio** | Mixer, tracks, monitoring, sync offset, filters, VST3/CLAP in helper processes | Audio stays in sync for 4 hours; latency below libobs's |
| **E4. Our capture** | Screen, window, camera, app audio on Mac and Windows; images, media, text, colour, web pages | libobs is only needed for game capture and exotic plugins |
| **E5. Game capture** | Windows game capture through OBS's hook protocol; anti-cheat-friendly fallbacks (window capture) | The top 20 streamed games capture |
| **E6. Beyond OBS** | Keyframes, crash-proof helper processes, guests, WebAssembly plugins, HDR | Each feature on its own |
| **E7. libobs optional** | Engine default; the libobs bridge becomes an optional download for old OBS plugins | Installer without libobs; parity list fully ticked |

### Honest sizes and risks

- **OBS is about 12 years and ~600,000 lines of C/C++.** We don't need all of it: the parity list
  (STUDIO-PARITY.md) is the real target, and libobs keeps covering the rest until we're there.
- **Hardest parts:** game capture and anti-cheat, encoders across many GPUs and drivers, audio
  drift over hours, and testing on real hardware (CI machines have no GPU). We need a small
  set of real test PCs (NVIDIA, AMD, Intel, Apple) or owner tests at each step.
- **Rough time:** E0 weeks; E1–E2 a few months; the full set (E0–E5) around a year of steady
  rounds. E6 features can land early and in any order.
- **Licence:** Ferrocast is GPL-3; libobs and OBS's hook are GPL-2-or-later, so reusing their
  ideas and code is allowed. ffmpeg as an LGPL build; x264 GPL (fine for us).

### Decisions for the owner

1. **Start E0 now**, or first ship more of Part 1 on libobs (border, shadow, background removal,
   guests, native alerts)?
2. **Name** for the engine.
3. **Windows web pages:** keep Chromium (big, identical to OBS) or try WebView2 (small, needs a
   test of its off-screen capture)?
4. **Linux:** later, or in the plan from E4 (PipeWire capture)?
5. **Test hardware:** which PCs/GPUs we can test on for each step.
