# Lessons and rules (R-01 … R-78)

The rules we build by. Each one comes from a real incident, bug or user complaint; the research behind them lives outside this repo (owner's research folder).

## 1. Product
- **R-01** **Works with zero Ferrocast servers.** The free desktop app runs fully offline from our cloud. If our relay is down, Twitch and YouTube keep working and Kick events are queued and replayed. (Mixer, StreamElements, Trovo, DeepBot all vanished with weeks of notice.)
- **R-02** **Export everything from v1**: overlays *with their media*, commands, points, viewers, tip history (JSON + CSV). Tested in CI. (SE overlay URLs didn't contain the media; Chatterino's backup request open 6 years.)
- **R-03** **Importers are our best acquisition tool**: StreamElements first, then Streamlabs, Streamer.bot, Firebot commands.
- **R-04** **Public end-of-life pledge**: 180 days' notice before closing any cloud feature, export open until the end, open-source handover.
- **R-05** **Free stays free forever.** Never move a free feature behind Pro. Write it down publicly. (Restream, Eklipse, Streamlabs widget removals.)
- **R-06** **Pro never talks to the audience.** No upsell messages in chat, no branding on overlays. (A tool doing this got uninstalled on the spot.)
- **R-07** **Charge only for what costs us money to run**: 24/7 cloud bot, sync/web dashboard, smart filters, premium voices/themes, custom tip domain.
- **R-08** **Explain every missing alert.** An event log that says *why* something didn't fire (Twitch hid a low-trust follow, below minimum, filtered, duplicate). (Reddit: many "alerts not firing" are Twitch hiding follows.)
- **R-09** **Show platform honesty in the UI**: badges like "events can be late" (Kick), plus a "known gaps per platform" table.
- **R-10** **Each platform is a plugin with a health check**, so we can drop a dying one cleanly. Trovo is dropped; Facebook is not planned.
- **R-11** **Shared-Chat aware**: reply only in the source channel, never post timers twice, honour `is_source_only`.
- **R-12** **No core feature on Twitch Extensions** (shrinking surface). Overlays + chat are the portable surface.
- **R-13** **Tips: the streamer's legal name is never shown to tippers.** Lead with this; doxxing worries people more than chargebacks.
- **R-14** **Mod tools are a headline, not an extra**: cross-platform ban, panic mode, bot/scam defence, viewer notes. Integrate with or match Sery_Bot.
- **R-15** **Don't lead with AI.** It's a negative signal on Reddit. Call it "smart filters".
- **R-16** **Never copy competitors' text, assets or names.** "Works with OBS", never "Ferrocast OBS".

## 2. Connections and platforms
- **R-17** **One adapter per platform behind a stable internal event schema.** Only the adapter changes when a platform does. (Twitch forced 4 rewrites 2022–2025.)
- **R-18** **One connection supervisor per platform**: exponential backoff + jitter, deadlines on every handshake step, keepalive watchdog using the server's interval, rebuild on OS wake. Health = "events flowing", not "socket open". (Chatterino's reconnect loop, the most-reacted issue in the dataset; zombie sockets after sleep.)
- **R-19** **Client-side rate budgets** per platform (joins, messages, API calls) and a diagnostics panel showing what's left.
- **R-20** **Outgoing chat queued while offline expires** (~10 s) instead of flooding chat on reconnect.
- **R-21** **Twitch: EventSub WebSocket + Helix only.** No IRC, no PubSub, no unofficial GQL. On `session_reconnect`, connect to `reconnect_url` and keep subscriptions (don't resubscribe).
- **R-22** **Ask at first login for every scope we'll need within a year**; a missing scope gives a one-click re-consent, never a silent failure.
- **R-23** **Pro cloud bot = app token + `channel:bot` over Conduits** (only route to the Chat Bot badge and unlimited channels).
- **R-24** **YouTube: gRPC `streamList`, never `liveChatMessages.list` polling, never scraping.** Budget quota per method, apply for the audit early, allow bring-your-own key.
- **R-25** **Kick: official API through our webhook relay.** Verify signature + timestamp + replay. Unofficial Pusher chat only as an opt-in fallback.
- **R-26** **No TikTok.** Decided 17 Oct 2026: no official API, breaks 5–8 times a year, ToS risk. Not built, not planned.
- **R-27** **Weekly API canary in CI** against each platform's live schema and changelog; parse leniently and log unknown fields/types. Watch Twitch dev forum, YouTube and Kick changelogs.
- **R-28** **Default bot identity is the streamer's own account (or their own bot account)**, not one shared bot that can be banned everywhere at once. (Nightbot's ban.)

## 3. Events, alerts and data
- **R-29** **Every event gets an idempotency key** and is deduped in the event bus *before* alerts, goals or points change. (TikTok gifts firing twice ~90% of the time; Twitch bits counted twice.)
- **R-30** **Staged startup with an event gate**: DB → variables → connections → open gate. Early events are buffered, not dropped.
- **R-31** **Alert queue is persisted and ordered**, with priority modes and a staleness policy (condense alerts after an outage; "missed while disconnected" instead of 50 alerts at once).
- **R-32** **Money is typed**: real money separate from Bits/KICKs/YouTube Super Chats in platform currency. Refunds and chargebacks reverse goals, leaderboards and points.
- **R-33** **Every event type ships sample data** so "Test alert" always works.
- **R-34** **SQLite (WAL) with versioned, tested migrations.** Atomic file writes (temp + fsync + rename). Automatic snapshot before every update/migration, with "restore previous" in the UI. A corrupt file never silently starts with defaults; a migration never flips a user setting.
- **R-35** **No blocking I/O on the async runtime; no single mutexed DB connection; no god files or global singletons.**

## 4. Overlays
- **R-36** **Overlays render; they never create events.** Only the core creates alerts. (Blocks forged "$100M" alerts.)
- **R-37** **Viewer text is untrusted**: overlays render with `textContent`/escaped templates; raw HTML is opt-in per widget with a warning. (Firebot XSS, fixed May 2026.)
- **R-38** **One WebSocket per overlay page**, current state sent on connect (no blank labels), pause when OBS hides the source, no infinite CSS animations by default, avoid WebM-alpha heavy alerts by default.
- **R-39** **Every overlay/widget accepts custom CSS/JS** (top requested feature), sandboxed.
- **R-40** **24-hour soak test in CI** (1 event/s) that fails on memory growth. Low-motion preset for Linux (OBS browser sources are slow there).
- **R-41** **Media served from our local server with Range support**, not Tauri's asset protocol.
- **R-42** **SE custom widgets later run in a sandboxed frame with our own `SE_API` shim**, never a real API token.

## 5. Security
- **R-43** **Local server binds 127.0.0.1 only**, per-overlay unguessable token, Origin/Host checks (anti DNS-rebinding). LAN access is an explicit toggle. (Firebot and OxidizeBot listen on all interfaces with no auth.)
- **R-44** **Overlay URLs are revocable**: "regenerate URL" one click away, URLs blurred in the UI by default.
- **R-45** **Tokens in the secret store** (`ferrocast-secrets`), never in settings, the database, exports, backups, logs or crash reports. Windows: Credential Manager; Linux: Secret Service. macOS: a private `secrets.json` in the data folder (folder `0700`, file `0600`, atomic temp-file + fsync + rename, a damaged file is an error and is never overwritten), not the Keychain: our builds are ad-hoc signed (no Apple Developer ID), so each update has a new code identity and the Keychain would ask for the login password after every update, which looks like phishing. OBS keeps its own WebSocket password in a plain config file, so a user-only file is the same bar. Never read the old Keychain items (reading is what prompts); macOS users reconnect once. Persist the refresh token before using it (twitch_oauth2 `take()` pitfall). Login through the system browser (Device Code / loopback PKCE), never an embedded webview.
- **R-46** **No client secrets in the app.** Platforms that need one go through our relay broker.
- **R-47** **Scripts, plugins and imported actions are sandboxed** (Rhai/WASM with explicit capabilities) with a permission prompt on import. No unbounded "fetch URL" variables (SSRF).
- **R-48** **TTS off by default.** When on: minimum amount, hold queue, approve button, lookalike-aware word filter, max length, never read numbers/phones/addresses, auto-drop when mods delete the message.
- **R-49** **Alerts only on captured/settled payments.** "Pending" for large or first-time tippers, optional hold window, velocity limits.
- **R-50** **Hate-raid panic mode is local and instant**: one key → followers-only, emote-only, slow mode, hold new accounts, mass-ban by pattern, pause alerts and TTS, on every platform.
- **R-51** **Minimal cloud data**: no chat logs stored by default; tip data stays with the payment provider.
- **R-52** **Supply chain**: `cargo-deny`/`cargo-vet`, review `build.rs` of dependencies, no prebuilt binaries, pinned overlay dependencies (no CDN scripts).

## 6. Desktop app (Tauri)
- **R-53** **The webview only renders.** All logic, timers, connections, TTS scheduling and the overlay server live in Rust (hidden webviews throttle JS).
- **R-54** **Batch UI updates** (flush every 50–100 ms via Channels or our local WebSocket); virtualised chat list. (Tauri `emit` leaks memory at volume.)
- **R-55** **Linux**: detect NVIDIA + Wayland and apply the DMABUF workaround with a toggle; prefer Flatpak + .deb/.rpm over AppImage; re-check Tauri's CEF runtime before v1.
- **R-56** **Tray is never the only way in**: all tray actions in the menu (Linux ignores left-click) + a global hotkey to show the window. Hide windows instead of destroying/recreating them repeatedly.
- **R-57** **Opaque windows by default**; transparent in-game overlay only while visible (macOS transparent windows burn GPU).
- **R-58** **Native Apple Silicon and ARM64 Linux builds from day one.**
- **R-59** **No marketing memory claims** without our own measured benchmark per OS.

## 7. Releases and updates
- **R-60** **Never update while live, never force an update.** "Remind me after stream", stable + beta channels, one-click rollback to the previous version. (Firebot added this after a bad Dec 2025 release.)
- **R-61** **Sign everything**: Apple notarisation (staple .app and .dmg, check with `spctl`), Windows via SignPath (`signtool verify` in CI). No UPX/packers. (Tauri builds get flagged as trojans.)
- **R-62** **Windows installer is per-user NSIS (no admin), not MSI.** CI tests updating from N-1 and N-2 as standard and admin users on every OS; in-app "download manually" fallback.
- **R-63** **CI on macOS arm64, Windows, Linux X11 + Wayland**, plus idle-CPU gates per OS.

## 8. Business and licensing
- **R-64** **Licence chosen once; rights never taken away.** Public licence pledge in the README. (HashiCorp, Elastic, Redis, Mix It Up, Cal.com backlash.)
- **R-65** **Recommended: GPL-3.0 desktop app, closed separate cloud service, DCO not CLA.** GPL stops a closed rebrand (the Streamlabs-OBS pattern) and fits SignPath's rules.
- **R-66** **Trademark "Ferrocast"** so forks must rename.
- **R-67** **Pro must pay the bills; donations won't.** OBS raises ~$96k/yr, Firebot ~$1.2k/yr. GitHub Sponsors as a bonus only.
- **R-68** **Price Pro low for India and emerging markets.** Price changes: existing subscribers keep their price ≥12 months, 60+ days' notice, never remove a feature from a paid tier.
- **R-69** **We never hold creators' money** and run no sponsorships.
- **R-70** **Documented local API + open export formats**; never gatekeep data to block competitors.
- **R-71** **Bus factor ≥ 2**: two maintainers with release keys; publish a succession plan.
- **R-72** **Launch channels**: not r/Twitch (it bans all tool promotion since 1 Sep 2026). Use r/obs, r/streaming (check rules), GitHub, Discord. Open source, signed and auditable is our answer to the "AI-coded tool" distrust. Don't headline Kick to Twitch audiences.

## 9. The Studio (libobs, 2.1)
- **R-73** **`unsafe` lives in one file**: `crates/studio/src/ffi.rs` (libobs and AppKit calls), each block with its SAFETY reason; the crate denies it everywhere else and the workspace still forbids it. Raw libobs calls are crate-private; only the safe API is public.
- **R-74** **libobs is loaded at runtime** (`libloading`), never linked: Ferrocast builds and runs where OBS's engine isn't present, and a failed load is a plain error.
- **R-75** **Options come from a live source.** Camera, capture card, screen and some filters crash when asked for their options by kind alone; OBS's own dialog always asks an instance (`obs_source_properties`). Interface-only plugins (frontend-tools, the DeckLink/AJA UIs, obs-websocket) are disabled: they need OBS's Qt window.
- **R-76** **Remove before release.** libobs 32 keeps scenes in its canvas: `obs_source_remove` every scene and source before letting go, or it's freed after its plugin unloaded (a text source crashed in FreeType at shutdown). `obs_wait_for_destroy_queue` alone isn't enough.
- **R-77** **The signed app must be allowed to load OBS.** Ferrocast ships with the hardened runtime; it refuses libraries signed by another Team ID ("mapping process and mapped file have different Team IDs"), so libobs never loads without `com.apple.security.cs.disable-library-validation`. Camera and microphone sources also need `device.camera` / `device.audio-input` plus their Info.plist texts. Test Studio changes in a hardened, signed build too, not only the unsigned dev binary.

- **R-78** **Web pages render through the system's WebKit, off-screen.** The Studio's Web page source (`ferrocast_web`) is our own libobs source: a WKWebView in a hidden window, snapshotted while shown and uploaded as a texture. Off-screen WebKit throttles itself to a frozen frame unless `inactiveSchedulingPolicy` is None (macOS 14+) and occlusion detection is off; both are checked with `respondsToSelector` first. A script injected at document start sends the page's Web Audio and media elements to the engine (16-bit chunks through a script message handler, out through `obs_source_output_audio`) and silences the speakers; Interact moves the same window on screen, scaled to fit, and its close button puts it back. A volume meter must not be dropped because one caller didn't ask for it: two screens polling different sounds kept recreating each other's meters, so they always read silent.

- **R-79** **Editing in the preview.** The native preview view returns nil from `hitTest:` so clicks reach the page under it; the page picks, moves and resizes (canvas pixels from the letterboxed box) and the engine draws the outline in the display's draw callback. libobs works an item's box out only on the next frame (`update_item_transform` is deferred for items in a scene), so never read `obs_sceneitem_get_box_transform` right after a change: compute it from pos, scale, rot, alignment, bounds and the cropped size as obs-scene.c does (`Place` in collection.rs, checked against libobs in examples/collection.rs). Ending an effect's technique puts its parameters back to their defaults, so set a colour before every `gs_effect_loop`. Unsigned (ad-hoc) builds are a new app to macOS's privacy switches after every update while System Settings still shows the old one on: `tccutil reset ScreenCapture <id>` plus `CGRequestScreenCaptureAccess`, then a restart (the Add dialog's Screen Recording card).
- **R-80** **One signing identity for every macOS build.** macOS keeps Camera, Microphone and Screen Recording answers by the app's designated requirement. Ad-hoc builds have only their cdhash, so each update was a new app while System Settings showed the old one switched on. Release builds are signed with Ferrocast's own self-made certificate (CN "Ferrocast (ClearCraft Labs)", until 2051): the requirement is `identifier "com.clearcraftlabs.ferrocast" and certificate root = H"2b833f57…"`, the same in every build. codesign signs with an untrusted identity by name; Gatekeeper still asks once on first open (no Apple account), as with ad-hoc. `deploy/macos-signing.sh` sets up a throwaway keychain (the .p12 must be `-legacy`: macOS's `security` can't read OpenSSL 3's default), `APPLE_SIGNING_IDENTITY` makes Tauri use it, and Release checks the root hash. The certificate and its password live in `secrets/macos-signing.*` and the repo secrets MACOS_SIGNING_P12(_PASSWORD); a new certificate would reset everyone's permissions once more, so keep it. Local test builds: `deploy/macos-signing.sh setup`, `APPLE_SIGNING_IDENTITY=… cargo tauri build`, then `teardown`.