# Competing with OBS: facts, built-in plugins, and a plugin system

Written 10 Nov 2026 for the owner's morning review. Nothing here is built yet except where it
says so. Sections 2 and 3 need your picks (questions at the end).

## Progress (10 Nov, overnight)

The owner approved all of it ("make all of them, you decide the order", WebAssembly plus shaders,
no native plugins). Built, tested (unit, negative, and in the real app with recordings), committed:

| Item | State |
|---|---|
| Top layer over every scene (Downstream Keyer) | **Done**: a scene's menu, "Show over every scene" |
| Freeze filter | **Done** |
| Source Clone | **Done**: "Copy of a source" in Add a source |
| Shader gallery | **Done**: 8 ready-made effects; "Effects" in Add a filter |
| Countdown | **Done**: a source, set time or time of day |
| Plugin system, step 1: effect plugins and the plugin manager | **Done**: Settings › Plugins, `docs/PLUGINS.md`, `examples/plugins/scanlines` |
| Move, Source Record, NDI output, shape masks, area blur, vertical, caption translation to English | **Were already built** (the table below said otherwise) |
| Audio visualizer, instant replay in a scene, URL/API text, "When X, do Y", input overlay | Next, in this order |
| Plugin system steps 3 to 5 (WebAssembly logic plugins, panels, catalog) | After those |
| Windows track, encoders | Need a Windows PC to test on |

## 1. Is the comparison true?

Mostly, but four claims about OBS were wrong. Checked against OBS's own source
(`../reference/obs-studio`, Oct 2026) and our code.

| Claim | Verdict |
|---|---|
| Alerts, chat, overlays, bot, TTS and Studio in one app; OBS needs plugins and browser sources | **True** |
| Background removal needs a plugin in OBS | **Partly.** OBS has NVIDIA background removal and blur built in (`plugins/nv-filters`), but only on Windows with an RTX card. Mac, AMD and Intel users need the obs-backgroundremoval plugin. Ours works everywhere. |
| Noise removal needs a plugin in OBS | **False.** OBS has noise suppression built in (RNNoise, Speex, NVIDIA). Ours (DeepFilterNet) is a stronger model, but OBS isn't missing it. |
| Live captions need a plugin | **True** (LocalVocal). OBS only passes captions through (libcaption, Decklink). |
| Follow face and guest cameras need plugins | **True** (face tracker plugin; VDO.Ninja through a browser source). |
| Slideshow and playlist need plugins | **False.** OBS has Image Slide Show and a VLC playlist source built in. |
| Green screen | **OBS has chroma key built in.** Only our eyedropper Pick is extra. |
| Recovers instead of crashing | **True for us** (a failed frame is skipped, the watchdog restarts captures, the mixer adapts). OBS mostly shows black or freezes a source, and has no capture watchdog. |
| Safer code (Rust) | **Mostly true.** The engine is Rust, but we still call C libraries (FFmpeg, system capture APIs) and have a few small `unsafe` blocks. |
| Plain-language health | **True.** |
| 187 automated engine tests against OBS's ~4 | **True.** OBS has 4 small unit tests (`test/cmocka`) plus manual test sources. We added the sync and frame-timing tests on 10 Nov. |
| Mac: zero-copy capture, CPU 31% to 10%, frame timing now matches OBS | **True.** The timing fix (frame thread priority and App Nap off, both as OBS does) is built and measured but **not released yet**. |
| One-click setup, OBS scene import, phone dashboard | **True.** |
| OBS stronger: maturity, plugins and scripting, encoders and outputs, capture breadth | **True**, and Windows is our weakest platform (the 4 Nov real-PC test failed). |

## 2. Competing where OBS is stronger

| Gap | What "better than OBS" looks like | Size | My order |
|---|---|---|---|
| **Windows maturity** | The plan in `WINDOWS-NEXT.md`: a Windows test bench first, then game capture, encoders, installer. Without this, nothing below matters to most streamers. | Large | **1** |
| **Encoders** | NVENC, AMD AMF and Intel QSV picked for you by a quick test, with HEVC and AV1 where the platform takes them. OBS makes you choose; we choose and say why. | Medium | 2 |
| **Plugins** | Section 4: safe plugins that can't steal your keys or freeze your stream. OBS plugins can do both. | Large | 3 |
| **Capture breadth** | Windows game capture (Windows only, it touches anti-cheat), capture cards (we already read them as cameras; Decklink later), NDI output (we receive NDI already). | Medium | 4 |
| **Scripting** | Comes with the plugin system (a plugin is the script). The "When X, do Y" automation covers most of what people script OBS for, with no code. | Part of 3 | 3 |

## 3. Popular OBS plugins to build in (pick which)

What people install most (from the OBS forum's top downloads and streamer guides; my own
ranking, not measured numbers), and what we have.

| OBS plugin | What it does | We have | Effort to build in | My pick |
|---|---|---|---|---|
| **Move** (Exeldro) | Sources glide between scenes; animate any value | Planned (ENGINE.md), not built | Medium | **Yes, first** |
| **Advanced Scene Switcher** | Automation: when X happens, do Y | Partly (Ferrocast's events) | Medium | **Yes** (as "When X, do Y") |
| **Source Record** | Record one source (the webcam) on its own | No | Small (we have recording outputs per scene) | **Yes** |
| **Aitum Vertical** | Vertical canvas | **Have** | | |
| **Multistream (Aitum)** | Many platforms at once | **Have** | | |
| **Background Removal** | Remove background without green screen | **Have** | | |
| **LocalVocal** | Captions and **translation** | Captions yes, translation no | Medium | Yes (translate captions) |
| **Shaderfilter** | Custom shader effects | **Have** a shader filter | Small: a gallery of ready shaders | Yes, cheap |
| **Advanced Masks** | Shapes, rounded corners, gradient masks | Rounded corners and shadow yes; shapes no | Small | Yes |
| **Composite Blur** | Good blurs (Gaussian, motion, zoom) | Basic blur | Small | Yes |
| **Downstream Keyer** | A layer on top of every scene (logo, alerts) | No | Small | **Yes** |
| **Input Overlay** | Show keyboard, mouse, gamepad on stream | No | Medium (needs input permission on Mac) | Maybe |
| **Waveform / audio spectrum** | Music visualizer | Our music pieces partly | Small | Yes |
| **Freeze filter** | Freeze a source's picture | No | Tiny | Yes |
| **Replay source** | Instant replay inside a scene | Replay buffer yes, in-scene replay no | Medium | Yes |
| **Source Clone** | One source shown twice with different filters | No | Small | Yes |
| **URL/API source** | Show text from any web API | Partly (web source) | Small | Maybe |
| **Countdown / timer** | "Starting in 5:00" | Overlay widgets partly | Small | Yes |
| **NDI (DistroAV)** | Send and receive NDI | Receive yes, send no | Medium | Later |
| **StreamFX** | 3D transform, blurs, shaders (now abandoned and paid) | Covered by the rows above | | |
| **VST audio plugins** | Pro audio effects | No (OBS has VST built in) | Medium | Later, via the plugin system |

## 4. A plugin system: proposal

### What people can make

1. **Filters and sources that draw**: a shader (WGSL) plus a small settings file. Covers most
   OBS Shaderfilter and StreamFX plugins. No code runs on the CPU, so this is the safest kind.
2. **Logic plugins**: automation actions, chat commands, sources that make pictures or sound,
   audio effects. Compiled to **WebAssembly** (written in Rust, C, C++, Go, AssemblyScript...).
3. **Panels and overlay widgets**: HTML and JavaScript shown in the dashboard or on stream,
   isolated in a sandboxed frame.

### How it runs (my recommendation)

| Option | Like | Speed | Safety | Verdict |
|---|---|---|---|---|
| A. Native libraries (`.dylib`, `.dll`) | OBS | Full | **None**: a plugin is the app; it can read every key and file, and crash the stream | No (developer mode at most, later) |
| B. **WebAssembly in a sandbox** (wasmtime) | Figma, Zed, Envoy | Near native | Strong: only what we hand it; time and memory limits | **Yes**, for logic plugins |
| C. JavaScript in an isolated frame | Browser extensions | Fine for UI | Good with a strict sandbox | **Yes**, for panels and widgets |
| D. Shader files | Shaderfilter | GPU speed | Very good: runs on the GPU with fixed inputs | **Yes**, the first and easiest kind |

### Security: the boundaries to design against

A plugin is someone else's code. The streamer installs it, and its author is the lower-trust party.

| Risk | Design |
|---|---|
| Plugin reads stream keys, platform sign-ins, account token, `secrets.json` | WASM gets **no file system and no network** by default. The host API never exposes keys or tokens, by design and not by policy. |
| Plugin calls home or scrapes chat | Network only to hosts listed in its manifest, shown at install ("Can talk to api.example.com"), through our own fetch with its own rate limit. |
| Plugin freezes or slows the stream | It runs off the frame thread with a time budget per call (wasmtime epoch interruption) and a memory cap; over budget, it's paused and named in Stats. |
| A package writes outside its folder (zip slip) or swaps files | Unpacked like the Mac updater: no `..`, no absolute paths, no links; files hashed into the manifest. |
| A malicious update or a lookalike | Version pinned by hash; an update that asks for new permissions asks again. Later, a catalog with signed packages. |
| A panel runs script in the dashboard and steals the session | Panels in `<iframe sandbox="allow-scripts">` on a separate origin, without the dashboard token; they talk only through a narrow message API. CSP blocks everything else. |
| A widget on stream shows viewer text as HTML | Same rule as our overlays: viewer text is text only; the widget API hands text, never HTML. |
| One bad plugin takes Ferrocast down | Safe mode: start with plugins off after a crash, and each plugin can be turned off on its own. |

### Steps

1. **Shader plugins** (kind 1): a folder with `plugin.json` and a `.wgsl`; shows up under Add a filter. About a week.
2. **Plugin manager** in Settings: install from a file, permissions screen, turn on/off, safe mode.
3. **WASM runtime** (kind 2) with the first host API: settings, events in (chat, alerts, scenes), actions out (switch scene, show source, play sound). An SDK crate and three example plugins.
4. **Panels and widgets** (kind 3).
5. **A catalog** on the website, signed packages. Later.

## Questions for you

1. Section 3: which plugins to build in first? My picks: Move, Downstream Keyer, Source Record,
   Freeze, Source Clone, then automation.
2. Section 4: OK to go WebAssembly plus shaders, with **no native plugins** (unlike OBS)? It's
   safer but means OBS plugins can't be loaded as they are; authors port them.
3. Order: Windows first, as I suggest, or plugins first?
