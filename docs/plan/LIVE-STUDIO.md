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
