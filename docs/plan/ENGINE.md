# Ferrocore: Ferrocast's own engine

The engine is named **Ferrocore** (2 Nov 2026); its code is `crates/ferrocore`. The plan below was written
while it was still called "our own engine".

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

Working name: **Ferrocast Engine** (crate `crates/ferrocore`; name to pick).

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

### Status

**1 Nov 2026 (one night's run): E0 done; E1 built; E2's compositor matches libobs; E3 and E4
started; the engine core runs with no libobs at all.** All in `crates/ferrocore` (`ferrocore`), not
used by the app yet.

Checked against libobs by `cargo run -p ferrocast-studio --example engine_match -- <dir>
[opengl]` (pixels more than 2 of 255 apart, out of 921,600):

| What libobs does | Our engine | Metal | OpenGL |
|---|---|---|---|
| Placement: move, scale, turn, alignment, crop, every bounds kind | `transform.rs` (libobs's own sums) | 0 | 0 |
| Mixing: sRGB-aware in linear light, older sources (Text) in gamma, premultiplied images | `compose.rs` | 0 | 0 |
| Scale filters: point, bilinear, bicubic, lanczos, area (down and up), low-res | `draw.wgsl` (libobs's effects ported) | 0 | 0 |
| Blend modes (7), sRGB-off blending; Metal's alpha rule differs from D3D/GL's, both matched | `compose.rs` | 0 | 0 |
| Groups, nested scenes (cropped, blended) | `compose.rs` | 0 | 0 |
| Ferrocast's filters: rounded, border, shadow, shape, blur/pixelate, looks (chained) | `effects.wgsl` | 0 | — |
| OBS's filters: Chroma Key, Color Correction, Color Key, Luma Key, Sharpen (v2, linear light). Not yet copied: libobs's "bypass" for a lone OBS filter on a scaled image (it filters at the final size; ours filters, then scales: edges differ slightly) | `effects.wgsl` | 0 | — |
| Transitions: fade, fade to colour, slide, swipe, luma wipe (held at 40% by the T-bar) | `transition.wgsl` | 0 | — |
| NV12 for encoders (BT.709 limited, chroma sited left) | `render_nv12` | 0 | — |

| Step | Built | Checked |
|---|---|---|
| E1 outputs | VideoToolbox H.264/HEVC (CBR, average, quality), AudioToolbox AAC (priming compensated), FLV (classic and Enhanced RTMP HEVC), crash-safe fragmented MP4, `Pipeline` into Go live's hub (`Hub::start/video/audio/end` now public), `Studio::program_feed` (libobs's frames handed over) | `own_stream`: 25 min over real RTMP and to MP4, all 45,002 frames, sync within 2 ms of libobs's own recording throughout |
| E3 audio | `mix.rs`: timestamps (70 ms jitter smoothing, as libobs), any rate to 48 kHz (rubato), volume, mute, balance (sine law), mono, sync offset, 6 tracks, levels; `afx.rs`: gain, invert, noise gate, compressor, limiter, expander/gate, upward compressor, 3-band EQ, ported from obs-filters line for line | unit tests (placement to the sample, pitch kept, an hour of blocks without drift); not yet compared against libobs's output |
| E4 sources | image (PNG, JPEG, animated GIF with its timing, WebP, BMP), text (cosmic-text: any font, emoji in colour, RTL and Indic scripts, outline, shadow, gradient, wrap, align, chat-log mode), screen and window capture (ScreenCaptureKit), cameras and microphones (AVFoundation), media files (AVAssetReader, looping) | screen capture run (stats only); media: every beep within 6 ms of its flash over 25 s and two loops (libobs's media source drifted −20 to +63 ms on the same kind of clip); cameras and microphones built, not run yet |
| E7 core | `engine.rs`: sources, scenes, cut and transitions, outputs with a canvas each (size, scene, rate divisor, track), its own frame loop on the host clock | `own_engine`: screen + text + test pattern, no libobs, over RTMP and to MP4: sync within 1 ms |

Speed: a 1080p frame (background, game, camera uploaded each frame with rounded corners and
border, a turned bicubic logo, a lower third) drawn and read back as NV12 in **2.7 ms** on this
Mac (`cargo run --release -p ferrocore --example bench`), a sixth of a 60 fps frame.

**1 Nov 2026 (day): in the app, and further.**

| What | Where | Checked |
|---|---|---|
| The "Ferrocast's own encoder" switch (beta, off by default): streams and recordings encoded by our engine while libobs draws; anything ours doesn't do yet falls back to OBS's encoder and logs why; an own recording refuses pause, split and chapters with a message | `crates/studio/src/own.rs`, Studio settings → More options (dashboard) | `own_switch`: stream and recording at once through `Studio::stream` / `record_to` |
| Encoding through FFmpeg where there's no native encoder (Windows, Linux; forced with `FERROCAST_AV_ENCODER=ffmpeg`): NVENC, Quick Sync, AMF, Media Foundation or x264, whichever works; the app's own FFmpeg is passed in | `ffenc.rs` | encoder tests through FFmpeg on the Mac (VideoToolbox and x264), and a Windows CI job; media sync within a frame (x264 14 ms) |
| What the Mac plays, captured with the screen (no driver) | `apple/screen.rs` | 2 s of a quiet tone: 96,000 frames at -22.7 dB |
| Monitoring: sources heard here only, or here and sent, on the system's or a named device | `monitor.rs`, `mix.rs` | a quiet beep on this Mac's speakers |
| OBS's own filters: Chroma Key, Color Correction, Color Key, Luma Key, Sharpen | `effects.wgsl` | engine_match: 0 pixels apart |
| Item opacity (a group fades as one); keyframes (position, size, rotation, opacity; eased, held, looping); the Move transition built in; stingers | `compose.rs`, `engine.rs` | unit tests in the engine |
| Cameras and microphones run on real devices | `apple/camera.rs` | USB camera 1920×1080; iPhone mic 48 kHz |
| OBS scene collections open in the engine: colour, image, text, media, screen, camera and microphone sources; every item setting (place, turn, scale, crop, bounds, blend, scale filter, shown); scenes inside scenes; picture and sound filters; volume, balance, mono, sync, tracks, monitoring. Whatever it can't run yet (a browser source, an unported filter) is listed by name | `obs.rs`, `Engine::load_obs` | `obs_import`: three scenes made in libobs, saved, opened by the engine: 0 pixels apart |
| Groups in imported collections (OBS's backup copies of their items skipped) | `obs.rs`, `engine.rs` | `obs_import` Grouped: a turned, scaled group, 313 edge pixels of 921,600 apart |
| More of OBS's filters: Crop/Pad, Image Mask/Blend (all five kinds), Apply LUT (PNG and .cube, 1D and 3D, amount, domain, alpha passed through), Scroll (wrapping, or scrolling away; its position from the scene's clock, so dropped frames don't make it drift) | `effects.wgsl`, `lut.rs`, `compose.rs` | `obs_import`: worst difference 1 (Scroll compared at rest) |
| Only what's on air is heard (and monitored), as libobs mixes its program's tree; OBS's Desktop Audio and Mic/Aux come over as global sources; the Mac's sound without a screen picture | `engine.rs`, `obs.rs` | unit tests (a tone heard only when its scene, or a scene showing it, is on air); `capture --sound --tiny` |
| **The engine draws for the app** (switch "Ferrocast's own engine", beta, off by default): streams and recordings drawn, mixed and encoded with no libobs frames; the Studio's scene changes follow within a quarter second, other edits reopen the scenes within half a second, sources that didn't change running on | `own.rs` (`Collection::stream_drawn`, `record_drawn`, `follow`), the desktop app, dashboard | `own_draw`: two scenes over real RTMP, a switch and an edit mid-stream: every frame and the sound arrive, each part 0.5-0.7 steps on average from libobs's own picture |
| A new bitrate while live (VideoToolbox), for both own paths | `apple/vt.rs` | 11,976 kbps, then 2,166 a second after asking for 2,000 |

CI: Linux's software OpenGL can't give a texture two views, so the compositor refuses it with
a reason (tests skip there); macOS and Windows run every engine test.
Still to do: Windows capture (WGC, WASAPI, Media Foundation cameras) and native encoders (FFmpeg
covers encoding there for now); the engine drawing for the app (E7: it opens the app's
collections now; next it takes over the preview and output); zero-copy (IOSurface and shared
D3D textures from capture to encoder); the preview drawn by the engine too (libobs still
draws it); edits applied in place instead of reopening the scenes; the rest of libobs's filters (Scale/Aspect Ratio, render
and video delay, HDR tone mapping); sound filters compared against libobs's output; web pages;
game capture (E5); helper processes (E6).

**1 Nov 2026 (afternoon): our engine only, branch `own-engine-only`.** The owner chose to
remove libobs with no fallback. Done on the branch (each checked as noted in its commit):

| What | Notes |
|---|---|
| Every stream and recording through our engine, no fallback | what it can't do is refused with the reason |
| Constant quality; x264 and Lossless through FFmpeg (fetched by itself when picked) | lossless round trip gives back every pixel |
| Pause, split (by hand, minutes, megabytes), MKV, up to six sound tracks | real files checked with ffprobe |
| Output size (scaled), stream delay, the vertical stream, the replay buffer, Source Record | |
| One engine shared by every output and preview | cameras and microphones open once |
| The preview, Studio Mode's on-air view, the vertical preview, the Scenes grid's tiles (Mac) | checked in a real window: edits within 0.4 s, outlines and handles |
| Fixed on the way (also on main): FFmpeg recoloured our frames (BT.601 taken for BT.709); the mixer dropped queued sound at a gap | |

**The scene store without libobs (step 3): done.** `crates/studio/src/stage.rs` keeps the
collection as an OBS-format document edited in Rust and hands it to the engine after each edit
(unchanged sources keep running); it answers every call the app makes on a collection, and its
outputs and views answer as the app's streams, recordings and previews. Kinds and their forms
are our engine's own (`kinds.rs`), recording names follow OBS's patterns (`naming.rs`), stats
come from the system (`system.rs`), undo keeps whole snapshots, and recordings are remuxed by
FFmpeg. **The app's Studio starts with no libobs**: run from a folder with no OBS files, it
records H.264 and AAC at 1080p with no libobs loaded.

**Then (1 Nov evening):** web page sources, projectors and the multiview, media play/pause/seek,
network media and guests (FFmpeg reads the stream), chapter marks in MP4 and MKV, Twitch's VOD
track (Enhanced RTMP multitrack audio), Strong noise removal (DeepFilterNet as an engine sound
plugin) and Remove background (Vision's person mask into a Cutout effect) all run on our engine.

**libobs is gone from the branch:** its code (the old `Studio`, `Collection`, streams,
recordings, filters, ~11,000 lines), the libobs examples, the bundled OBS runtime
(`deploy/obs-runtime-*`), the `obs-ffmpeg-mux` helper and the OBS-only Windows CI jobs.

Left: Windows capture and preview (step 6; Remove background there waits on it, its RVM model
kept in `ffi/matte.rs`); SRT, RIST and WHIP sends; the virtual camera (needs Apple signing).

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
