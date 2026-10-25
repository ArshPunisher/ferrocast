# Spike: libobs inside Ferrocast (Track B, phase 0), macOS

25 Oct 2026, on the owner's Mac (Apple Silicon, OBS 32.2.2 installed). A 300-line C program
loads OBS's engine from `OBS.app` (no OBS window, no Qt), loads its plugins, lists every
source, filter, transition, encoder and output with its options, builds a scene and records it.

## Build and run

```sh
# headers: obs-studio tag 32.2.2 (libobs/), plus SIMDe (header-only)
git clone --depth 1 --branch 32.2.2 --filter=blob:none --sparse https://github.com/obsproject/obs-studio.git obs
(cd obs && git sparse-checkout set libobs)
git clone --depth 1 https://github.com/simd-everywhere/simde.git simde-src && mkdir -p inc && ln -s "$PWD/simde-src/simde" inc/simde
printf '#pragma once\n#define OBS_RELEASE_CANDIDATE 0\n#define OBS_BETA 0\n' > obs/libobs/obsconfig.h
clang -O1 -o spike spike.c -I obs/libobs -I inc -F/Applications/OBS.app/Contents/Frameworks \
  -framework libobs -Wl,-rpath,/Applications/OBS.app/Contents/Frameworks
./spike out/
```

## What worked

- libobs 32.2.2 starts and renders 1280x720 at 30 fps with **both** renderers (Metal and OpenGL).
- Every plugin loads except the interface-only ones (below): 22 input types, 31 filters,
  7 transitions, 16 encoders, 14 outputs, about 230 top-level options read through
  `obs_properties`, the way a generic settings renderer will.
- A scene (color source + FreeType text) recorded for 5 s through x264 + AAC into an MP4 with
  OBS's native MP4 writer (`mp4_output`): 1280x720, 30 fps, H.264 + AAC, plays correctly.

## What the Studio must do (found here)

1. **Skip interface-only plugins** (`obs_add_disabled_module`): frontend-tools,
   decklink-output-ui, aja-output-ui, decklink-captions and obs-websocket need OBS's Qt
   window and crash without it. DeckLink/AJA capture can load when the drivers exist.
2. **Read options from a live source, never from the type alone.** Camera, capture card,
   screen and some filters (Apply LUT) crash when `obs_get_source_properties(id)` is called
   without an instance; OBS's own dialog always uses `obs_source_properties(source)`.
   The generic renderer creates (private) sources first.
3. **Bundle the helpers next to our executable:** `obs-ffmpeg-mux` (the default recorder,
   ffmpeg_muxer) and the Chromium framework for the browser source are looked up relative to
   the running program, so they ship inside Ferrocast.app (as OBS.app does).
4. **Renderer:** Metal had no shader errors but 100 of 298 frames lagged when run from a plain
   command-line program; OpenGL lagged 2 of 184 but one filter shader (pow(vec3, float)) failed
   to compile. Next: run inside the app's real main thread / run loop, where OBS itself runs,
   and measure again before choosing.
5. **Shutdown order:** both runs crashed on exit after a clean recording; release outputs,
   encoders and sources and wait for the video thread before `obs_shutdown`.
6. Camera, screen and audio capture need the macOS permissions (camera, microphone, screen
   recording) for Ferrocast.app itself: Info.plist usage strings and a first-run screen.

## Next (phase 0 continues)

- The same from Rust (bindgen over libobs, or the `libobs` crates) inside the Tauri app, on its
  main run loop, with a preview drawn into the window.
- Screen capture and the webcam with permissions, then streaming to Ferrocast's own local
  ingest (Track A) instead of a file.
- Windows: the same spike against OBS's `obs.dll` (D3D11).
- Size: OBS.app's frameworks are 392 MB (Chromium 258 MB, Qt 46 MB) plus 32 MB of plugins; without Qt that is about 380 MB, most of it Chromium, which Ferrocast's overlays could share. The
  libobs core, ffmpeg and plugins alone are about 90 MB.
