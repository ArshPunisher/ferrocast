# Live Studio: going live from Ferrocast itself

Owner decisions, 22 Oct 2026. Nothing here is built yet. Engine: **libobs** (owner confirmed). OBS feature by feature, with what we do better: `research/10-obs-feature-map.md`.

## Goal

Stream from Ferrocast without OBS, to Twitch, YouTube and Kick **at the same time**, with
scenes, sources and Ferrocast's own alerts and overlays built in. OBS support stays: people
who prefer OBS keep it, with the 1.4.2 Start / Stop streaming controls.

## Versions (owner, 22 Oct 2026)

| Version | What |
|---|---|
| 1.4.2 | OBS start/stop from Studio, YouTube description and thumbnail, theme choice, fixes |
| 1.4.3 / 1.5.0 | Only if the website + dashboard merge needs app changes (it's mostly site, server and sign-in) |
| **2.0.0** | **Go live everywhere** (Track A): multistream for OBS users and later the Studio, per-platform stream info, vertical forwarding |
| 2.0.x | Fixes |
| **2.1.0** | **Ferrocast Studio, preview** (Track B): stream without OBS, labelled preview |
| 2.1.x / 2.2 | Studio fixes, more built-in features |
| **3.0.0** | Studio ready as a daily OBS replacement, and the plugin system for other developers |

Track A is **not an OBS plugin**: nothing is installed into OBS. OBS's stream setting points at
Ferrocast (one click), and Ferrocast sends the stream on, so an OBS update can't break it.

## Decisions

| Question | Decision |
|---|---|
| Platforms for the first version | **Mac and Windows**. Linux later |
| Multistreaming | **From the streamer's PC**: one encode, sent to every platform; their upload carries each stream |
| Price | **Studio is free** (it runs on their PC, R-05/R-07). A later cloud forwarding option (one upload, our server sends it on) would be **Pro**, because it costs us bandwidth |
| When | Order: 1.4.2 → website + dashboard merge (ferrocast-web `docs/plan/STEPS.md` §9) → **Track A** → Track B (the Studio) |
| Other platforms | Twitch, YouTube, Kick by API. **Plus any custom RTMP destination** (the streamer pastes the server and key): video only, no chat, alerts or title from it; Ferrocast doesn't name or build for TikTok (R-26 stands) |

## Two tracks

Owner, 22 Oct: multistream, per-platform info and vertical must work **whether people stream
from Ferrocast or from OBS**. Research: `research/09-going-live-and-multistream.md`.

### Track A: Go live everywhere (works with OBS today, and later with the Studio)

```
OBS (or the Studio) ──one stream──▶ Ferrocast on the same PC (rtmp://127.0.0.1)
                                     ├─▶ Twitch    same packets, no re-encode
                                     ├─▶ YouTube   (almost no CPU)
                                     ├─▶ Kick
                                     └─▶ a vertical stream, if one is sent (see below)
```
- **One click to set OBS up:** Ferrocast points OBS's stream at itself through obs-websocket
  (`SetStreamServiceSettings`), and puts it back on request. No plugin.
- **Keys fetched, never pasted** where the platform allows (Twitch `channel:read:stream_key`,
  YouTube liveStreams with the sign-in we have, Kick `streamkey:read` to confirm).
- **One Go live:** pressing Start Streaming in OBS (or in Ferrocast, 1.4.2) starts every
  ticked platform; each one connects and reconnects on its own, with a health line
  (live, reconnecting, dropped) and a plain reason when it fails (R-08).
- **Before going live:** an upload check ("3 platforms at 6 Mbps need 18 Mbps; you have 12")
  and platform rules in plain words (Kick Partners' multistream toggle and payout, Twitch's
  equal-quality rule, merged chat on Twitch).
- **Stream info for every platform in one form:** title, category, tags, description and
  thumbnail, **the same everywhere by default, or different per platform** (for example
  Twitch its own title, YouTube and Kick the same, or all different). Each field shows which
  platforms take it (only YouTube takes a description and a thumbnail). Saved in presets.
- **Vertical for OBS users:** OBS sends a second, vertical stream (OBS 32's canvas with Aitum
  Vertical today) to a second local address; Ferrocast forwards it to vertical destinations
  (YouTube Shorts live, and others). Without a vertical canvas, a later option crops the
  horizontal stream on the GPU (costs one more encode).
- **Upload is the limit:** every platform is a full copy on the streamer's internet. The later
  Pro cloud forwarding (one upload, our server sends it on) answers that.
- **Rust pieces:** a small local RTMP server and RTMP/RTMPS clients (remux only), one task per
  destination under the connection supervisor (R-18), keys in the keychain (R-45).

### Track B: Ferrocast Studio (stream without OBS)

The libobs studio below. It feeds Track A's outputs directly, makes the vertical canvas
itself, and builds in what people add to OBS as plugins:

| Plugin people install today | In the Studio |
|---|---|
| Multiple RTMP outputs, Aitum Multistream | Track A, built in |
| Aitum Vertical | A vertical canvas sharing the same sources, per-orientation layouts |
| Move | Animated transitions |
| Advanced Scene Switcher | "When X, do Y" automation (events, timers, ads), growing what Ferrocast has |
| Background removal | One switch on the webcam |
| Noise suppression, audio monitor | One switch per mic, hear it while live |
| Source Record | Record the webcam (or any source) separately |
| SE.Live / Streamlabs plugin | Not needed: alerts, chat, bot are Ferrocast |

**Settings chosen for them:** a speed test and GPU check pick encoder, bitrate, resolution
and FPS, with the reason in words; "encoder overloaded" turns into a plain fix (or an automatic
step down). Advanced settings stay one click away for people who want them.

## Go live everywhere: the feature list (2.0)

**Setup, in minutes:**
- Connect Twitch, YouTube and Kick once; keys are fetched, never pasted (custom destinations: pasted).
- One click points OBS at Ferrocast, one click puts it back.
- An upload test **to each platform's ingest**, with a verdict in words ("2 platforms at 1080p,
  or 3 at 720p").

**Going live:**
- One Start (in OBS or Ferrocast) starts every ticked platform.
- Title, category, tags, description, thumbnail: the same everywhere or different per
  platform, field by field; each field shows which platforms take it.
- Platform rules before going live: Twitch's equal-quality rule, Kick Partners' toggle and payout.

**While live:**
- A health line per platform; one failing never stops the others, and it reconnects alone.
- Add or remove a platform without ending the stream.
- Chat, alerts, moderation and the bot across all of them (already Ferrocast).

**Ours alone:**
1. **Upload protection:** when the upload can't carry every platform, pause the lowest-priority
   one (the streamer sets the order) so the main stream stays smooth; bring it back when the
   connection recovers.
2. **Smart titles:** templates (`Day {count} | {game} ranked`) that fill themselves, with short
   versions where a platform's limit is smaller.
3. **Thumbnail maker:** a template (logo, title, game) plus a frame from the live stream, one
   click to a YouTube thumbnail.
4. **Schedule once:** one time creates the YouTube scheduled stream, the Twitch schedule
   entry and the Discord announcement.
5. **Never lose a stream:** a local backup recording starts with the stream.
6. **One after-stream report** for every platform: peaks, followers, best moment, clips for Shorts.
7. **Vertical layouts made easy:** presets like "webcam on top, game below", no second scene
   collection to build.

## Track A: design picks and build steps (owner, 22 Oct 2026)

Board: `../design/2026-10-22-go-live/index.html`. Picks: **1A** a new first Studio tab "Go live";
**2A** one reorderable list (bottom pauses first), rules notes per row, vertical as its own group;
**3A** three setup steps on the tab, then one line with "Put OBS back"; **4B** the upload check as
a table per platform (server, speed, delay, needs) with the verdict under it; **5A** a health line
per platform while live, health dots on the top pills; **6 mixed**: tabs Everywhere / Twitch /
YouTube / Kick (a dot marks a changed tab, "Use the everywhere title" resets a field), and the
Everywhere tab lists under each field what a platform changed; **7** custom destination dialog
and **8** the phone view as drawn.

Engine pieces (new crate `crates/golive`, RTMP from `rml_rtmp` 0.8, MIT, sans-IO; our own Tokio
I/O, TLS through rustls for `rtmps://`):
- [x] **A1.1 Local ingest:** RTMP server on `127.0.0.1` only (R-43), an unguessable stream key per
  install (wrong key: refused), one publisher at a time; keeps the metadata and the audio/video
  sequence headers so a destination that (re)connects mid-stream starts cleanly at the next
  keyframe (at most one keyframe interval, about 2 s; later: keep the last keyframe group so it
  starts at once).
- [x] **A1.2 Forwarding:** one task per destination (R-18): connect, publish, send the cached
  headers, then the same packets (no re-encode); backoff with jitter, never giving up while the
  show runs; a slow destination drops its own queue, never slowing the others; health per
  destination (state, bitrate, drops, reconnect try, plain reason).
- [x] **A1.3 Destinations and keys:** Twitch, YouTube, Kick and custom (server + key, keychain
  R-45); Twitch key by Helix (`channel:read:stream_key`), YouTube by liveStreams; Kick to confirm.
- [x] **A1.4 OBS setup:** point OBS's stream at the local ingest (`SetStreamServiceSettings`),
  remember the old setting, put it back on request; never while OBS is streaming.
- [x] **A1.5 Upload check:** per-platform test to its ingest (Twitch `bandwidthtest`), speed and
  delay, the verdict in words.
- [x] **A1.6 API + dashboard:** routes in API.md, the Go live tab (1A, 2A, 3A, 4B, 5A), demo engine.
- [x] **A1.7 Real stream test (owner):** done 22–23 Oct with 2.0.0-beta.3 on the owner's Mac (notes in STATUS.md). OBS on the owner's PC to Twitch and YouTube at once;
  reconnect Twitch and Kick first (new scopes `channel:read:stream_key`, `streamkey:read`; Kick's
  app may need `streamkey:read` ticked in its developer settings). Then release 2.0.0-beta.
- [x] **A1.8 Upload protection:** done 24 Oct (unreleased, for 2.0.0-beta.4). With 2+ live,
  4 s of a destination under 70% of the stream's rate or skipping packets pauses the bottom
  live one (`crates/golive/src/protect.rs`); after 10 calm seconds and its pause (60 s, doubling
  to 10 min) the highest paused one comes back. The health line says "Paused" and why, with
  Resume now; a switch under the platforms turns it off (on by default). Advanced limits wait
  for the Simple/Advanced work.
- [ ] **A2:** stream info per platform (6 mixed), vertical forwarding.

## Owner's notes, 23 Oct 2026 (to start later; nothing built)

### Vertical streams (camera on top, game below)
The owner streams with OBS plus the Aitum Vertical plugin (OBS shows "Vertical Scenes" and
"Vertical Sources"). The goal is the Shorts look: camera on top, game underneath, 1080×1920.
Three ways, in order:

1. **Ferrocast cuts it from the normal stream (works with any OBS setup).** In Ferrocast the
   streamer sees the normal stream and drags two boxes, one round the camera and one round the
   game, and picks a layout (camera on top, small camera in a corner, game only, …). Ferrocast
   crops, scales and stacks them upright and sends that to vertical destinations (YouTube
   Shorts live, custom servers). No OBS change. Costs: decode plus one extra encode (GPU, light);
   a small corner camera looks soft when enlarged.
2. **OBS's own vertical picture (best quality).** With Aitum Vertical (or OBS 32's second
   canvas) OBS sends an upright stream to Ferrocast's second local address and Ferrocast
   forwards it (plan A2). Later: one click in Ferrocast builds the vertical scene from the
   streamer's own camera and game sources in a chosen layout, if obs-websocket allows it
   (check canvas support first). Works for the owner today, since Aitum Vertical is installed.
3. **Ferrocast Studio (2.1+).** Ferrocast draws the vertical picture itself: full-quality
   camera, any layout, different per platform.

Recommended: build 1 first (everyone), plus 2's forwarding for people with a vertical canvas;
3 comes with the Studio.

**Aitum Multistream:** it also sends to several platforms from OBS. Using it and Ferrocast's
Go live together uploads everything twice. The setup should notice it and say so.

### A different picture per platform (Studio only)
With OBS, Ferrocast only copies OBS's one stream, so every platform gets the same picture. In
the Studio (2.1+) each platform can have its own scene or camera (Twitch: game and a big
camera; YouTube: game only); a scene switch can apply to all platforms or one. Cost: one
encode per different picture (2–3 are fine on a normal GPU); upload is unchanged (each
platform is already its own copy).

### OBS's screen against Ferrocast
| In OBS | Ferrocast today | Ferrocast Studio |
|---|---|---|
| Scenes | Switches OBS's scenes | Its own scenes (2.1) |
| Sources (camera, screen, mic, images, browser) | Shows/hides OBS's sources | Its own sources (2.1) |
| Audio mixer | Mutes mics, mic check before going live | Full mixer and filters, noise removal (2.2) |
| Start Streaming | Yes, to every platform at once | Yes, without OBS (2.1) |
| Start Recording | No | Yes (2.1), plus a backup recording while live |
| Replay buffer | Through OBS ("clip that") | Built in (2.2) |
| Studio Mode | No | Yes (2.2) |
| Scene transitions | No | Yes (2.2) |
| Hotkeys | Panic key | Full hotkeys (2.2) |
| Vertical scenes | Forwarding next (A2) | Built in, with layouts |
| Virtual camera | No | Not planned yet: owner to decide |

Easier than OBS: ready-made scene layouts, alerts and overlays built in, scenes that switch on
events, one-click import of an OBS scene collection, each platform's rules checked live, a
frozen camera or game capture restarted by itself.

### Overlays
- With OBS (now): no copy and paste; each overlay's "Add to OBS" puts it in OBS as a browser source.
- With the Studio: no browser source at all. Overlays are part of the scene; tick an alert box,
  chat box or goal bar on or off per scene.

### Simple mode and Advanced mode
Everyone starts in **Simple**; **Advanced** is one switch away (in the sidebar next to
Light/Dark). Simple shows the few choices most people need, with ready-made layouts and
self-adjusting settings. Advanced shows the same screens with every setting visible and typed
as numbers, nothing decided for the streamer. Each card also gets a "More options" link, so a
Simple user can open one section without switching everything. Remembered per install.

| Area | Simple | Advanced adds |
|---|---|---|
| Go live | Tick platforms, set up OBS, test upload | Server region by hand, rtmps or rtmp, retry timing, buffer size, upload kept free, local port, a new local key |
| Vertical | Pick a layout | Every box to the pixel, crop, zoom, background, size, frame rate, bitrate |
| Upload protection (A1.8) | On, sensible order | Own limits, which platform pauses first, how long before it comes back |
| Studio (2.1+) | Ready-made scenes and templates | Every source, filter, transition timing, encoder settings, audio routing, hotkeys |
| Alerts and overlays | Pick a design | The full designer (exists) |

**Before building:** a design board with the Simple/Advanced switch and the Go live and
vertical screens in both modes, for the owner to pick from (no code until the owner says so).

**Picks, 24 Oct 2026** (board `../design/2026-10-24-vertical-modes/index.html`):
- **1B** the Simple/Advanced switch sits at the top of each screen, and each screen remembers
  its own mode (Go live can be Advanced while Alerts stays Simple). No global switch in the
  sidebar.
- **2B** Go live's Advanced settings live in one "Advanced settings" card below the platforms,
  with a picker for Twitch / YouTube / Kick / Local server (server region, rtmps or rtmp, first
  retry, longest wait, buffer; local port, new key, upload kept free).
- **3** upload protection as drawn: Simple is the switch; Advanced is the rule as a sentence
  with editable numbers (share, trouble seconds, which pauses first, calm seconds, first and
  longest pause).
- **4B** vertical gets its own Studio tab ("Vertical", after Go live) with the source choice
  (cut from your stream, or OBS's vertical canvas, Aitum Vertical detected) and its own
  destinations. Replaces 2A's "vertical group" in Your platforms.
- **5A** Simple layout: one screen, the stream with Camera and Game boxes (guessed from OBS
  sources), the upright preview beside it, four layouts (camera on top, small camera, half and
  half, game only).
- **6, 7, 8, 9** approved as drawn: Advanced vertical numbers, the Aitum Multistream double
  upload warning, stream info per platform (pick 6 mix), the phone layout picker.

Build order once the owner says to code: A2.1 stream info per platform (8), A2.2 Aitum
Multistream warning (7), A2.3 vertical forwarding from OBS's canvas (second local address,
4B tab), A2.4 cut-from-stream vertical (decode, crop, stack, encode; 5A, 6), A2.5 the per-screen
Simple/Advanced switch (1B) with Go live's Advanced card (2B) and upload protection's (3).

- [x] **A2.1 Stream info per platform:** done 24 Oct. `StreamInfo.per` (a platform's own
  fields, merged over everywhere); dashboard tabs Everywhere / Twitch / YouTube / Kick.
- [x] **A2.2 Aitum Multistream:** done 24 Oct. Found through its obs-websocket vendor request,
  OBS's output list and its settings file (names and servers only); setup note with "I use it
  on purpose", live "getting your stream twice" with "Stop Ferrocast's copy".
- [x] **A2.3 Vertical from OBS's canvas:** done 24 Oct. A second local key into its own
  stream; destinations have a canvas; YouTube Shorts live (Ferrocast makes a reusable "Ferrocast
  vertical" stream and a self-starting broadcast with the normal live's title and privacy);
  Aitum Vertical pointed here and back through its vendor requests; Studio's Vertical tab (4B).
- [x] **A2.4 Cut from the stream:** done 24 Oct (engine and 5A editor). ffmpeg crops, fills and
  stacks the camera and game into 1080x1920, encodes on the GPU (VideoToolbox, NVENC, QSV, AMF,
  else x264) and publishes to the vertical key. Real test on the owner's Mac: their 2560x1440
  OBS scene cut in about 3 s, ~70% of one core with GPU decode. Still to do: ffmpeg comes with
  the app (today it uses Homebrew's or PATH), and the Advanced numbers (item 6) with A2.5.
- [ ] **A2.5** Simple/Advanced (1B, 2B, 3, 6).

### Found in the real test (23 Oct)
The owner's OBS sends ~12 Mbps at 1440p60; Twitch takes 6 and Kick 8 (Twitch "Unstable", Kick
dropping). Ferrocast warned on both rows. To do: lower OBS's bitrate to ≤ 6,000 kbps.

## Plugins (3.0; designed once 2.0 and the Studio exist)

OBS plugins run with full access, must match the exact OBS version, break on updates and can
crash a live stream (research 03, 08, 09). Ferrocast plugins follow R-39, R-42 and R-47:

| Type | Examples | Safety |
|---|---|---|
| Widgets and overlays | A custom goal bar, game stats | Sandboxed web frame, like custom CSS/JS designs today |
| Actions and automation | "Tip over ₹500: sound, scene, Discord post" | **WASM** sandbox, only the permissions it asks for (send chat, switch scene…), approved on install |
| Destinations | A new streaming platform | Same sandbox |
| Studio effects (2.1+) | Filters, sources | The Studio runs on libobs, so many **existing OBS plugins can load**; they have full access as in OBS, so they're marked and the sandboxed kind is suggested first |

- **One gallery in Ferrocast:** browse, one-click install, auto-update, ratings; no `.dll` hunting.
- **A stable, versioned plugin API:** a Ferrocast update never breaks a plugin.
- **A plugin can't take the stream down:** it runs apart; a misbehaving one is stopped, with why.
- **Developer kit:** a template project, a test mode with the demo engine's fake events, docs.

## How it fits

```
Ferrocast app
├─ engine (chat, alerts, bot, OBS control)
└─ studio helper process   own process: a capture or encoder crash never stops chat or alerts
     └─ libobs: capture → scenes → GPU mix → encoders → outputs
          ├─ Twitch (RTMP)    ├─ YouTube (RTMP)    ├─ Kick (RTMPS)    └─ recording
```

- **Engine:** libobs (OBS's core library, GPL-2.0-or-later, fine inside GPL-3.0 Ferrocast),
  driven from Rust. The dashboard's Studio tab gets the preview, scenes, sources, a mic
  mixer, Go live and Record.
- **Overlays** render inside the studio's scenes directly, no Browser Source setup.
- **Stream keys:** from each platform's API where allowed (Twitch, YouTube), pasted where not
  (Kick, to confirm). Kept in the OS keychain like other tokens (R-45).
- **Multistream:** encode once when the platforms take the same settings, then one output per
  platform; a platform needing different settings gets its own encode. Show the upload each
  needs before going live, and a per-platform health line while live.
- **Supervisor:** the engine starts, watches and restarts the helper (R-18); going live never
  depends on our servers (R-01).

## Phases (Track A first: it helps OBS users right away and the Studio reuses it)

| Track A phase | What ships | Rough time |
|---|---|---|
| A1 | Local relay, one-click OBS setup, keys from Twitch/YouTube/Kick, Go live to all 3, health and reconnects, upload check, platform-rule notes | 2–3 weeks |
| A2 | Per-platform stream info (same or different per field), presets; vertical forwarding from OBS's second canvas | 1–2 weeks |
| A3 | Pro cloud forwarding (one upload) | later, with server cost worked out |

Track B (the Studio):

| Phase | What ships | Rough time |
|---|---|---|
| 0. Spike | libobs builds and runs in our app on Mac and Windows; one screen capture streamed to Twitch; size and build impact measured | 1 week |
| 1. Preview build | Scenes; display, window, webcam, image and mic sources; our overlays; preview; hardware encoding (VideoToolbox, NVENC, AMF, QSV); multistream to all 3; recording. Labelled "preview" next to OBS support | 4–6 weeks |
| 2. Daily driver | Audio mixer and filters (noise suppression), transitions, game capture on Windows, studio mode, hotkeys, import an OBS scene collection | 2–3 months |
| 3. Beyond OBS | A vertical stream at the same time, clips from the stream, scene changes from events | later |

## Lessons from OBS's code and bug tracker

From `research/11-obs-internals-and-failures.md` (OBS 33.0 source and 2025–2026 issues):
the Studio isolates libobs, overlays and plugins in separate processes; watchdogs restart a
frozen capture, re-bind a lost audio device and step an overloaded encoder down; every
destination keeps reconnecting while the show runs (OBS stops after 20 tries); the backup
recording is kept apart from streaming; devices are remembered by stable id; auto settings
keep measuring instead of a one-time formula; unstable libobs APIs (canvases) sit behind our own
wrapper with the libobs version pinned per release; start and quit are clean; scene collections
import from OBS.

The spike also checks: libobs as a helper process driven from Rust, capture watchdog on
macOS ScreenCaptureKit, and one encoder feeding several outputs.

## Costs and risks

- App size grows by about 100–200 MB (libobs and its plugins).
- Release builds get harder: libobs per OS, signing its plugins on Mac, Windows drivers.
- Testing needs real hardware: webcams, capture cards, NVIDIA / AMD / Intel GPUs.
- Support: "black game capture", "encoder failed" become ours to answer; R-08 applies (say why).
- Upload: three 1080p60 streams need roughly 18–24 Mbps; the go-live check must say so.

## Open questions for the spike

- Exact libobs build and packaging per OS, and the size it adds.
- Whether Kick gives an app its stream key and ingest URL by API.
- Encoder sharing across outputs when platforms differ (Kick's and YouTube's limits).
