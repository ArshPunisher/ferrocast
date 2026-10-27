# Local API (v1)

The contract between the engine (this repo) and the dashboard and overlays (`ferrocast-web`). Everything is served by the loopback server: `http://127.0.0.1:<port>`, default port **47820** (the next free one if taken; the desktop app knows the real one).

JSON everywhere, `snake_case` keys, enums tagged with `"type"` (Rust serde `tag = "type"`). Times are Unix **milliseconds**.

## 1. Errors

Every error, on every route:

```json
{ "error": { "code": "overlay_not_found", "message": "That overlay doesn't exist anymore.", "hint": "Reload the overlays list." } }
```

| Status | When |
|---|---|
| 400 | Bad JSON or a field fails validation (`code: "invalid"`, `message` names the field) |
| 401 | No or wrong session token |
| 403 | Wrong `Origin`/`Host`, or LAN access turned off |
| 404 | Unknown id or route |
| 409 | Conflict (name taken, already connected) |
| 413 | Upload too big; `message` states the limit ("Sounds can be up to 5 MB.") |
| 415 | Wrong file type; `message` says which types are allowed |
| 422 | Valid JSON, but the action can't run now ("Twitch isn't connected.") |
| 429 | Too many requests; `Retry-After` header |
| 503 | The engine is still starting; `Retry-After: 1` |

## 2. Dashboard auth

- The desktop window receives `window.__FERROCAST__ = { api: "http://127.0.0.1:47820", token: "<session>" }` from Tauri before the page loads.
- In a normal browser, the dashboard opens with `/app/#pair=<code>`; it calls `POST /api/v1/session/pair { code }` → `{ token }`. Codes are one-time and expire after 2 minutes.
- Every `/api/v1` call sends `Authorization: Bearer <token>`. The live socket sends it as `?session=<token>` (browsers can't set WebSocket headers).
- The server rejects requests whose `Origin` (when present) isn't its own origin, and whose `Host` isn't `127.0.0.1:<port>` or `localhost:<port>`.

## 3. Shapes

```ts
type Platform = "twitch" | "youtube" | "kick" | "tips" | "ferrocast";

type Money = { minor: number; currency: string };           // ₹500.00 = { minor: 50000, currency: "INR" }
type Actor = { platform: Platform; id: string | null; login: string | null; display_name: string;
               avatar_url: string | null; color: string | null; roles: number; badges: Badge[]; anonymous: boolean };
// roles bit flags: 1 broadcaster, 2 moderator, 4 vip, 8 subscriber, 16 member, 32 founder, 64 staff, 128 verified, 256 bot, 512 first_time
type Content = { text: string; fragments: Fragment[] };
type Fragment = { type: "text"; text: string }
              | { type: "emote"; provider: "native" | "seven_tv" | "bttv" | "ffz"; id: string; name: string; url: string; zero_width: boolean }
              | { type: "mention"; login: string }
              | { type: "link"; url: string };

type Event = { id: string; seq: number; platform: Platform; channel: { platform: Platform; id: string; name: string } | null;
               source_id: string | null; occurred_at: number; received_at: number;
               origin: "live" | "backfill" | "replay" | "test"; kind: EventKind };
// EventKind: { type: "chat_message", actor, content, reply_to, is_action } | { type: "follow", actor }
//   | { type: "subscription", actor, tier, months, message, gifted_by } | { type: "gift_batch", gifter, tier, count }
//   | { type: "platform_gift", actor, value: { unit: "bits" | "kicks" | "jewels" | { other: string }, amount }, message }
//   | { type: "paid_message", actor, money, content, sticker_url } | { type: "tip", actor, money, message, provider, payment_ref, status }
//   | { type: "raid", from, viewers } | { type: "reward_redemption", actor, reward, cost, input }
//   | { type: "message_deleted", message_id } | { type: "user_messages_cleared", target } | { type: "ban", target, reason, until }
//   | { type: "stream_online", title } | { type: "stream_offline" } | { type: "viewer_count", count } | { type: "connector", state, ... } | …
// tier: { type: "prime" | "tier1" | "tier2" | "tier3" } | { type: "level", id, name }

type RenderedAlert = { alert_id: string; event_id: string; rule_id: string; variation_id: string; kind: string; platform: Platform;
                       title: string; body: string; image: string | null; sound: string | null; volume: number;
                       duration_ms: number; layout: string; css: string; tts_text: string | null; tts_audio: string | null; test: boolean;
                       design?: AlertDesignView /* only when an alert design dressed it (§4 Designs); layout is then "design" */ };
// sound: a media id, or `builtin:<id>` for one of the overlay's built-in sounds (ids as in Sound.builtin below). A new install's
//   default rules start with one each (core::alert::default_sound: follow glint, subscription cascade, gift_batch bloom,
//   platform_gift / paid_message / tip tally, raid lift, reward_redemption pebble); saved rules never change.
// tts_audio is a media id of a WAV made with the computer's own voices; it may be null even when tts_text is set
// (synthesis still running or unavailable): then speak tts_text with speechSynthesis.
```

## 4. Routes

### Status and session
| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/status` | → `{ version, port, live: { twitch, youtube, kick }, connectors: { twitch: ConnectorState, youtube, kick, obs, relay }, setup_done: boolean }` |
| POST | `/api/v1/session/pair` | `{ code }` → `{ token }` (no auth needed) |
| POST | `/api/v1/session/pair-code` | → `{ code, url, expires_at, lan: { active, restart_needed } }` (makes an "open in browser" link). With LAN access on, `url` uses this computer's network address and `lan.active` is true; otherwise it's `127.0.0.1` (works on this computer only), and `restart_needed` says LAN access was switched on but applies at the next start |
| POST | `/api/v1/setup/complete` | → 204: first run is done, `setup_done` stays true (it's also true once an overlay exists) |
| GET | `/api/v1/stats/session` | → `{ live, started_at (kept after the stream ends), follows, subs, tips: Money[] (one per currency, refunds taken off), bits, messages, peak_viewers, viewers: { twitch?, youtube?, kick? } }` |
| GET | `/api/v1/recap?since=<ms>` | → `Recap` (below): this stream so far, or the last one after it ended. `since` (optional, not in the future, else 400 `since: …`) picks the start instead |
| GET | `/api/v1/themes` | → `{ id, name, description, pro }[]`: `molten` (default), `daylight`, `arcade`, `minimal`, `sticker`; unknown ids fall back to `molten` |
| GET | `/api/v1/updates` | → `UpdateView` (below). Never offer an update while `blocked_while_live` (R-60). `checked_at: null` means not checked yet, which is not "up to date" |
| POST | `/api/v1/updates/check` | "Check for updates": asks the desktop app to read the feed now and answers 200 `UpdateView` once it has (after 20 s still `checking: true`; the `update` frame brings the rest). A failed check is in `error`, not an HTTP error. 409 `no_updater` (with where to download) in the headless app |
| POST | `/api/v1/updates/install` | → 202 `{ installing }`; takes a backup, then the desktop app downloads it (progress in `install`), checks its signature and installs it (on Windows the installer waits for the restart). It does **not** restart: the new version runs after `/updates/restart`. 404 `no_update` when nothing newer was found, 409 `live` while live with "never update while live" on (R-60), 409 `update_busy` while downloading or installing, 409 `restart_needed` once installed, 409 `no_updater` in the headless app, 422 when the backup fails |
| POST | `/api/v1/updates/restart` | "Restart to finish": → 202 `{ restarting }` and the app restarts into the installed version. 409 `not_ready` before an install finished, 409 `live` while live with "never update while live" on, 409 `no_updater` in the headless app |

```ts
type Recap = { started_at: number | null /* null: no stream on record yet */; ended_at: number | null; live: boolean;
               follows; subs; tips: Money[]; bits; messages; peak_viewers /* the live session's numbers for this stream; from the log otherwise (peak 0) */;
               new_followers: { platform, name }[]; subscribers: { platform, name, months, gifts }[];
               tippers: { platform, name, amounts: Money[], bits }[]; raiders: { platform, name, viewers }[];
               top_chatters: { platform, name, messages }[] /* top 10, never the streamer or bots */;
               thanks: string[] /* subs and gifters, tippers, raiders, followers; each name once */ }
```
Lists come from the event log (live events only, refunded tips left out), up to 200 each.

`ConnectorState = { state: "off" | "connected" | "reconnecting" | "disconnected" | "sign_in_needed" | "scopes_missing", account: { name, avatar_url } | null, detail: string | null, last_event_at: number | null }`

### Accounts
| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/accounts` | → `Account[]` (`{ platform, name, avatar_url, connected_at, scopes_ok }`) |
| POST | `/api/v1/accounts/{platform}/connect` | → `{ flow: "device_code", user_code, verification_url, expires_at }` (Twitch) or `{ flow: "browser", url }` (YouTube, Kick). The engine opens the system browser itself; the UI shows the code and waits for the `connector` event. |
| DELETE | `/api/v1/accounts/{platform}` | → 204 |

### Overlays
An overlay is one OBS browser source (a 1920×1080 canvas by default) holding widgets.

```ts
type Overlay = { id: string; name: string; url: string; width: number; height: number; widgets: Widget[]; updated_at: number;
                 design: { id: string; scene: string; branded: boolean; follows: boolean } | null /* the gallery design it came from (§4 Designs) */ };
type Widget = { id: string; kind: "alerts" | "chat" | "events" | "goal" | "label" | "viewers" | "poll" | "giveaway" | "layer";
                x: number; y: number; w: number; h: number; config: object;
                locked?: boolean; hidden?: boolean; rotation?: number /* degrees, -360…360 */; opacity?: number /* 0…1 */ };
// locked/hidden/rotation/opacity are left out when false/false/0/1. Widgets are drawn bottom to top in array order.
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/overlays` | → `Overlay[]` |
| POST | `/api/v1/overlays` | `{ name, preset?: "starter" \| "alerts_only" \| "chat_only" \| "blank" }` → `Overlay` |
| GET / PUT / DELETE | `/api/v1/overlays/{id}` | `Overlay` (PUT replaces name, size, widgets) |
| POST | `/api/v1/overlays/{id}/regenerate-url` | → `Overlay` (old URL stops working at once) |
| POST | `/api/v1/overlays/{id}/follow-design` | → `Overlay`: takes its design's layout again and `design.follows` turns true (its URL stays). 400 when it wasn't made from a theme or scene design |
| POST | `/api/v1/overlays/{id}/add-to-obs` | → `{ scene, source, created }`: `created` is false when an existing Ferrocast source was updated in place (422 with a hint if OBS can't be reached) |

Widget `config` per kind:
- `alerts`: `{ theme: string, rule_ids: string[] | null }` (null = all rules)
- `chat`: `{ platforms: Platform[], max_messages: number, fade_after_s: number | null, show_badges: boolean, show_platform: boolean, hide_commands: boolean, hide_bots: boolean, font_size: number, theme: string }`
- `events`: `{ kinds: string[], max_items: number, theme: string }`
- `goal`: `{ goal_id: string, theme: string }`
- `label`: `{ source: "latest_follower" | "latest_sub" | "latest_tip" | "top_tip_session" | "session_follows" | "session_subs", template: string, theme: string }`
- `viewers`: `{ platforms: Platform[], theme: string }`
- `poll`: `{ theme: string }` (shows the running or last poll; hidden when there's none)
- `giveaway`: `{ theme: string }` (how to enter while open, then a winner card)
- `layer` (a piece of a scene): `{ type: "image" | "text" | "shape" | "frame" | "webcam" | "brand" | "fx" | "music" | "sound", anim?: { in: AnimIn, delay_ms: 0…10000, loop: "none" | "float" | "pulse" | "shimmer" }, css?: string }` plus, by type:
  - `image`: `{ media: MediaRef, fit?: "cover" | "contain" | "fill", radius? }`
  - `text`: `{ text (≤500 chars, shown as is), font?: FontFamily, size? 6…400, weight? 100…900, color?, align?: "left" | "center" | "right", uppercase?, italic?, letter_spacing? -0.2…1, shadow?: "none" | "soft" | "hard" | "glow", outline? 0…20, outline_color? }`
  - `shape`: `{ shape?: "rect" | "ellipse" | "line" | "slant" | "triangle", fill?, fill2? (gradient end), angle? 0…360, stroke?, stroke_width? 0…50, radius? }`
  - `frame` / `webcam` (a frame with a see-through middle for the camera): `{ style?: "line" | "double" | "corners" | "notch" | "glow", color?, accent?, thickness? 1…40, radius? 0…400, label? ≤40 }`
  - `brand`: `{ variant?: "mark" | "badge" | "watermark", color? }`: the small Spark mark of a built-in design (below)
  - text countdowns (25 Oct): `{ countdown? 1…86400 (seconds), countdown_look?: "digital" | "thin" | "ring" | "bar" | "flip" | "words", countdown_restart?, countdown_show?, countdown_up? }`. `ring` and `bar` empty as it counts; `words` says "4 minutes"
  - `fx` (a motion background): `{ preset: "hex" | "nightdrive" | "wasteland" | "circuit" | "tunnel" | "dunes" | "forge" | "arena" | "coderain" | "orbit" | "sonar", color?, color2?, background? (the preset's own when unset), speed? 0.25…3, density? 0.5…2, reactive? }`. `reactive` moves it with the music OBS hears (`music` frames, §6)
  - `music` (the wave player or the heartbeat line, with the song): `{ look?: "wave" | "pulse", color?, color2?, text_color?, background? (a panel behind it, hidden with it), font?: FontFamily, title_size? 8…160, show_title? (default true), only_playing? (default true: shown only while music plays) }`
  - `sound` (heard, not seen): `{ on_show?: Stinger, on_show_media?: MediaRef, loop?: "rain" | "wind" | "pad" | "hum", loop_media?: MediaRef, last_ticks?, at_zero?: Stinger, volume? 0…1, duck? }`, `Stinger` = `"whoosh" | "riser" | "impact" | "tick" | "chime" | "glitch"`. `on_show` plays when the screen comes on air, `loop` while it is, `last_ticks` ticks each of a timer's last ten seconds on that screen, `at_zero` plays when it ends, `duck` plays quieter while music plays. The sounds are files in the overlay bundle; a browser source Ferrocast makes sends its audio through OBS (below)
- Any live widget may carry a style from a widget design: `config.style = { variant?, font?, text?, accent?, background?, track?, muted?, radius? 0…64, font_size? 8…120, uppercase?, brand?: BrandSpot }`, with `config.design_id` and `config.branded`.

Colours are `#rgb`, `#rrggbb`, `#rrggbbaa` or `transparent`. `MediaRef` is a media library id or `asset:<path>` (a file bundled with the app, lowercase `a-z 0-9 / _ . -`). `FontFamily` is letters, digits, spaces and hyphens (1–60). Every check fails with 400 naming the field (`layer 'title': config.color: "red;}" isn't a colour; use #rrggbb.`).

PUT `/overlays/{id}` can't change `design`. `design.follows` (set by the engine) is true for overlays made from a theme or scene: saving that design gives them its new layout. A PUT that changes the overlay's widgets or size turns it false, so a hand-edited screen stays as it is until `follow-design`. An overlay made from a **built-in** design (`design.branded`) keeps its `brand` layer, and a widget styled by a built-in widget design keeps `config.branded: true`: an edit that removes, hides or unbrands it is a 409 `branding` ("Duplicate the design in the gallery to make an unbranded copy").

### Alerts
| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/alert-rules` | → `AlertRule[]` (see `core::alert`) |
| PUT | `/api/v1/alert-rules/{id}` | `AlertRule` → `AlertRule` (400 with the validation message) |
| POST | `/api/v1/alert-rules` | `AlertRule` → `AlertRule` |
| DELETE | `/api/v1/alert-rules/{id}` | → 204 |
| POST | `/api/v1/test` | `{ kind, platform?, design_id?, amount? }` → 202 `{ ok: true }` (fires a `test` event through the real pipeline). `amount` (major units; bits, gifted subs, raid viewers or sub months for those kinds; above 0, up to 1,000,000,000) changes the sample's amount, else 400 `amount: …`. `design_id` shows it with that alert design instead of the one in use (the designer's "Send test alert"); 404 when unknown, 400 when it isn't an alert design |
| GET | `/api/v1/queue` | → `{ paused, playing: { alert, started_at } \| null, waiting: { alert, priority, enqueued_at }[] }` |
| POST | `/api/v1/queue/pause` · `/resume` · `/skip` · `/clear` | → queue |
| POST | `/api/v1/queue/{alert_id}/remove` | → queue |
| POST | `/api/v1/events/{event_id}/replay` | → `{ event_id }` |
| GET | `/api/v1/events?before=<seq>&limit=50&kinds=follow,tip` | → `{ events: Event[], next_before: number \| null }` |
| GET | `/api/v1/events/{event_id}/why` | → `{ alerted: boolean, reasons: string[] }` ("No alert rule for Kick raids", "Twitch hid this follow", "Below the ₹100 minimum") |

```ts
type UpdateProblem = { code: string; message: string; hint: string }
type UpdateView = {
  current: string; latest: string | null
  channel: "stable" | "beta" /* read-only: a prerelease build is "beta" */
  blocked_while_live: boolean
  notes_url: string | null /* the GitHub release page */
  notes: string | null /* what's new, plain text ("- " lines are a list); null when the release has none */
  published_at: string | null /* RFC 3339 */
  can_install: boolean /* the desktop app; false in the headless app (and so in a browser on it) */
  checking: boolean
  checked_at: number | null /* ms; when the last check finished, whatever it found */
  error: UpdateProblem | null /* the last check's problem: offline, feed_unavailable, no_build; an earlier find stays in latest */
  install: {
    phase: "idle" | "downloading" | "installing" | "ready" | "failed"
    version: string | null; downloaded: number; total: number | null /* bytes */
    error: UpdateProblem | null /* failed: offline, download_failed, bad_signature, no_permission, install_failed, went_live, gone */
  }
}
```
The desktop app checks 15 s after start, then every 6 hours (15 minutes after a failed check). On Windows `ready` means downloaded and verified: the installer runs at `/updates/restart` (it closes Ferrocast and opens the new version), because it can't run without closing the app.

### Designs
Alert looks, full overlay themes, single scenes and widget styles: made in the dashboard's designer, shipped with the app (built-ins), or imported. Built-ins come with Ferrocast inside the overlay bundle (`designs/catalog.json`, `{ format: "ferrocast-designs", version: 1, designs: Design[] }`; the engine reads it at start, so new ones arrive with app updates and nothing needs a server). A built-in carries a small Ferrocast Spark mark (`branded`) and can't be edited, deleted or exported: **Duplicate** makes the streamer's own copy, with no branding, fully editable. Only built-ins are ever branded; every design the API creates or imports is the streamer's own.

```ts
type DesignKind = "alert" | "theme" | "scene" | "widget"
type Design = { schema: 1; id: string /* built-ins: "builtin.<name>" */; name: string /* 1–60 */; description: string /* ≤200 */;
                tags: { style: "minimal" | "bold" | "retro" | "playful" | "tech" | "cozy"; motion: "still" | "smooth" | "lively" | "bouncy" };
                pro: boolean /* built-ins only */; built_in: boolean; branded: boolean;
                origin: { type: "built_in" | "scratch" | "duplicate" | "import" | "asset_pack"; from: string | null };
                kind: DesignKind; content: AlertContent | ThemeContent | Scene | WidgetContent /* by kind */;
                created_at: number; updated_at: number }

type EventKey = "follow" | "sub" | "resub" | "gift" | "member" | "bits" | "kicks" | "tip" | "superchat" | "raid" | "redeem"
// follow; subscription → member on YouTube, resub at 2+ months, else sub; gift_batch → gift; platform_gift → kicks for KICKs, else bits;
// paid_message → superchat; tip; raid; reward_redemption → redeem. A missing look falls back: resub, member, gift → sub; kicks → bits;
// superchat → tip. An event with no look plays the alert rule's own look.
type AlertContent = { events: { [key in EventKey]?: { variations: DesignVariation[] } } }   // ≥1 event
type DesignVariation = { id: string; name: string /* 1–40 */; condition: Condition | null; priority: number; look: AlertLook }
// 1–10 per event, exactly one with condition null (the default). Picked like rule variations: the highest-priority match, else the default.
type AlertLook = {
  frame: "plate" | "neon" | "comic" | "glass" | "pixel" | "banner" | "pill" | "none";
  layout: "above" | "left" | "right" | "over" | "text" | "custom";
  media: { kind: "none" | "emblem" | "image" | "video"; ref: MediaRef | null }; media_size: number /* 24–480, on an 800×450 alert canvas */;
  offsets: { text_x, text_y, media_x, media_y } /* -400…400, used by layout "custom" */;
  title: string /* template, ≤300 */; body: string /* template, ≤500 */;
  font: { family: FontFamily; source: "bundled" | "google" | "system"; size: 10–160; weight: 100–900; uppercase: boolean; italic: boolean; letter_spacing: -0.2…0.5 };
  colors: { text; highlight /* name and amount */; plate; accent; outline };
  plate: boolean; shadow: "none" | "soft" | "hard" | "glow"; outline: 0–12;
  anim: { in: AlertIn; out: AlertOut; text: TextAnim; in_ms: 0–3000; out_ms: 0–3000; text_delay_ms: 0–10000 };
  motion?: Motion /* 1.1; a look without it reads as the defaults below, which play exactly like 1.0 */;
  sound: { ref: MediaRef | null /* plays instead of builtin */; builtin: "none" | "sparkle" | "glint" | "cascade" | "bloom" | "lift" | "tally" | "halo" | "droplet" /* Crystal */ | "kalimba" | "marimba" | "glow" | "ember" | "pebble" | "feather" | "hearth" | "wink" /* Felt */ | "sparkle_classic" | "chime" | "coin" | "whoosh" | "boing" | "bass" /* retired in 1.1: still accepted, the overlay plays Glint, Tally, Lift, Droplet, Ember */; volume: 0–100 };
  duration_ms: 1000–60000 /* more than in_ms + out_ms */;
  tts: { enabled: boolean; min_amount: number /* major units; 0 reads all */ };
  brand: "corner" | "divider" | "watermark" | "notch" | "badge" | "edge" /* where the mark sits when the design is branded */;
  css: string /* ≤20,000; no @import, url() only for data:image/… */ }
type AnimIn = "none" | "fade" | "rise" | "drop" | "slide_left" | "slide_right" | "zoom" | "pop" | "bounce" | "flip" | "wipe" | "magnet"
type AnimOut = "none" | "fade" | "sink" | "lift" | "slide_left" | "slide_right" | "zoom" | "shrink" | "flip" | "wipe"
// AnimIn is all a scene layer takes. Alert looks also take the motion designer's (1.1):
type AlertIn = AnimIn | "slam" | "elastic" | "bolt" | "glitch" | "portal" | "shatter" | "pour" | "flip_shine" | "confetti_pop" | "dash" | "neon"
type AlertOut = AnimOut | "dissolve" | "black_hole" | "zip" | "fold" | "shatter" | "tv_off" | "fall" | "iris" | "glitch"
type TextAnim = "none" | "wave" | "wiggle" | "bounce" | "pop" | "type" /* Typewriter, with a cursor since 1.1 */ | "glow"
              | "cascade" | "pop_wave" | "decode" | "shine" | "neon" | "rgb" | "stretch" | "spin"
// Every 1.0 id keeps its meaning (they map to themselves); in_ms/out_ms are the motion's real lengths for the 1.1 ids.
type Motion = { idle: "none" | "breathe" | "float" | "shimmer" | "glow" | "jelly" | "heartbeat" /* none */;
  particles: "auto" | "none" | "confetti" | "sparks" | "petals" | "coins" | "embers" | "stars" /* auto: the entrance's own; the 1.0 ones have none */;
  colors: "brand" | "rainbow" | "gold" | "white" | "custom" /* brand = the look's colours */; custom_color: Color /* #ff5aa5 */;
  bounce: 0–100 /* 45 */; speed: 60–160 /* %, 100 */; stagger: 0–90 /* ms between letters, 34 */;
  shake, flash, vignette, zoom_punch: boolean /* true; only the 1.1 entrances and exits have impacts */;
  by_amount: boolean /* true: bigger amounts hit harder (AlertDesignView.intensity) */;
  position: "tl" | "tc" | "tr" | "ml" | "mc" | "mr" | "bl" | "bc" | "br" /* mc, where 1.0 put it */;
  direction: "auto" | "top" | "left" | "right" | "bottom" /* auto */;
  impact_sounds: boolean /* false; synthesised hits at the look's sound volume */;
  beat: boolean /* false; delays the entrance so its hit lands on a downbeat */; bpm: 70–170 /* 120 */;
  recipe: "heavy" | "arcade" | "cyber" | "molten" | "storm" | "party" | "clean" | null }
// Every Motion field has a default, so a partial object reads; a wrong type or unknown name is refused.

type ThemeContent = { scenes: Scene[] /* 1–8, keys unique */; alert_design: string | null /* the alert design that goes with it */ }
type Scene = { key: "in_game" | "just_chatting" | "starting_soon" | "getting_ready" | "brb" | "paused" | "offline" | "ending" | `custom_${number}`; name: string /* 1–40 */;
               width: number; height: number; background: string | null; layers: Widget[] /* checked exactly like an overlay's widgets */ }
type WidgetContent = { widget: WidgetKind /* not "layer" */; w: 40–1920; h: 20–1080; config: object /* starting settings, no style */; style: WidgetStyle }

type AlertDesignView = { design_id: string; variation_id: string; event_key: EventKey; branded: boolean; look: AlertLook;
                         title_parts: { text: string; hl: boolean }[]; body_parts: { text: string; hl: boolean }[];
                         intensity: 0.6–2 /* 1.1, missing reads as 1: the amount against a usual one on a log scale (a usual tip
                         in its own currency, e.g. ₹500 or $5, = 1; ×10 = 1.5; ×100 = 2; a tenth = 0.6); Bits/KICKs/Jewels against 500,
                         gift batches against 5; other events 1 */ }
// title/body already rendered; hl marks text from {name}, {amount}, {value}, {count}, {count_text}, {viewers}, {viewers_text}, {reward}.
```

**The alert design in use** dresses every alert (and the overlay `?test` previews): the rule still decides whether an event alerts, the design gives the look, text, media, sound, length and CSS, and its `tts` switch and minimum apply on top of the TTS policy (a message the design doesn't read is never held for approval either). With none picked, alerts look exactly as before and carry no `design`.

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/designs?kind=alert` | → `Design[]`: built-ins first (catalogue order), then your own, newest first. 400 `kind: …` for an unknown kind |
| POST | `/api/v1/designs` | `Design` → 201 `Design`, always your own: a new id, never built-in, branded or Pro. `origin.type` may be `scratch`, `duplicate` or `asset_pack` (anything else becomes `scratch`) |
| GET | `/api/v1/designs/{id}` | → `Design` |
| PUT | `/api/v1/designs/{id}` | `Design` → `Design`. 409 `built_in` for a built-in; 409 `conflict` when the kind would change; `id`, `origin` and `created_at` stay as they were |
| DELETE | `/api/v1/designs/{id}` | → 204. 409 `built_in`; 409 `in_use` while it's the alert design in use |
| POST | `/api/v1/designs/{id}/duplicate` | `{ name? }` (body optional) → 201 `Design`: your own copy, unbranded, named "… copy", without the mark layers. A theme whose alert design is a built-in gets a copy of that too |
| POST | `/api/v1/designs/{id}/use` | `{ overlay_id?: string, scenes?: string[], alert_style?: boolean }` (body optional) → `{ alert_design: string \| null, overlays: Overlay[] }`. Alert design: becomes the one in use. Theme: one new overlay per scene (or the `scenes` listed), named "Scene (Theme)", plus its alert design when `alert_style` isn't false. Scene: a new overlay. With `overlay_id` (one scene only), that overlay's size and widgets are replaced and its URL stays, so OBS keeps working. Widget design: adds the styled widget to `overlay_id` (required). Layers keep their ids; built-ins keep their mark. 400 names a bad field, 404 for an unknown overlay, 409 when the overlay is full |
| GET | `/api/v1/designs/{id}/obs` | Theme in OBS (2.0, 24 Oct board): Ferrocast's guesses for putting a theme into OBS → `{ plan: ThemePlan, screens: { key, name }[], obs_scenes: string[], obs_cameras: string[], old_widgets: { source, vendor: "StreamElements" \| "Streamlabs", scenes: string[] }[], applied: ObsTheme \| null }`. An old widget's address (it holds that service's key) is never returned. 400 `design:` for a design that isn't a theme, 404 unknown design, 422 `obs_*` when OBS can't be reached |
| POST | `/api/v1/designs/{id}/obs` | `ThemePlan` (body optional: empty uses the guesses) → `{ steps: Step[], applied: ObsTheme }`. Each screen becomes an overlay (the one made from that screen before is reused, so its URL stays) and goes into OBS: `new` makes the scene `prefix + screen name`, `on_top` adds it on top of a scene the streamer has, `replace` does that and hides the scene's other browser and image sources, `skip` leaves it out. `under` (a scene or source OBS has) goes at the bottom: the streamer's game or camera, as a nested scene. A new browser source is scaled to fill OBS's canvas. Then Start show's scenes (`role` → starting, live, ending) and the BRB scene are set, `hide_old` widgets are hidden (not deleted), the theme's alert look is used when `alert_style`, and OBS switches to the Starting screen when `switch_to_starting` (never while OBS is streaming: that step is skipped). A step that fails says why; the rest still run. Doing it again adds nothing twice. 400 names a bad field (`screens.0.under: …`). With another theme in OBS, this one takes its place: the same overlays (their URLs stay) and the scenes and sources Ferrocast made are renamed for it; Undo still goes back to before the first theme |
| GET / DELETE | `/api/v1/obs/theme` | GET → `ObsTheme \| null`: `{ design_id, design_name, at, screens: { key, name, scene, role, found: boolean \| null }[] }` (`found` null while OBS can't be asked). DELETE takes it out → `{ steps: Step[], applied: null }`: only what Ferrocast added (its scenes, its sources, items it put in the streamer's scenes), hidden sources shown again, OBS back on the scene it was on, Start show's scenes and the alert look back where they're still what Ferrocast set. 404 `no_theme_in_obs`; 409 `theme_on_air` while OBS streams one of the theme's scenes |
| GET / PUT | `/api/v1/designs/active-alert` | `{ design_id: string \| null }`. PUT 404 for an unknown design, 400 for a design that isn't an alert design |
| POST | `/api/v1/designs/{id}/export` | → a `.ferrodesign` download (`application/zip`). 409 `built_in` (duplicate it first) |
| POST | `/api/v1/designs/import` | multipart `file` (up to 100 MB) → 201 `{ designs: Design[], media_files: number, notes: string[] }` |

**`ThemePlan`** (unknown fields are refused): `{ prefix: string (≤ 40, e.g. "Forge · "; empty names scenes after the screen), screens: { key: string, place: { kind: "new" } | { kind: "on_top" | "replace", scene: string } | { kind: "skip" }, under?: string | null, role?: "starting" | "live" | "brb" | "ending" | null }[], switch_to_starting?: boolean (default true), hide_old?: string[], alert_style?: boolean (default true), mic_on_breaks?: boolean }`. `mic_on_breaks` is false by default: no mics are added. When true, the break screens (Starting soon, Getting ready, BRB, Paused, Offline, Ending) get Start show's mic check list (or, when it's empty, every mic OBS has), except a mic the source under that screen already has; turning it off on a later apply takes out the mics Ferrocast added. The guesses: every screen `new`; under In game the scene that names or captures the game (a camera counts; never a break or recording-only scene); under Just chatting a scene named chat or talk, else a camera-only scene, else a camera; roles from the screen; every old widget hidden. `Step` is Start show's `{ id, label, status: "done" | "failed" | "skipped", detail }`.

**`.ferrodesign`** (version 1) is a zip with `manifest.json` (`{ format: "ferrodesign", version: 1, app_version, created_at, name, kind, designs: number, files: { "media/<sha256>.<ext>": { sha256, bytes, mime } } }`), `designs/<n>.json` (a theme's own alert design travels with it) and `media/<sha256>.<ext>` (png, jpg, gif, webp, mp3, ogg, wav, webm, mp4). Inside the designs, library files are named by their SHA-256. Nothing else is allowed in the file: no scripts, web pages or SVG, no settings, tokens, overlay URLs or viewer data. Import checks every name, every file's real type, size and checksum and every design, then always adds **new** designs of your own (new ids, unbranded, `origin.type: "import"`) and reuses a library file with the same content. Errors: 413 `too_large` (the file over 100 MB, one media file over its limit, more than 10 designs or 300 files), 415 `wrong_content_type` (not multipart), 415 `bad_design_file` (not a zip, or not a Ferrocast design file: a `.ferropack` too), 415 `unsafe_design_file` (a script, page or unexpected file inside), 415 `damaged_design_file` (a checksum doesn't match), 415 `unsupported_type` (a media file that isn't a picture, sound or video Ferrocast plays), 422 `design_too_new` (made by a newer Ferrocast), 422 `invalid_design` (the message names the file and field: `designs/1.json: content.events.tip.variations[0].look.colors.highlight: "red" isn't a colour; use #rrggbb.`), 400 when there's no `file` field or it's empty. Nothing is saved when an import fails.

Designs are in `.ferropack` exports (`designs`); a pack can't bring in a built-in or branded design, or one that fails these checks (it's skipped). Media used by a design can't be deleted (409 `in_use` names the design).

### Goals
`Goal = { id, title, kind: "follows" | "subs" | "tips" | "bits" | "custom", target: number, currency: string | null, starts_at, ends_at: number | null, current: number }`
GET/POST `/api/v1/goals`, PUT/DELETE `/api/v1/goals/{id}`, POST `/api/v1/goals/{id}/adjust { delta }`.
Tips goals are in **major units** of their currency everywhere (API, `goals` frames, overlay `state`): ₹3,450.50 is `3450.5`; `delta` too. A tips goal needs a `currency`; other goals take whole numbers and no currency (400 otherwise). `current` is never taken from a PUT, and a goal with progress can't change its kind or currency (409).

### Chat and moderation
| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/chat?limit=200` | → `Event[]` (recent chat_message events) |
| POST | `/api/v1/chat/send` | `{ text, platforms: Platform[] \| "all", reply_to?: { platform, message_id } }` → `{ sent: Platform[], failed: { platform, reason }[] }` |
| POST | `/api/v1/mod/delete` | `{ platform, message_id }` → 204 |
| POST | `/api/v1/mod/timeout` | `{ platform, user_id, seconds, reason? }` → 204 |
| POST | `/api/v1/mod/ban` | `{ user: { platform, user_id, login, display_name? }, reason?, everywhere: boolean }` → `{ banned: Platform[], failed: [...] }` |
| POST | `/api/v1/mod/unban` | `{ platform, user_id }` → 204 (always asks the platform, so bans the filter made can be undone) |
| GET | `/api/v1/mod/bans` | → `BanEntry[]`, newest first |
| GET | `/api/v1/mod/panic` | → `{ on, plan: PanicConfig }` |
| POST | `/api/v1/mod/panic` | `{ on: boolean }` → `{ on, applied: { platform, modes: string[] }[] }`; also sends `panic` and one `notice` frame with `key: "panic"` (it replaces the previous panic notice) |
| GET / PUT | `/api/v1/mod/panic/config` | `PanicConfig` (400 names the field) |
| GET / PUT | `/api/v1/mod/filters` | `Filters` (400 names the field, e.g. `timeout_seconds: must be between 1 second and 14 days.`) |
| GET | `/api/v1/mod/held` | → held chat `Event[]`, oldest first |
| POST | `/api/v1/mod/held/{event_id}/release` · `/drop` | → 204: release shows it in chat overlays; drop deletes it on the platform. 404 `held_not_found` when another mod got there first |
| GET | `/api/v1/viewers/{platform}/{user_id}` | → `ViewerCard` (404 `viewer_not_found`) |
| GET / PUT | `/api/v1/viewers/{platform}/{user_id}/note` | `{ text }` (up to 2,000 characters) |

```ts
type BanEntry = { platform; user_id; login; display_name; reason: string | null; banned_at: number; everywhere: boolean; by: string | null; offline: boolean /* banned from a phone through the cloud bot while this PC was off (§9.5 Offline, M10) */ }
type FilterAction = "delete" | "timeout" | "ban" | "hold"
type Filters = { words: { enabled; list: string[]; lookalikes: boolean; action }, links: { enabled; allow_roles: number; allow_domains: string[]; action; hold_first_time? },
                 caps: { enabled; min_length; max_percent; action }, emotes: { enabled; max; action }, symbols: { enabled; max_percent; action },
                 solicitation: { enabled; action }, timeout_seconds: number, exempt_roles: number, max_length? }
type PanicConfig = { followers_only; followers_only_minutes; emote_only; slow_mode_seconds /* 0 = leave alone */; hold_new_accounts; pause_alerts; pause_tts; hotkey: string; subscribers_only? }
type ViewerCard = { platform; user_id; login; display_name; avatar_url: string | null; roles: number; first_seen: number | null; messages: number;
                    follows_since: number | null; note: string; banned: boolean; timeouts: number; other_identities: { platform, login }[];
                    points: number /* loyalty balance, shared by linked accounts */ }
```

### Bot
GET/POST `/api/v1/commands`, PUT/DELETE `/api/v1/commands/{id}`; GET/POST `/api/v1/timers`, PUT/DELETE `/api/v1/timers/{id}`.
`Command = { id, name: "!discord", aliases: string[], response: string (template), roles: "everyone" | "subs" | "vips" | "mods", cooldown_s: number, user_cooldown_s: number, platforms: Platform[] | [], enabled }` (`id` may be blank on POST; the engine picks one)
`Timer = { id, name, message, every_min: number, min_chat_lines: number, platforms: Platform[] | [], enabled }`

POST `/api/v1/commands/starter` → `{ added: Command[], skipped: string[] }`: adds `!socials` (switched **off**: its reply is a placeholder until the streamer writes their links, so viewers never see it), `!lurk`, `!uptime` and `!so` (mods only), skipping any name a command or alias already uses (two clicks at once never add a command twice). Reply variables the engine fills in: `{uptime}` ("1h 5m", only while live), `{uptime_text}` ("We've been live for 1h 5m." or "We're not live right now."), and for replies that use them, `{target_game}` (what the first argument's channel last streamed, empty when unknown) and `{target_game_note}` ("They were last playing Hades II." or empty). The game comes from the platform when it offers a lookup (Twitch: Get Channel Information); elsewhere the shoutout is plain.

### Loyalty points
One balance per person: linked accounts on Twitch, YouTube and Kick share it. Never below 0, never above 1,000,000,000,000.

```ts
type LoyaltyConfig = { enabled: boolean; name: string /* "points", 1–20 letters, numbers, spaces, - _ */;
                       per_interval: number; interval_min: number /* 1–120 */;
                       follow_bonus: number; sub_bonus: number /* per sub, and per gifted sub for the gifter */;
                       tip_bonus: number /* per tip, Super Chat, Bits cheer or Kicks gift */; raid_bonus: number }  // amounts 0–1,000,000
type LeaderboardEntry = { rank: number; platform; user_id; login; display_name; points: number }
```

| Method | Path | Body → Response |
|---|---|---|
| GET / PUT | `/api/v1/loyalty` | `LoyaltyConfig`. PUT saves only the fields sent; 400 names a bad field (`interval_min: must be between 1 and 120 minutes.`) |
| GET | `/api/v1/loyalty/leaderboard?limit=20` | → `LeaderboardEntry[]`, highest first, balances above 0 only (`limit` 1–100, else 400 `limit: …`) |
| GET | `/api/v1/loyalty/viewers/{platform}/{user_id}` | → `{ points, name }` (404 `viewer_not_found`) |
| POST | `/api/v1/loyalty/viewers/{platform}/{user_id}/adjust` | `{ delta }` (whole number, not 0, at most ±1,000,000,000) → `{ points, name }`. Taking more than they have is a 400 `delta: they have 75 points, so you can take away at most 75.` |

Defaults: on, "points", 10 every 5 minutes for each viewer who chatted while their platform was live, 50 per follow, 250 per sub, 100 per tip, 100 per raid. Only live events pay (never tests, replays or backfill), each event once. The streamer, bots (the bot role and well-known bot accounts) and anonymous supporters never earn.

Chat (when the bot is on): `!points` (or `!<name>`, like `!coins`) replies with the sender's balance, once per 15 s per person. Mods: `!addpoints @name 100`, `!removepoints @name 100` (stops at 0). A custom command with the same name wins. Balances are in `.ferropack` exports (`loyalty_balances`).

### Polls
Chat polls that work the same on Twitch, YouTube and Kick: viewers type `!vote 2`. No platform poll API, so no extra permissions.

```ts
type PollView = { id: string; question: string; options: { text: string; votes: number }[]; total: number;
                  started_at: number; ends_at: number; closed: boolean; leaders: number[] /* indexes of the leading answers; several on a tie */ }
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/polls/current` | → `PollView \| null`: the running poll, or the last one that finished |
| POST | `/api/v1/polls` | `{ question (1–120 chars), options: string[] (2–5, 1–60 chars, no duplicates), duration_s (10–1800, default 60) }` → 201 `PollView`. 409 while another poll is open. The poll is announced in chat on every connected platform |
| POST | `/api/v1/polls/current/end` | → `PollView`: closes it now and posts the result. 404 `poll_not_found`; 409 when it already ended (the timer or another dashboard got there first) |
| DELETE | `/api/v1/polls/current` | → 204: removes it without posting anything (404 `poll_not_found`) |

One vote per person: linked accounts vote once, voting again changes the vote, a vote after the end doesn't count. Only live chat in this channel votes (not Shared Chat from a partner, not replays). When time runs out the result is posted in chat. Polls live in memory: a restart forgets them.

### Giveaways
Viewers type a keyword (`!join`) on any platform to enter; entries can cost loyalty points.

```ts
type GiveawayEntrant = { platform; user_id; login; display_name; tickets: 1 | 2 }
type GiveawayView = { id; prize; keyword; cost: number; subscriber_bonus: boolean; started_at; open: boolean /* taking entries */; ended: boolean;
                      entries: number; tickets: number; entrants: GiveawayEntrant[] /* newest first, up to 500 */; winners: GiveawayEntrant[] }
type GiveawayCard = { id; prize; keyword; open: boolean; entries: number; winner: GiveawayEntrant | null /* the latest */ }  // for overlays
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/giveaways/current` | → `GiveawayView \| null`: the running giveaway, or the last one that ended |
| POST | `/api/v1/giveaways` | `{ prize (1–100 chars), keyword?: "!join", cost?: 0 (points, 0–1,000,000,000; needs points turned on), subscriber_bonus?: false }` → 201 `GiveawayView`, announced in chat. 409 while another hasn't ended |
| POST | `/api/v1/giveaways/current/close` | → `GiveawayView`: stop taking entries (409 when already closed) |
| POST | `/api/v1/giveaways/current/draw` | → `GiveawayView`: closes entries and draws the first winner, announced in chat. 409 if one was already drawn (two clicks at once draw once), 422 when nobody entered |
| POST | `/api/v1/giveaways/current/redraw` | → `GiveawayView`: draws another winner, never someone who already won. 409 before the first draw, 422 when everyone has won |
| POST | `/api/v1/giveaways/current/end` | → `GiveawayView`: finished; fees are spent (409 when already ended) |
| DELETE | `/api/v1/giveaways/current` | → `{ refunded }`: cancels it and gives every unfinished entry fee back |

All of them answer 404 `giveaway_not_found` when there's no giveaway. One entry per person (linked accounts enter once); subscribers and members get 2 tickets with `subscriber_bonus`. The streamer, bots and anonymous accounts can't enter. Someone who can't afford the fee is told once. Winners are drawn with the operating system's random numbers, weighted by tickets, without modulo bias. Fees are recorded with the entry: if Ferrocast stops before the giveaway ends, they're refunded at the next start.

### First-time chatters
The dashboard marks someone's first message ever (counting all their linked accounts) and people chatting again after a while, with the live `chatter` frame. A welcome message is optional and **off** by default.

`WelcomeConfig = { enabled: boolean /* post the welcome */; template: string /* 1–300 chars; {user}, {name}, {platform} */; returning_badge: boolean; returning_days: number /* 7–365, default 30 */ }`

GET / PUT `/api/v1/welcome`: PUT saves only the fields sent; 400 names a bad field. The welcome replies on the chatter's own platform, only for live chat in this channel, never during panic mode or when the bot is off, and at most once every 10 seconds (a raid of first-timers doesn't flood chat).

### TTS
GET/PUT `/api/v1/tts` (`{ enabled, voice, min_amount, currency, max_chars, hold_for_approval, read_names }` plus `min_amounts: Money[]` and `min_platform_value`). `min_amount` is in major units of the main currency (Settings `currency`, from the computer's region by default; echoed as `currency`), and wins over that currency's entry in `min_amounts`; the other currencies keep their own minimum. Every field is optional on PUT: what isn't sent is kept.
GET `/api/v1/tts/voices` → `{ id, name, language, pro }[]`: the computer's own voices (`pro: false`), then, while Pro is active and the relay has them, Ferrocast voices (`pro: true`, ids `google:…`, plus `gender` and `family`; §9.4). GET / PUT `/api/v1/tts/cloud` and POST `/api/v1/tts/preview`: §9.4. GET `/api/v1/tts/pending` and the `tts_pending` frame → `{ id, event_id, platform, name, text, amount: string | null ("₹500", "1,000 bits"), received_at }[]`. POST `/api/v1/tts/{id}/approve` · `/reject`.

### Tips
The streamer uses their own provider pages; the relay verifies each provider's webhook and forwards name, amount and message only.

```ts
type TipProviderState = { page_url: string; page_id: string | null; test_mode: boolean; has_secret: boolean }
type TipSettings = { handle; display_name; message; page_url: string | null; webhooks: { razorpay; kofi; stripe } | null;
                     razorpay: TipProviderState | null; kofi: TipProviderState | null; stripe: TipProviderState | null }
```
GET `/api/v1/tips` → `TipSettings` (never secrets). PUT with the same fields; a provider is `null` (off) or `{ page_url, page_id?, test_mode?, secret? }`, where a blank secret keeps the saved one → the normalised `TipSettings`. DELETE → 204 (page down; the name stays held for this install for 30 days).
Errors: 400 `invalid` (handle 3–30 of `a-z 0-9 - _`, reserved words, links must be https on the provider's own domains, Stripe secrets start `whsec_`, `page_id` must be a `plink_` id), 409 `conflict` (name taken), 429 `rate_limited` (three name changes a day), 422 `unavailable` (no relay in this build or the relay can't be reached).

### Ferrocast Pro
Pro is billed through the relay (Razorpay Subscriptions, INR); nothing the free app does needs it (R-05). The engine asks the relay with this install's identity and remembers the last answer: it is reused for 10 minutes, and when the relay can't be reached a status the relay confirmed keeps counting for 7 days (`offline_grace_days`), never past `access_until`. Pro is **not on sale** until the relay's `PRO_SALES_OPEN` is switched on (after version 1).

```ts
type ProView = {
  active: boolean            // Pro right now, offline grace included: use this
  pro: boolean               // what the relay last said
  status: "none" | "created" | "authenticated" | "active" | "pending" | "halted" | "paused" | "cancelled" | "completed" | "expired"
  plan: "monthly" | "yearly" | null
  subscription_id: string | null       // sub_…, also in Razorpay's emails (for Restore)
  current_end: number | null           // end of the paid period (ms)
  access_until: number | null          // when Pro stops (period + 3-day grace for a late renewal; no grace once cancelled)
  cancel_at_period_end: boolean; refunded: boolean; installs: number /* of 3 */
  sales_open: boolean
  prices: { currency: "INR"; monthly: 19900; yearly: 199900 } | null   // paise
  checked_at: number                   // when the relay said this (ms; 0 = never)
  stale: boolean                       // the relay couldn't be reached; this is the remembered answer
  offline_grace_days: 7
  launch_note: string                  // "Ferrocast Pro launches after version 1. …"
  warning: string | null
  features: ProFeatures                // which Pro features the relay has (§9); all false until each ships
  quotas: { [name: string]: ProQuota } // this subscription's counters (§9); {} without one
}
type ProFeatures = { cloud_bot: boolean; remote: boolean; tts: boolean; discord: boolean; backups: boolean; tip_brand: boolean; history: boolean; weekly_email: boolean; tip_domain: boolean; smart: boolean }
type ProQuota = { used: number; limit: number; period: "day" | "month" /* UTC calendar */; reset_at: number /* ms */ }
```
Show a Pro feature's panel only when `features.<name>` is true and `active || sales_open` (§9).

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/pro` | → `ProView` (never an error: no relay answer at all is `status: "none"`, `stale: true`, with a `warning`) |
| POST | `/api/v1/pro/refresh` | → `ProView`, asking the relay and Razorpay now (e.g. after paying) |
| POST | `/api/v1/pro/checkout` | `{ plan: "monthly" \| "yearly", email }` → 201 `{ checkout_url, subscription_id, plan, amount, currency }`. Open `checkout_url` (Razorpay's hosted page) in the browser, then POST `/pro/refresh`. |
| POST | `/api/v1/pro/manage` | → 201 `{ url, expires_at }`: the relay's manage page (status, stop renewing), valid 30 minutes. Open it in the browser. |
| POST | `/api/v1/pro/cancel` | → `ProView`: stops renewing; Pro lasts until `current_end` |
| POST | `/api/v1/pro/restore` | `{ subscription_id, email }` → `ProView`: moves Pro onto this install (new PC, reinstall). At most 3 installs share one subscription; the oldest drops off. |
| GET | `/api/v1/pro/history` | `?from&to&platform` (ms; `twitch` \| `youtube` \| `kick`) → the relay's `{ from, to, streams, truncated, weekly, plan, keep_days, earliest, limited }` (§9.1). Free with a signed-in account (30 days), Pro a year. 400 for a bad `from`/`to`/`platform`, then the relay's errors (403 `account_required` when this PC has neither) |
| GET | `/api/v1/pro/history/samples` | `?platform&key` → `{ platform, key, samples: [{ at, viewers }] }`: one stream's viewer count every 5 minutes (§9.1). 400 without both, then the relay's 404 `stream_not_found` |
| GET | `/api/v1/pro/history/export.csv` | Same filters → `text/csv` download (`ferrocast-history.csv`) |
| GET, PUT, DELETE | `/api/v1/pro/history/email` | The weekly email: its state, save an address (202, a confirm email sent), stop (§9.8) |
| GET, PUT, DELETE | `/api/v1/pro/tips/domain` | The tip page's own address: its status and DNS records (CNAME + TXT proof), add `{ hostname }` (201; 200 for the same one), remove (§9.2) |
| GET, POST, … | `/api/v1/pro/backups…` | Cloud backups: list, back up now, recovery key, passphrase, restore, delete (§9.3) |

Errors pass the relay's code, message and hint through: 409 `pro_not_on_sale` (hint: launches after version 1), 409 `already_pro`, 409 `already_cancelled`, 409 `not_cancellable`, 404 `no_subscription`, 404 `not_found` (restore: id and email don't match, always the same answer), 422 `invalid_plan` / `invalid_email` / `invalid_subscription`, 429 `rate_limited`, 502 `razorpay_unavailable`, 503 `billing_unavailable` (no keys on the relay yet) / `billing_misconfigured`, 503 `relay_unrecognised` (the relay forgot this install; it re-registers by itself), 502 `relay_unreachable`. Pro feature calls (§9) also pass through 402 `pro_required`, 413 `too_large`, 415 `unsupported_type`, 429 with the relay's `Retry-After`, 503 `vault_unavailable` / `storage_unavailable`. 400 for a body that isn't JSON or misses a field, 415 without `Content-Type: application/json`.

### Smart features (Pro, 1.4)
Smart moderation, smart replies and the written stream recap (`crates/engine/src/smart.rs`; the relay side is §9.10). The dashboard never calls them "AI": they're *smart moderation*, *smart replies* and the *stream recap*. They run only with Pro and `features.smart` (the relay has a free-tier model set up); otherwise nothing leaves the PC and the switches say why.

- **Smart moderation** takes a second look at chat that passed the streamer's own filters, only messages a cheap local check finds doubtful (a first message, a link, a long number, words that are often abuse in English, Hindi or Hinglish, spelling tricks like `bh3nch0d` or `m.c`, shouting, floods), never a mod's, the streamer's or an exempt role's. They're checked in batches of up to 20 (or after 5 s). A verdict with confidence ≥ 0.6 becomes a **suggestion** (`smart_mod` live frame, the Moderation page): Delete, Time out, Ban or Dismiss, done as the streamer. `moderation: "act"` also times out a very clear case (≥ 0.85, a timeout or ban verdict) on its own for `act_timeout_s`, never bans and never deletes, logged with **Undo** (lifts the timeout). The last 200 suggestions are kept in memory only.
- **Smart replies** answer a live chat question (a `?`, or a first word like *what, when, how, kya, kab, kaise, kitne*) that no command or auto-reply answered, from the streamer's own `about` facts **only after the streamer approved that exact text** (`about/approve`; changing it needs approving again). At most one every `reply_gap_s` (default 60 s) per platform and one per viewer every 10 minutes, only while the bot is on, live, and panic mode is off. The relay refuses any reply with a link the facts don't contain. The last 50 replies are listed on the Bot page.
- **Stream recap**: a short written recap of this (or the last) stream from the recap's numbers, kept for that stream.

When the relay says to wait (429 `smart_rate` / `smart_quota`, 503 `smart_resting` / `smart_off`, 402), that feature pauses for its `Retry-After` (15 minutes for 402/503 without one, at most a day) and the view shows `last_error` with its message and hint. The streamer's own filters, commands and auto-replies never wait on any of it.

```ts
type SmartView = {
  settings: { moderation: "off" | "suggest" | "act"; act_timeout_s: number /* 10–600, default 60 */; rules: string /* ≤ 500 */;
              replies: boolean; about: string /* ≤ 2,000 */; reply_gap_s: number /* 30–3,600, default 60 */ }
  about_approved: boolean
  pro: boolean; available: boolean /* features.smart */; active: boolean /* both */
  quotas: { smart_moderation?: ProQuota; smart_replies?: ProQuota; smart_recaps?: ProQuota }   // from the last Pro status
  moderation_paused_until: number | null; replies_paused_until: number | null
  last_error: { status, code, message, hint, retry_after, at } | null
  waiting: number                      // messages queued for a check
}
type SmartSuggestion = {
  id: string /* the chat event's id */; platform; user_id: string | null; login: string | null; name: string; text: string
  message_id: string | null; action: "delete" | "timeout" | "ban"; category: string; reason: string; confidence: number; at: number
  state: "suggested" | "done" | "undone" | "dismissed"; done: "delete" | "timeout" | "ban" | null; by: "smart" | "you" | null
  seconds: number | null; error: string | null
}
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/pro/smart` | → `SmartView` (never an error) |
| PUT | `/api/v1/pro/smart` | any of `{ moderation, act_timeout_s, rules, replies, about, reply_gap_s }` → `SmartView`. 400 for a value out of range (the message names it) or an unknown field. A new `about` clears the approval |
| POST | `/api/v1/pro/smart/about/approve` | `{ about }` (the text the page shows) → `SmartView`. 409 `about_changed` when it isn't the saved text any more; 400 when there are no facts yet |
| GET | `/api/v1/pro/smart/moderation` | → `{ items: SmartSuggestion[] /* newest first */, replies: { at, platform, name, question, reply }[] }` |
| POST | `/api/v1/pro/smart/moderation/{id}/apply` | `{ action?: "delete" \| "timeout" \| "ban", seconds? }` (empty body: what it suggests; a timeout defaults to 10 minutes) → the suggestion. 404 `not_found` (gone from the last 200), 409 already done, 400 when the platform gave no message id (delete) or user id |
| POST | `/api/v1/pro/smart/moderation/{id}/undo` | → the suggestion (`undone`): lifts a timeout or ban. 409 `cant_undo` for a delete |
| POST | `/api/v1/pro/smart/moderation/{id}/dismiss` | → the suggestion (`dismissed`) |
| GET | `/api/v1/recap/summary` | → the last written recap `{ started_at, summary, at }` (all `null` before the first) |
| POST | `/api/v1/recap/summary` | `?since=` (optional, as `/recap`) → `{ started_at, summary, at }`. 402 `pro_required`, 503 `smart_off`, 409 `no_stream`, then the relay's 429 `smart_quota` (3 a day) / `smart_rate`, 503 `smart_resting`, 502 `smart_unavailable` |
| GET | `/api/v1/insights/supporters` | `?from&to` (ms; `from` defaults to 30 days ago, up to 400 days) → `{ from, to, truncated, totals, new_followers, subscribers, tippers, raiders, top_chatters }` from **this PC's own event log** (not Pro, no account): subscribers and gifters (most gifts first), tippers (Bits, then the largest amount, each currency on its own, R-32), raiders (most viewers first), the 10 most active chatters in the range. 400 for a bad time, 422 `invalid_range` |

Phones (§9.5): everything under `/pro/smart` stays on the PC; `/recap/summary` and `/insights/supporters` are open to an owner's phone like `/recap`.

### Ferrocast account (optional, 1.3)
The app works the same without an account. Signing in (Google or an email link, in the browser) ties Pro to the account, so it follows the streamer to up to 3 PCs, and lets the web dashboard (`https://ferrocast.clearcraftlabs.com/app/`) reach this PC without Pro. Relay contract: `docs/ACCOUNTS.md`. The routes live under `/pro`, so a paired phone can never sign the PC in or out (owner phones are denied `/pro/*`, §9.5) and `account` live frames never go to phones.

```ts
type AccountView =
  | { state: "signed_out"; error: AccountProblem | null }          // error: why (revoked elsewhere, the last sign-in failed or was cancelled)
  | { state: "pending"; started_at: number; expires_at: number | null; url: string | null }   // waiting for the browser (10 minutes)
  | { state: "signed_in"; stale: boolean; checked_at: number;   // stale: the relay couldn't be reached, this is the remembered answer
      account: { id: string; email: string; name: string | null; avatar_url: string | null; created_at: number; providers: ("google" | "email")[] } | null;
      plan: "free" | "pro";
      pro: { active: boolean; source: "subscription" | "grant" | null; plan: "monthly" | "yearly" | "comp" | null; status: string | null;
             subscription_id: string | null; current_end: number | null; access_until: number | null; renews: boolean; cancel_at_period_end: boolean };
      session: { id: string; kind: "app"; label: string; expires_at: number };
      pcs: number;                                               // the account's signed-in PCs
      error?: AccountProblem }                                  // only with stale: true
  | { state: "disabled"; error: AccountProblem }                 // the owner disabled the account (hint: write to support)
type AccountProblem = { message: string; hint: string }
type AccountSession = { id: string; kind: "app" | "web"; label: string; created_at: number; last_used_at: number; expires_at: number; current: boolean }
```
`account.pro` is the **account's** Pro. Whether this PC has Pro is still `GET /pro` (`ProView.active`): the relay counts the install's own subscription **or** its account's, and the engine re-reads it after every sign-in and sign-out. The session token (`fca_…`) stays in the OS keychain (the engine's secret store, like the install secret); it is never sent to the dashboard, written to settings or logged.

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/pro/account` | `?refresh=1` asks the relay now → `AccountView`. Never an error. Without `refresh` a relay answer from the last 15 minutes is reused; the engine also refreshes at start, every 4 hours, after sign-in, checkout, redeem and `POST /pro/refresh` |
| POST | `/api/v1/pro/account/sign-in` | `{ label? }` (this PC's name in the account's device list, ≤ 40 characters; default the relay's "Ferrocast app") → 201 `{ url, expires_at }`. **Open `url` in the browser** (`window.open(url)` / a `target="_blank"` link: the desktop window sends web pages to the system browser). The engine waits on `127.0.0.1:<random port>/auth/callback` for up to 10 minutes, then `account` frames say how it went. Show "Finish signing in in your browser…" with a Cancel button while `state` is `pending` |
| POST | `/api/v1/pro/account/sign-in/cancel` | → 204. Stops waiting (the browser tab, if finished later, shows an error). `state` goes back to `signed_out` |
| POST | `/api/v1/pro/account/sign-out` | `{ everywhere?: false }` → 204. This PC only, or every app and browser of the account. Forgets the session here even when the relay can't be reached (then only `everywhere: true` fails, 503) |
| GET | `/api/v1/pro/account/sessions` | → `{ sessions: AccountSession[] }` (active ones, newest use first) |
| DELETE | `/api/v1/pro/account/sessions/{id}` | → 204. Signs that app or browser out; revoking `current` signs this PC out too. An app session's PC loses the account's Pro at once |

**Checkout while signed in.** `POST /api/v1/pro/checkout` is unchanged and the relay attaches the new subscription to the account by itself (the install is linked to it). While signed in, `email` may be left out or empty: the account's email is used. Redeeming a code and checkout refresh the account too.

**Dashboard anywhere.** A PC signed in to any account (free too) may open the tunnel (§9.5): it opens when a phone is paired or a pairing link waits (as with Pro), and when the web dashboard asks for this PC (the relay's `remote_wanted` item on the relay link). Signed-in PCs keep the relay link open for that.

Errors (always `{ error: { code, message, hint } }`): 409 `already_signed_in` (sign out first to switch accounts), 401 `signed_out` (sessions, revoke: not signed in, or the session ended elsewhere; the PC is then signed out here too), 403 `account_disabled`, 404 `not_found` (no such session), 400 `invalid` (a session id that can't be one), 422 `invalid` (label too long), 429 `rate_limited` (`Retry-After`; 10 sign-ins an hour per PC), 410 `flow_expired`, 422 `invalid_challenge` / `invalid_redirect`, 502 `relay_error` (the relay answered something Ferrocast can't use), **503 `relay_unreachable`** (no internet or the relay is down; hint: check the connection, everything else keeps working), 503 `relay_unrecognised`, 503 `no_relay` (this build has no relay set up), 503 `keychain_unavailable` (the OS keychain refused; hint: unlock it and try again), 503 `listener_unavailable` (couldn't listen on 127.0.0.1 for the browser; hint: allow Ferrocast in the firewall). Problems that end a sign-in in the background (`state: "signed_out"` with `error`): the browser came back with another sign-in's `state` (that request is answered 400 and ignored; the real one keeps waiting), the person cancelled at Google, no code came, the code was refused (wrong or expired), 10 minutes passed.

### Cloud sync (1.4, optional)
With a signed-in account the app can keep its overlays, alert rules, own designs and the alert design in use in sync with **cloud mode** (`docs/CLOUD.md` §1.1), and show the cloud overlays' URLs so OBS keeps working with this PC off. Both switches are **off by default**; the app works fully without them. Under `/pro`, like the account, so a phone never reaches them.

```ts
type CloudSyncView = { signed_in: boolean; sync: boolean; use_cloud_overlays: boolean; last_sync_at: number | null;
                       error: { code, message, hint } | null /* the last sync that failed */; notes: string[] /* what the cloud replaced here, newest first */;
                       synced: number; cloud_overlays: number; running: boolean }
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/pro/cloud` | → `CloudSyncView` (never an error) |
| PUT | `/api/v1/pro/cloud` | `{ sync?, use_cloud_overlays? }` → `CloudSyncView`. Turning sync on turns cloud mode on for the account (free) and syncs at once; turning it off turns cloud overlays off too. 409 `signed_out` without the account; the relay's errors as they come (503 `relay_unreachable`) |
| POST | `/api/v1/pro/cloud/sync` | → `CloudSyncView` after a sync now. It also runs every 5 minutes, and 10 s after an overlay or alert rule changes here |

The cloud copy wins a conflict: this PC's overlay or design is kept as "… (this PC)", an alert rule's change is named in a `notice` (`key: "cloud-sync"`). With `use_cloud_overlays`, `Overlay.url` is the cloud URL for synced overlays (`https://<overlay origin>/o/<token>/`), so Add to OBS and Copy use it.

### Media
| Method | Path | Body → Response |
|---|---|---|
| POST | `/api/v1/media` | multipart `file` → `{ id, kind: "image" \| "sound" \| "video", name, bytes, url }`. Limits: images 10 MB (png, jpeg, gif, webp, svg not allowed), sounds 5 MB (mp3, ogg, wav), video 50 MB (webm, mp4). 413 / 415 with the limit in the message. |
| GET | `/api/v1/media` | → media list |
| DELETE | `/api/v1/media/{id}` | → 204 (409 if an alert still uses it, with which one) |

### Discord go-live posts
The streamer pastes a Discord webhook URL; Ferrocast posts when a platform goes live. The URL is a secret: it is kept in the secret store (R-45), never in the database, never in a `.ferropack`, never sent back.

```ts
type DiscordView = { has_webhook: boolean; webhook_hint: string | null /* "discord.com webhook …4821" */;
                     enabled: boolean; template: string /* 1–1,500 chars; {name}, {platform}, {title}, {url} */; mention_everyone: boolean
                     pro_go_live: boolean /* the Ferrocast bot (Pro, §9.6) posts go-live, so this webhook post is skipped */ }
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/discord` | → `DiscordView` |
| PUT | `/api/v1/discord` | `{ webhook_url?: string \| null, enabled?, template?, mention_everyone? }` → `DiscordView`. Only fields sent change; `null` removes the webhook. The URL must be `https://discord.com/api/webhooks/<id>/<token>` (or `discordapp.com`, or `/api/v10/…`), nothing else: 400 `webhook_url: …` |
| POST | `/api/v1/discord/test` | → 204 after posting a test message (never pings). 422 without a webhook; 422 "Discord says this webhook was deleted…" (Discord 404); 422 "Discord refused this webhook…" (401/403); 429 with `Retry-After` when Discord rate-limits; 422 when Discord can't be reached |

Defaults: on, `{name} is live on {platform}! {title|default:"Come hang out."} {url}`, no @everyone ping (`mention_everyone` lets the message ping). One post per platform per 30 minutes (a stream that drops and comes back isn't news). A failed go-live post becomes one `notice` with a link to the Discord settings.

### OBS
Browser sources Ferrocast makes (Add to OBS, a theme into OBS) have "Control audio via OBS" on (`reroute_audio: true`), so alert sounds, music screens and sound elements reach the stream on their own. A source that exists keeps its audio settings, and monitoring is never switched on (with Desktop Audio capturing the monitoring device, viewers would hear every sound twice): the dashboard says how to hear them yourself.

GET `/api/v1/obs` → `{ state, version: "31.1.2" | null, scene: string | null, host, port, has_password, auto, detail }`; PUT `/api/v1/obs` `{ host, port, password }` (password goes to the secret store; absent keeps it, `null` clears it).

The engine keeps OBS connected by itself: it checks a working connection every 5 s, retries a closed OBS every 3 s, and sends one `status` frame whenever the state changes. `auto: true` means Ferrocast follows OBS's own settings (below): each attempt re-reads them, so switching on OBS's WebSocket server (or changing its password) connects within seconds, no click needed, and the password is never copied out of OBS's file. The details become the streamer's own (`auto: false`) when they PUT `/obs` with anything different from what's in use; from then on detected values never replace them, until POST `/obs/detect`. Settings saved before `auto` existed count as the streamer's own only if the address or port differ from `127.0.0.1:4455`. With `auto` and no OBS settings on this computer, a failed attempt is `state: "off"` (nothing set up), not `disconnected`.

Auto-detect reads OBS's own WebSocket settings (`plugin_config/obs-websocket/config.json` in OBS's config folder: `~/Library/Application Support/obs-studio` on macOS, `%APPDATA%\obs-studio` on Windows, `$XDG_CONFIG_HOME` or `~/.config/obs-studio` and the Flatpak folder on Linux), so nobody types the port or password:
- GET `/api/v1/obs/detect` → `{ found, port: number | null, has_password, server_enabled, in_use, reason: string | null }`. `in_use`: Ferrocast already uses these (following OBS, or the typed details match), so there is nothing to offer. Never the password. A missing or damaged file is `found: false` with a plain `reason`; `server_enabled: false` comes with how to switch it on.
- POST `/api/v1/obs/detect` ("Use these") → the OBS view (as GET `/obs`) after switching back to following OBS's settings (`auto: true`; a typed password is forgotten). 422 with the reason when nothing usable was found.

### Music for the screens (25 Oct, free)
| Method | Path | |
|---|---|---|
| GET | `/api/v1/music` | `{ config: MusicConfig, now: MusicNow, beat_problem: string \| null, song_problem: string \| null }` (a problem is a plain sentence with what to do) |
| PUT | `/api/v1/music` | partial `MusicConfig` → as GET. 400 naming the field |

`MusicConfig = { beat_input: string | null /* an OBS audio source, e.g. "Desktop Audio" */, song: "off" | "player" | "file", file: string | null /* a .txt file's full path */ }`. `MusicNow = { playing: boolean, level: 0…1, title: string | null, artist: string | null }`.
- The beat: OBS's volume meters for `beat_input` (a connection of its own that asks for `InputVolumeMeters`), -50 dB as 0 and 0 dB as 1.
- The song, every 3 s (5 s on Windows): `player` asks Spotify, then Music (macOS; neither is opened, and macOS asks once to allow it), the system's media controls (Windows) or playerctl (Linux); `file` reads the first line as the song and the second as the artist (never split on dashes), up to 4 KB.
- `playing`: the player says it plays, or OBS heard `beat_input` louder than -46 dB in the last 2.5 s; with a file and no `beat_input`, a song in the file. `level` is 0 while not playing.

### Studio engine (2.1 preview, Track B: stream without OBS)
OBS's own libobs and plugins, run by the **desktop app** on its main thread (STUDIO-PARITY.md, `crates/studio`). For the preview it uses the OBS installed on the computer (32 or newer). The headless server and browsers have no engine: every route but GET answers 422 `studio_unavailable`. Phones (Dashboard anywhere) are refused all of them.

| Method | Path | |
|---|---|---|
| GET | `/api/v1/studio/engine` | `{ available, running, version: string \| null, renderer: "opengl" \| "metal" \| null, detail: string \| null }` |
| POST | `/api/v1/studio/engine/start` | Loads the engine (once; it stays loaded until Ferrocast quits, as libobs can't restart in one process) and puts a scene on air → status. 422 `studio_failed` with the reason |
| POST | `/api/v1/studio/engine/stop` | Takes the scene and the preview away → status |
| GET | `/api/v1/studio/engine/kinds` | `{ inputs, filters, transitions, encoders, outputs: { id, name }[] }` from the loaded plugins. 409 `studio_stopped` |
| GET | `/api/v1/studio/engine/kinds/{id}/options` | `{ key, label, kind, help, visible, enabled, min, max, step, slider, suffix, choices, values /* each choice's saved value */, group, checkable /* a group with its own switch, its key true or false */, path /* file, save, folder, or files for a list of files */, filter /* OBS's file types */, text_type /* a text's kind: multiline (a box), password, info (read only) or "" */, monospace /* a text in a fixed-width font */ }[]`, read from a live private source (some plugins can't describe themselves without one). 400 for a bad id, 409 `studio_stopped` |
| PUT | `/api/v1/studio/engine/preview` | `{ rect: { x, y, w, h, vh? /* the page's visible height, window.innerHeight */, canvas? /* { x, y, w, h }: where the canvas is inside the box */, bg? /* 0xRRGGBB: the page's colour round the canvas */, holes? /* up to 16 { x, y, w, h, r? /* corner radius, 0…200 */ } from the box's top left: where the page's menus and panels lie over it */, fit? /* { pad, zoom }: with canvas, how the page fits it (the largest box of its shape inside pad, times zoom, centred) */, opacity? /* 0…1, missing 1 */, shade? /* 0…1: a dark layer the app draws over the picture, as a dialog's backdrop dims the page */ } \| null }` (CSS pixels in the desktop window) → 204. The app draws libobs's picture into a native view over that box; null takes it away. The picture is left out of `holes`, so a menu or panel over the preview shows there while the rest keeps playing. Without `canvas` the picture fills the box and the view reaches 10 px past it on every side, for the handles of a source at the canvas's edge. With it the box is the whole stage and the canvas sits at `canvas` (zoomed out, the outlines of what's off the canvas show round it). With `fit` the view keeps its distance from the window's edges as the window is resized and fits the canvas by that rule itself, so the picture follows a resize at once; the page sends the exact box when it settles. Holes keep their rounded corners, and overlapping holes stay holes. 400 for a box off the window or 0 / over 16384 px |
| GET | `/api/v1/studio/engine/kinds/{id}/defaults` | The settings a new source of that kind starts with (OBS's defaults). 400 for a bad id, 409 `studio_stopped` |
| GET / POST | `/api/v1/studio/engine/stream` | GET → `{ active, reconnecting, bytes, frames, dropped, error: string \| null } \| null` (null: not streaming). POST `{ on }` starts or stops streaming the Studio into this PC's Go live ingest (H.264 6000 kbps CBR, 2 s keyframes, AAC 160; the Mac's hardware encoder when there is one) → the same stats, or null when stopped. Go live then sends it on to the destinations switched on, as it does a stream from OBS. 422 `golive_off` when the local ingest didn't start, 409 `studio_stopped` |
| GET / POST | `/api/v1/studio/engine/scenes` | `{ scenes: { name, items, current }[] /* in order */, current, transition: { kind, ms } }`. POST `{ name }` adds an empty scene at the end → 201. Names are unique across scenes and sources (as in OBS): 409 `studio_name_taken` |
| DELETE | `/api/v1/studio/engine/scenes/{scene}` | → 204. If it was on air, the one before it goes on. 409 `studio_last_scene` for the only scene, 404 `studio_no_scene` |
| POST | `/api/v1/studio/engine/scenes/{scene}/duplicate` | `{ name }` → 201: a copy right after it, showing the same sources |
| POST | `/api/v1/studio/engine/scenes/{scene}/move` | `{ to }` (0 is the top) → 204 |
| POST | `/api/v1/studio/engine/scenes/{scene}/current` | Puts the scene on air through the transition → 204 |
| PUT | `/api/v1/studio/engine/transition` | `{ kind /* fade_transition, cut_transition, swipe_transition… */, ms /* 0–20000 */ }` → 204. 422 `studio_failed` for a kind OBS doesn't have |
| GET, PUT | `/api/v1/studio/engine/transitions/{kind}/settings` | A transition kind's own settings (a stinger's video and transition point, a swipe's direction, a colour, a luma image): GET → what was set (OBS fills in the rest; read the form from `/kinds/{kind}/options`), PUT `{ settings }` changes only the keys sent and the transition on air takes them at once. Saved per kind in the collection as OBS saves them (`transitions`), so OBS collections bring their stingers in. Kinds: fade, cut, swipe, slide, fade_to_color, wipe and `obs_stinger_transition` (also allowed as a scene's override). 400 for any other kind or settings that aren't an object |
| GET / POST | `/api/v1/studio/engine/scenes/{scene}/items` | The scene's sources, top first: `{ id, name, kind, visible, locked, audio, group, children /* a group's own items */ }[]`. POST `{ kind, name /* 1–100, one line */, settings?: object /* OBS's own keys, ≤ 256 KB */, fit?: boolean }` → 201 `{ item, settings }` (settings as OBS filled them in); or `{ existing: name }` adds a source from another scene → 201 `{ item }`. 409 `studio_name_taken`, 422 `studio_failed` for a kind OBS can't make |
| POST | `/api/v1/studio/engine/scenes/{scene}/items/{id}/edit` | `{ do: "show" \| "hide" \| "lock" \| "unlock" \| "fit" \| "remove" \| "up" \| "down" \| "top" \| "bottom" \| "stretch" \| "center" \| "reset" \| "flip_h" \| "flip_v" \| "rotate_cw" \| "rotate_ccw" \| "rotate180" \| "duplicate" \| "blend_normal" \| "blend_additive" \| "blend_subtract" \| "blend_screen" \| "blend_multiply" \| "blend_lighten" \| "blend_darken" \| "scale_off" \| "scale_point" \| "scale_bicubic" \| "scale_bilinear" \| "scale_lanczos" \| "scale_area" }` → 204 (fit scales it into the canvas, keeping its shape; stretch fills it; reset is its own size at the top left; flips and turns happen in place, keeping the box's middle (an item without a bounding box is anchored at its top left first, as OBS's own flips leave it, since libobs draws a flipped item anchored elsewhere off its box); center moves the box's middle to the canvas's; duplicate copies the source, settings and filters, just above it; remove takes it out of this scene). Items carry `blend` and `scale_filter`. 404 `studio_no_source` for an unknown id |
| PUT | `/api/v1/studio/engine/scenes/{scene}/items/{id}/order` | `{ to }` → 204: moves the source to that place among its neighbours (the scene's sources, or its group's), counted from the top as the list shows them: 0 is on top of everything (OBS's drag in its Sources list). A place past the end is the bottom. One undo step. 400 for `to` over 10000, 404 `studio_no_source` |
| PUT | `/api/v1/studio/engine/scenes/{scene}/items/{id}/box` | `{ x, y, w, h, save? }` (canvas pixels) → the item: moved and sized from the preview (a source inside a group too: its `frame` and this box are always canvas pixels). Its box's first corner goes to (x, y) and its sides become w and h long, keeping its turn and flips; an item with a bounding box (Fit, Stretch) changes that box, any other its scale. `save: true` writes the scenes file (the end of a drag; moves during it don't). Every item carries `frame: [sideX.x, sideX.y, sideY.x, sideY.y, corner.x, corner.y]`, its box on the canvas (a point (u, v) of the box, each 0 to 1, is at corner + u·sideX + v·sideY), worked out as libobs does it. 400 for w or h under 1 or numbers past ±100000 |
| PUT | `/api/v1/studio/engine/scenes/{scene}/items/{id}/crop` | `{ left, top, right, bottom, save? }` (the source's own pixels) → the item: cropped from the preview (OBS's Alt-drag on a handle). What's left of the picture stays where it is on screen. `save: true` ends a drag (one undo step). 400 for a negative crop; 422 for one that leaves no picture |
| GET, PUT | `/api/v1/studio/engine/scenes/{scene}/items/{id}/transform` | OBS's Edit Transform: `{ x, y, rotation, scale_x, scale_y, alignment, bounds, bounds_alignment, bounds_width, bounds_height, crop_left, crop_top, crop_right, crop_bottom, width, height }`. Alignments are OBS's bits (0 middle; 1 left, 2 right, 4 top, 8 bottom); `bounds` is none, stretch, fit, fill, width, height or max; `width` and `height` are the source's own size (read only). PUT sets all of it → the item. 400 for scales under 0.0001 or over 1000, a point that's both left and right, an unknown bounds kind, negative crops or numbers past ±100000 |
| PUT | `/api/v1/studio/engine/preview/selection` | `{ scene, id, frames?, also? }` (both null for none) → 204: the preview outlines that item with eight handles (and every id in `also`, picked with it: ⌘-click and Shift-click, as OBS's multi-select) (in the app's native preview; kept when the preview is made again). With `frames: true` every showing source of `scene` gets a thin outline too (`id` may then be null). Each source's outline has its own colour, by `id % 8`: #ff5a1f, #33d4ff, #a3e635, #f472b6, #facc15, #a78bfa, #2dd4bf, #60a5fa |
| PUT, PUT, POST, PUT | `/api/v1/studio/engine/studio-mode` `{ on }`, `/studio-mode/preview` `{ scene }`, `/studio-mode/transition` `{ quick? }`, `/quick-transitions` `{ list }` | OBS's Studio Mode: a preview scene to edit before it goes on air. On, the preview starts as what's on air and `/scenes` carries `preview` (null when off) and each scene's `preview`; the panels edit the preview scene. Transition sends it on air the way a click on it would (its own transition, or the Studio's), or with a quick transition `{ kind, ms }`. Quick transitions (up to 10, default Cut and Fade 300 ms) are in `/scenes` as `quick` and saved with the collection, as is Studio Mode and its preview. `PUT /preview` takes `program` too: a second box where the app shows what's on air. 400 for a quick transition that isn't one of the Studio's or over 20 s; 422 "Studio Mode is off" for a preview or transition while it's off |
| PUT, POST, PUT, POST | `/api/v1/studio/engine/studio-mode/options` `{ swap, duplicate_scene, duplicate_sources, double_click }`, `/studio-mode/swap`, `/studio-mode/tbar` `{ at, release? }`, `/black` | OBS's Studio Mode extras. Options (OBS's defaults: swap and duplicate scene on): after a transition the preview shows what was on air; what goes on air is a private copy of the scene (with `duplicate_sources`, of its sources too) so editing the preview never touches the stream; a double-click on a scene sends it on air (the page does this). Changing the copying puts what's on air again, never mid-transition (422). Swap trades preview and on air with a cut. The T-bar: `at` 0 (on air) to 1 (preview) → `{ active, at }`; the first move starts a hand-driven transition (a cut or stinger is driven as a fade), `release` near the end (≥ 0.9) finishes it, near the start (≤ 0.1) goes back, anywhere else holds both scenes mixed. Transition, swap and scene removal wait while it's held (422); a switch made elsewhere ends it. `/black` → `{ black }` fades what's on air to black, or back. `/scenes` carries `studio_options`, `tbar` and `black`. In Studio Mode a second click on Transition while one runs does nothing, as in OBS |
| POST, GET | `/api/v1/studio/engine/undo`, `/redo`, `/history` | Undo and redo → `{ done, undo, redo }` (labels like "Move Camera", or null), as OBS's Edit menu: sources in scenes (moves and resizes, Edit Transform, show/hide, lock, order, fit, flips, turns, blending, scaling, show/hide transitions, adding, duplicating, removing), scenes (add, copy, remove with everything in it, reorder, rename, own transition), every filter change, a source's settings, its sound (volume, mute, monitoring, balance, sync, mono, tracks) and deinterlacing. A drag's moves, arrow nudges within a second, and slider changes (Edit Transform, volume, balance, sync, filter and source settings) within 3 s are one step; mute and push-to-talk shortcuts aren't steps. 100 steps are kept each way, forgotten when the Studio stops or the collection changes. A removed source held for undo frees its name (a new source can take it; then it can't come back under it, 422) and isn't saved. `GET /history` → `{ undo: [labels], redo: [labels] }`, newest first. Stats carry `history: { undo, redo }`. 422 `studio_failed` when a step can't be done any more (its scene was removed) |
| GET | `/api/v1/studio/engine/scenes/{scene}/audio` | The scene's mixer: the global devices first, then its own sounds (groups included): `{ name, kind, channel: 1–6 \| null, volume_db /* -100–26 */, muted, monitor: "off" \| "only_me" \| "me_and_stream" }[]` |
| GET | `/api/v1/studio/engine/sources` | Every source in a scene: `{ name, kind, scenes: string[] }[]` (for adding one again) |
| POST | `/api/v1/studio/engine/sources/{name}/rename` | `{ name }` → 204 (scenes too). 409 `studio_name_taken` |
| GET / PUT | `/api/v1/studio/engine/sources/{name}/settings` | The source's settings object; PUT `{ settings: object }` changes only the keys sent → 204. 404 `studio_no_source` |
| GET | `/api/v1/studio/engine/sources/{name}/options` | The source's options as they look for its settings now (OBS shows, hides and fills some from what's chosen). 404 `studio_no_source` |
| PUT | `/api/v1/studio/engine/sources/{name}/audio` | `{ volume_db?, muted?, monitor?, balance?, sync_ms?, mono?, tracks? }` (any of them; OBS's Advanced Audio Properties) → 204. Balance 0 (left) to 1 (right); sync -950 to 20000 ms; tracks a list of 1–6. The mixer's sounds carry all of them. 400 outside -100–26 dB or these ranges |
| PUT, PUT | `/api/v1/studio/engine/scenes/{scene}/items/{id}/transition` `{ show, transition }`, `/sources/{name}/deinterlace` `{ mode, top_field_first? }` | An item's show (or hide) transition `{ kind, ms }` (null: at once), using that kind's Studio settings; items carry `show_transition` and `hide_transition`. Deinterlacing for video from a device or file: `off`, `discard`, `retro`, `blend`, `blend_2x`, `linear`, `linear_2x`, `yadif`, `yadif_2x`; items carry `deinterlace` (null for other sources) and `top_field_first`. 400 for an unknown mode; 422 for a source that isn't such video. Both saved with the collection |
| POST, PUT, POST | `/api/v1/studio/engine/scenes/{scene}/items` `{ group: name }` or `{ existing, copy: true }`, `…/items/{id}/group` `{ group }`, `…/items/{id}/ungroup` | Groups as OBS's: a new empty group on top (→ the item), or with `items: [ids]` a group holding those sources of the scene, each where it is on screen (Group Selected Items; one undo step that ungroups), an item moved into a group of the scene or out (`group` null; a group can't go in a group), a group taken apart (its sources stay). `copy: true` pastes a new, independent copy of a source ("Camera 2"; OBS's Paste (Duplicate)) → the item. Adding a group and pasting a copy are undo steps |
| GET / POST | `/api/v1/studio/engine/sources/{name}/filters` | `{ name, kind, enabled }[]` in the order they run. POST `{ kind, name, settings? }` adds one at the end → 201. 409 `studio_name_taken` for a filter name used on this source |
| DELETE | `/api/v1/studio/engine/sources/{name}/filters/{filter}` | → 204. 404 `studio_no_filter` |
| POST | `/api/v1/studio/engine/sources/{name}/filters/{filter}/edit` | `{ do: "enable" \| "disable" \| "up" \| "down" \| "top" \| "bottom" }` → 204 |
| GET / PUT | `/api/v1/studio/engine/sources/{name}/filters/{filter}/settings` | As a source's settings |
| GET | `/api/v1/studio/engine/sources/{name}/filters/{filter}/options` | As a source's options |
| PUT / DELETE | `/api/v1/studio/engine/channels/{channel}` | A global sound device heard in every scene: 1 and 2 desktop sound, 3 to 6 mics. PUT `{ kind, name, settings? }` → 204 (replaces what was there); DELETE takes it off → 204. 400 for a channel outside 1–6 |
| POST | `/api/v1/studio/engine/levels` | `{ names: string[] /* ≤ 64 */ }` → `{ [name]: { peak: [l, r], magnitude: [l, r] } }` in dBFS (-100 is silence). A meter starts on the first ask, so the first answer can be silent; poll it (a few times a second) while the mixer shows. |
| GET / PUT | `/api/v1/studio/engine/settings` | `{ encoder: "hardware" \| "software", video_kbps /* 500–60000 */, vbr, keyframe_s /* 0–10, 0 auto */, profile: "high" \| "main" \| "baseline", bframes, scale: [w, h] \| null, audio_kbps /* 64–320 AAC steps */, width, height, fps /* the canvas */, record_folder: string \| null /* absolute; null is Movies/Ferrocast */, record_format: "mp4" \| "mov" \| "fmp4" \| "fmov" \| "mkv" \| "flv" \| "ts" \| "hls", record_muxer /* ffmpeg key=value settings */, record_quality: "stream" \| "high" \| "lossless" \| "custom", reconnect, reconnect_every_s, reconnect_tries, delay_s /* 0–1800 */, sample_rate: 44100 \| 48000, stereo, monitoring: [name, id] \| null, hotkeys: { [action]: keys }, video_encoder: string \| null /* a particular encoder id (HEVC, AV1…) */, encoder_settings: {} /* that encoder's own options, OBS keys, over the ones above */, record_encoder, record_encoder_settings /* for record_quality "custom" */, record_name /* OBS's pattern, %CCYY-%MM-%DD %hh-%mm-%ss… */, record_spaces, record_overwrite, record_split_minutes /* 0–1440 */, record_split_mb, record_tracks /* 1–6 */, record_remux /* MKV, FLV, TS become MP4 when it ends */, replay_seconds, replay_mb, replay_format, speakers: "2.1" \| "4.0" \| "4.1" \| "5.1" \| "7.1" \| null /* surround, after a restart */, true_peak /* meters read true peak */, renderer: "opengl" \| "metal" /* how a Mac draws the picture; Metal resizes smoothly; after a restart */, multiview_layout: "top" \| "bottom" \| "left" \| "right" \| "scenes_only", multiview_labels, multiview_safe_areas, snapping, snap_px /* 1–50 screen pixels */, snap_screen, snap_sources, snap_center /* preview snapping, OBS's defaults: on, 10, edges only */, confirm_go_live, confirm_end_stream, confirm_stop_record /* the dashboard asks first; ending a stream by default */, record_when_streaming, keep_recording, replay_when_streaming, keep_replay /* all off by default: recording and the replay buffer start with the stream and stop with it unless kept */ }` (missing fields take their defaults). PUT the whole object → `{ settings, restart: string \| null }`. POST `/studio/engine/stream` may add `also` (in words: what was set to start or stop with the stream but didn't) (what applies only after a restart, or didn't apply, in words). Actions (OBS's hotkeys): go_live, end_stream, record, pause_record, split_record, chapter, replay, save_replay, virtualcam, studio_mode, transition, swap_scenes, fade_to_black, screenshot (what's on air), undo, redo, quick:<1–10>, scene:<name>, screenshot:<name>, mute:<sound>, push_to_talk:<sound>, push_to_mute:<sound>, media_play_pause / media_restart / media_stop / media_next / media_previous:<name>, show:<scene>\n<source>, hide:<scene>\n<source>, filter:<source>\n<filter> (on or off); keys are named keys joined with + (`Alt+Shift+KeyR`, `Cmd+F9`). 400 for a field outside its range; 422 `studio_failed` for a canvas change while live or recording |
| GET | `/api/v1/studio/engine/stats` | `{ engine: { cpu /* % of one CPU */, fps, frame_ms, lagged, frames, memory_mb }, canvas: { width, height, fps }, stream: stream stats \| null, record: { active, paused, bytes, frames, file, seconds, error } \| null }`. 409 `studio_stopped`. Also `scene`: the scene on air now, so a page notices switches made elsewhere (shortcuts, a multiview click). |
| POST | `/api/v1/studio/engine/record` | `{ on }` → the recording's stats (with `file`), or null when stopped (it waits for the file to be finished). Files go to the recordings folder named by `record_name` (" (2)"… unless `record_overwrite`), with the audio tracks in `record_tracks`, a new file every `record_split_minutes` / `record_split_mb`. MP4 and MOV are OBS's crash-safe hybrid writers; fragmented MP4/MOV, MKV, FLV and TS use OBS's own mux helper through the `obs-ffmpeg-mux` launcher beside Ferrocast (OBS must stay installed). With `record_remux`, every part of an MKV, FLV or TS recording becomes an MP4 beside it when it ends (stats carry `remux`) |
| POST, POST | `/api/v1/studio/engine/record/split`, `/record/chapter` `{ name? }` | → 204. Split: the recording goes on in a new file. Chapter: a marker at this frame (MP4, MOV; an empty name is numbered). 422 when nothing is recording, or for a chapter in another format |
| POST, POST, POST | `/api/v1/studio/engine/replay` `{ on }`, `/replay/save`, `/virtualcam` `{ on, source? }` | OBS's replay buffer: on, it keeps the last `replay_seconds` (`replay_mb` at most) with the recording's encoding and tracks → `{ active }`; save writes them to the recordings folder as `Replay <record_name>.<replay_format>` → `{ file }` (null until written; stats carry `replay: { active, last, error }`). Needs the `obs-ffmpeg-mux` launcher. The virtual camera shows what's on air, or one scene or source (`source`) → `{ active }`; stats carry `virtualcam`. On macOS it needs OBS's camera extension, which macOS lets only OBS itself install, so it may refuse with OBS's reason (422); on Windows it uses OBS's registered camera. Shortcut actions `replay`, `save_replay`, `virtualcam` |
| GET, PUT, POST | `/api/v1/studio/engine/missing-files`, `PUT` `{ source, parent?, kind, key, path, to }`, `/missing-files/search` `{ folder }` | OBS's Missing Files: GET → `[{ source, parent, kind, key, label, path }]`, every whole path a source, filter (with `parent`) or transition kind (`source` "") uses that isn't on this computer (file and folder options, each file of a list). PUT points it at `to` (a whole path) or clears it (`to` ""; a list loses the item); an undo step. Search → `{ found: { path: new path } }`, files found by name under the folder (8 levels, hidden folders skipped) |
| GET, POST | `/api/v1/studio/engine/sources/{name}/media` `{ do?: play \| pause \| play_pause \| restart \| stop \| next \| previous, seek_ms? }` | OBS's media controls for a video, playlist or slideshow (items carry `media: true`) → `{ state: none \| playing \| opening \| buffering \| paused \| stopped \| ended \| error, time_ms, duration_ms }`; GET only reads. 422 for another kind of source |
| POST | `/api/v1/studio/engine/remux` `{ file }` | → `{ to }`: OBS's Remux, an MP4 made beside an absolute MKV, FLV, TS, MOV or MP4 path without encoding again, in the background; stats carry `remux: { from, to, percent, done, error }`. 400 for another path; 422 while another remux runs or for a file of the recording under way |
| POST | `/api/v1/studio/engine/encoders/{id}/options` `{ settings }` | → `{ options, values }`: an encoder's own options (OBS's Advanced output: rate control, preset, tune, x264 options, lookahead…) as they look for `settings`, and the values with its defaults. For `encoder_settings` |
| POST | `/api/v1/studio/engine/record/pause` | `{ on }` → 204. 422 `studio_failed` when nothing is recording |
| GET | `/api/v1/studio/engine/monitoring` | `{ devices: { name, id }[], current: { name, id } }`: where "Hear it" can play |
| PUT | `/api/v1/studio/engine/scenes/{scene}/transition` | `{ transition: { kind, ms } \| null }` → 204: the scene's own way in (OBS's Transition Override, saved as OBS saves it); null uses the Studio's. Scenes list it as `transition` |
| POST | `/api/v1/studio/engine/sources/{name}/screenshot` | → `{ file }`: a PNG of a scene (canvas size) or a source (its own size), saved where recordings go |
| POST | `/api/v1/studio/engine/sources/{name}/paste-filters` | `{ from }` → 204: copies every filter of `from` onto `name` (OBS's Copy / Paste Filters). 400 when `from` is `name` |
| POST | `/api/v1/studio/engine/sources/{name}/web` | `{ do: "interact" \| "stop" \| "reload" }` → 204. A Web page source: interact opens it in a window to click and type in (closing the window puts it back); reload loads it again (also OBS's own Browser source). 400 `studio_not_web_page`; 422 `studio_not_supported` for interact on OBS's Browser source |
| GET | `/api/v1/studio/engine/obs-collections` | OBS's scene collections on this computer: `{ id, name, scenes, sources }[]` (empty when OBS has never run) |
| POST | `/api/v1/studio/engine/obs-collections/import` | `{ id }` → `{ id, name, scenes, sources, web_pages, missing: string[] }`: that OBS collection as a new scene collection (named after it, made unique), put in use; the Studio's own collections stay as they are and OBS's files aren't changed. On macOS Browser sources become Web page sources (`web_pages` counts them); `missing` lists kinds this OBS lacks. 400 for an id that isn't a plain file name, 404 `studio_no_obs_collection` |
| GET | `/api/v1/studio/engine/streamlabs-collections` | → `[{ id, name, scenes, sources }]`: Streamlabs Desktop's scene collections on this computer (`slobs-client/SceneCollections`, named from its `manifest.json`); `[]` without Streamlabs |
| POST | `/api/v1/studio/engine/streamlabs-collections/import` | `{ id }` → the same as importing OBS's: Streamlabs' collection turned into OBS's format as OBS's own importer does (sources, scenes and their items, filters, transitions), then added as a new scene collection. Its widgets (Browser sources) become Web page sources on macOS; Windows cameras, captures and sound devices come in as this computer's kinds, empty, to be picked again. 400 for an id that isn't a plain file name, 404 `studio_no_obs_collection` |
| GET | `/api/v1/studio/engine/log?lines=` | `lines` 1–5000 (default 500) → `{ file, lines: string[] }`: the app's log, newest last (OBS's engine writes into it too), OBS's Help › Log Files. Stream keys, tokens, passwords and key-like parts of addresses are replaced by `•••` before they leave the engine, so the text is safe to paste into a bug report. Nothing is uploaded anywhere. 400 for `lines` out of range; 422 `studio_not_here` without the app |
| POST | `/api/v1/studio/engine/log/show` | → 204: the log file shown in Finder or Explorer. 422 `studio_not_here` without the app |
| POST | `/api/v1/studio/engine/web-window` | `{ name /* 1–100 */, url /* http(s) only */ }` → `{ ok: true }`: a web panel's page in a window of its own in the app, for sites that won't be shown inside another page (the same address again brings its window forward; the window remembers where it was). It's a plain page: no Ferrocast key, no app commands; links that open new windows go to the browser. 400 for anything but a web address; 422 `studio_unavailable` with no Studio, `studio_not_here` where the Studio has no windows to open |
| POST | `/api/v1/studio/engine/pick-file` | `{ kind: "file" \| "files" \| "save" \| "folder", filter? }` → `{ path }`: the computer's own file dialog in the app (filter as OBS writes it, `Image Files (*.png *.jpg);;All Files (*.*)`). `path` is a string, a list for `files`, or null when cancelled. Options of kind `path` carry `path` (file, save, folder) and `filter`; an `editable_list` carries `path: "files"` when it holds files. 422 `studio_not_here` where there's no app to show it |
| GET | `/api/v1/studio/engine/screen-access` | `{ granted }`: macOS's Screen Recording switch for Ferrocast, as it was when Ferrocast opened (macOS applies a change only after a restart); true on other systems |
| POST | `/api/v1/studio/engine/screen-access/fix` | → `{ granted }`: forgets macOS's answer for Ferrocast (`tccutil reset ScreenCapture`) and asks again, which lists this Ferrocast in System Settings. Needed after an update: unsigned builds are new apps to macOS, and System Settings can show an older one switched on |
| POST | `/api/v1/studio/engine/restart` | → 202: Ferrocast quits and opens again a moment later. 409 `studio_busy` while the Studio streams or records |
| POST | `/api/v1/studio/engine/projectors` | `{ what: "program" \| "source" \| "multiview", name?, screen? }` → 201 `{ id }`: OBS's projectors in the app's own windows. `program` is what's on air, `source` one scene or source by name, `multiview` every scene below what's on air (the one on air framed red; a click puts a scene on air through its transition, and the multiview follows scenes as they change). With `screen` (an index from `/screens`) it's full screen on that display, over the menu bar (Esc closes it); without, a resizable window. Projectors close when the Studio stops or the canvas size changes. 400 for a name with program or multiview, or none with source; 404 `studio_no_source`, 404 `studio_no_screen` for a display that's gone, 422 `studio_not_here` outside the app |
| DELETE | `/api/v1/studio/engine/projectors` | → 204: closes every projector |
| GET | `/api/v1/studio/engine/screens` | The displays: `{ index, name, width, height, main }[]` (points; `[]` outside the app) |
| GET, POST | `/api/v1/studio/engine/library/{kind}` | Scene collections (`kind` collections) or profiles (`profiles`), as OBS has them: GET → `{ current, list: { id, name }[] }`; POST `{ name, copy? }` → 201 `{ id }`: a new one (empty, or a copy of the one in use) put in use. A collection holds the scenes, a profile the Studio settings (stream, picture, sound, recording, shortcuts). Each is a file in `studio/collections/` or `studio/profiles/`; the single `scenes.json` and `settings.json` of earlier versions became "Untitled". 400 for a name over 100 characters, 409 `studio_name_taken` |
| PUT | `/api/v1/studio/engine/library/{kind}/current` | `{ id }` → `{ id }`: puts it in use. A running Studio swaps its scenes at once (undo history and projectors follow); a profile's settings apply as Settings applies them, refused while streaming or recording (409 `studio_busy`). 404 `studio_no_such` |
| POST, DELETE, GET | `/api/v1/studio/engine/library/{kind}/{id}/rename` `{ name }`, `/library/{kind}/{id}`, `/library/{kind}/{id}/export` | Rename (409 `studio_name_taken`); remove (the file is kept aside, dated; 409 `studio_in_use` for the one in use); export → the collection as OBS writes it (OBS can import it) or the profile's settings |
| POST | `/api/v1/studio/engine/library/collections/import` | `{ name, doc }` (a collection file from OBS or Ferrocast) → 201 `{ id, name, scenes, sources, web_pages, missing }`: a new collection, put in use (Browser sources become Web page sources on macOS). `POST /obs-collections/import` now does the same with OBS's own file instead of replacing the scenes. 400 for a doc without a sources list |

On macOS the inputs include `ferrocast_web`, Ferrocast's own Web page source (the system's WebKit, off-screen). Its settings use OBS browser source's keys: `url`, `width`, `height`, `css`, `fps`. Point it at an overlay's `/o/…` URL to put alerts in the Studio picture. What the page plays goes into the Studio's mix (it shows in the mixer, like OBS's "Control audio via OBS") instead of the speakers.

The scenes are saved after every change, in OBS's own scene-collection format (`scene_order`, `current_scene`, `sources`, `DesktopAudioDevice1`…), in the app's data folder (`studio/scenes.json`). The sources and their filters, volume and monitoring come back at the next Start.

### Studio (streamer tools, all free)
Production helpers that run on the streamer's PC. OBS requests use the OBS details above; when OBS can't be reached every OBS route answers 422 `obs_unreachable` (`obs_password`, or `obs_refused` for an unknown scene/source) with a hint. Settings PUTs save only the fields sent; 400 names the field.

**Chatbot: counters, quotes, modules.** A custom command with the same name always wins, then counters, then modules. Only live chat in this channel gets answers (R-11); viewers have a 10 s per-person and 3 s per-command cooldown, mods none.

```ts
type Counter = { name: string /* a-z 0-9 _, 1–30 */; value: number /* ±1,000,000,000 */; command: string | null /* "!deaths" */;
                 reply: string /* template: {count}, {counter}, {user}; 1–300 */; enabled: boolean }
type Quote = { id: number; text: string /* 1–400 */; added_by: string; at: number }
type ModulesConfig = { eight_ball: { enabled }; dice: { enabled }; followage: { enabled }; watchtime: { enabled }; uptime: { enabled };
                       quotes: { enabled }; roulette: { enabled /* off by default */; min_bet; max_bet; win_percent /* 1–99 */ };
                       duel: { enabled /* off */; max_bet; accept_s /* 10–300 */ }; game_title: { enabled; mods_can_change };
                       shoutout: { enabled; template /* {target} {target_game_note} {url} */ } }
```
| Method | Path | Body → Response |
|---|---|---|
| GET / POST | `/api/v1/counters` | → `Counter[]`; POST `{ name /* "Boss Deaths" becomes boss_deaths */, command?: string \| null, reply?, enabled?, value? }` → 201 `Counter`. 409 when the name or command is taken (another counter, a custom command, or a built-in like `!quote`), 400 for a bad name/command/reply |
| PUT / DELETE | `/api/v1/counters/{name}` | `{ command?, reply?, enabled?, value? }` → `Counter`; DELETE → 204. 404 `counter_not_found` |
| POST | `/api/v1/counters/{name}/change` | `{ op: "add" \| "set" \| "reset", amount? }` → `Counter` (add defaults to 1, may be negative) |
| GET / POST | `/api/v1/quotes` | → `Quote[]` oldest first; POST `{ text }` → 201 (`added_by: "Dashboard"`); 409 at 5,000 quotes |
| PUT / DELETE | `/api/v1/quotes/{id}` | `{ text }` → `Quote`; → 204. 404 `quote_not_found` |
| GET / PUT | `/api/v1/modules` | `ModulesConfig`; PUT one module at a time (`{ "dice": { "enabled": false } }`); 400 `roulette.win_percent: …` |

Chat: `!deaths` shows the count (everyone); mods `!deaths +`, `+5`, `-`, `set 10`, `reset` (a wrong number gets the usage). `{count:deaths}` works in command replies, timers and overlay `label` templates (labels update live through a `state { labels }` frame). `!quote`, `!quote 12`, mods `!quote add <text>` / `!quote remove 12`. `!8ball`, `!roll 2d6` (1–10 dice, 2–1000 sides), `!roulette 100|half|all|10%` and `!duel @name 100` + `!accept`/`!deny` (loyalty points; never creates points; OS randomness), `!followage [@name]` (Twitch's follow date, else Ferrocast's log), `!watchtime` (minutes counted while live, every 5 minutes for people who chatted), `!uptime`, `!title` / `!game` (show; mods with text change them on every connected platform), `!so @name` (mods). Banned words are the Moderation filter's word list (`/mod/filters`), not a separate list.

**Stream info.** One form for Twitch, YouTube and Kick; each platform answers for itself.

```ts
type StreamInfo = { title?: string /* 1–140 (YouTube: 100) */; category?: { name: string; ids?: { twitch?, youtube?, kick? } };
                    tags?: string[] /* ≤10, letters and digits, ≤25 each; [] clears */; language?: string /* "en" */;
                    description?: string /* YouTube only; ≤5000, line breaks kept, no < or >; "" clears */;
                    thumbnail?: string /* a media library id; YouTube only; JPEG or PNG ≤2 MB */;
                    per?: { twitch?: StreamInfo; youtube?: StreamInfo; kick?: StreamInfo } /* a platform's own fields (no nested per) */ }
type PlatformResult = { platform; ok: boolean; applied: string[]; skipped: string[] /* "language: Kick has no stream language setting" */;
                        error: { code: "not_connected" | "reconnect_needed" | "rate_limited" | "refused" | "unreachable" | "no_broadcast"
                                       | "unknown_category" | "not_supported"; message; hint } | null }
type StreamInfoView = { last: HistoryEntry | null; presets: Preset[]; history: HistoryEntry[] /* newest first, 20 */;
                        platforms: { platform; connected; can_edit; needs: string | null; supports: { title, category, tags, language, description, thumbnail } }[] }
type Preset = { id; name /* 1–60, unique */; info: StreamInfo; updated_at }   // up to 50
type HistoryEntry = { info: StreamInfo; at: number; results: PlatformResult[] }
```
| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/stream-info` | → `StreamInfoView` |
| POST | `/api/v1/stream-info/apply` | `{ info, platforms?: Platform[] }` (default: every connected one) → **200** all took it, **207** some did, **422** `stream_info_failed` none did; every answer carries `{ results: PlatformResult[], ok, failed }`. 400 for a bad field or platform, 422 `no_platforms` when nothing is connected |
| GET | `/api/v1/stream-info/categories?platform=twitch&q=valo` | → `{ platform, items: { id, name, image_url }[] }` (YouTube: its video categories). 400 without `q`, 422 when that platform isn't connected |
| POST | `/api/v1/stream-info/presets` | `{ name, info }` → 201 `Preset`; 409 name taken |
| PUT / DELETE | `/api/v1/stream-info/presets/{id}` | `Preset` → `Preset`; → 204. 404 `preset_not_found` |

**Per platform (2.0).** A field in `per.<platform>` is what that platform gets instead of the everywhere value; fields it leaves out use the everywhere ones, and an empty entry is dropped. Each platform is sent only its merged fields. Errors name the place (`per.youtube.title: …`); an unknown platform under `per` is 400. Presets and history keep `per`, and Show's checklist sends a preset's `per` too. `!title` from chat sets one title everywhere.

Permissions: Twitch needs `channel:manage:broadcast` (Modify Channel Information, stream markers) and Kick `channel:write`; both are asked for at sign-in from this version. Accounts connected earlier show `can_edit: false` with `needs` ("Reconnect Twitch…") and get `reconnect_needed` instead of a silent failure (R-22). YouTube uses the existing `youtube.force-ssl` sign-in and changes the live (or next scheduled) broadcast's video, keeping its description unless `description` is given; a game category becomes YouTube's Gaming category. `thumbnail` uploads that picture with `thumbnails.set` (50 units); a channel without phone verification is refused ("custom thumbnails need a verified channel; verify yours at youtube.com/verify"). Twitch and Kick have no stream description and make their own thumbnails, so those fields are in their `skipped` with the reason, and a platform with nothing it takes isn't called (`ok`, empty `applied`). A thumbnail that fails next to other changes is in `skipped` as `thumbnail: <why>`; alone it's the platform's `error`.

**Show (go-live checklist).**

```ts
type ShowConfig = { preset_id: string | null; starting_scene: string | null; countdown_s: number /* 0 or 10–3600 */; live_scene: string | null;
                    alerts_on: boolean; discord: boolean; bot_on: boolean; replay_buffer: boolean;
                    start_stream: boolean /* off */; ending_scene: string | null; stop_stream: boolean /* off */; stop_delay_s: number /* 0–600, 30 */;
                    recap_message: string | null /* {thanks} {follows} {subs} {duration} */; mic_sources: string[]; cam_sources: string[];
                    min_disk_gb: number; platforms: Platform[];
                    at_zero: { switch: boolean /* to live_scene, on */; unmute: boolean /* mic_sources */; start_stream: boolean /* if not streaming */;
                               record: boolean; chat: string | null /* 1–400, one line */; marker: boolean /* on */ } }
type Check = { id; label; status: "ok" | "warn" | "fail" | "skip"; detail; fix: { label, href /* "#/settings/obs" or "action:unmute:Mic" */ } | null }
type ShowRun = { id; phase: "starting" | "countdown" | "live" | "ended"; started_at; countdown_ends_at: number | null; live_scene; ended_at;
                 steps: { id, label, status: "done" | "failed" | "skipped" | "waiting", detail }[] }
```
| Method | Path | Body → Response |
|---|---|---|
| GET / PUT | `/api/v1/show/config` | `ShowConfig` (400 `countdown_s: …`, `preset_id: …`, `at_zero.chat: …`). A countdown needs `live_scene` only while `at_zero.switch` is on; at zero, each `at_zero` action that's on runs and adds a `zero_*` step (`live_scene` for the switch) |
| GET | `/api/v1/show/preflight` | → `{ checks: Check[], ready }`: OBS, scenes exist, mics not muted, cameras shown, disk space (OBS's recording drive), platforms connected, stream-info permission, Discord webhook |
| POST | `/api/v1/show/preflight/fix` | `{ action: "unmute:<input>" \| "show:<scene>/<source>" \| "start_replay_buffer" }` → the new pre-flight |
| GET | `/api/v1/show` | → `{ show: ShowRun \| null }` |
| POST | `/api/v1/show/start` | `{ force?: boolean }` → 201 `{ show }`. Runs: preset → replay buffer → starting scene → OBS Start Streaming (`start_stream`, step `stream`) → alerts on → bot on → Discord post → countdown (switches to the live scene at zero). 409 while a show runs, 422 `preflight_failed` (unless `force`) |
| POST | `/api/v1/show/countdown/skip` · `/countdown/cancel` · `/live-scene` | → `{ show }`. 404 `no_countdown` |
| POST | `/api/v1/show/end` | → `{ show }`: ending scene, thank-you in chat, an "Show ended" marker, then with `stop_stream` OBS Stop Streaming (step `stop_stream`: `done` at once with `stop_delay_s: 0`, else `waiting` and updated by a `show` frame when OBS stops; a show started meanwhile keeps streaming) |

**OBS helpers.** `ScenesConfig = { chat_enabled; chat_allowed: string[] /* !scene allow-list */; hotkeys: string[] /* Alt+1…9 in the dashboard */; brb_scene; brb_on_ads; brb_mute: string[]; clip_command; clip_on_big_moments }`.

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/obs/scenes` | → `{ current, scenes: { name, items: { id, source, enabled, kind }[] }[] }` (top of the dock first) |
| GET | `/api/v1/obs/inputs` | → `{ name, kind }[]` |
| POST | `/api/v1/obs/scene` | `{ scene }` → 204 |
| POST | `/api/v1/obs/source` | `{ scene, item_id, visible }` → 204 |
| POST | `/api/v1/obs/input/mute` | `{ input, muted }` → 204 |
| POST | `/api/v1/obs/replay/save` | `{ label? /* ≤200 */ }` → 201 `{ saved, path, marker }`; 422 with how to start the replay buffer |
| POST | `/api/v1/obs/brb` | `{ on }` → `{ on, scene }` (remembers the scene to go back to; mutes `brb_mute`). 422 `no_brb_scene` / `no_return_scene` |
| GET / POST | `/api/v1/obs/stream` | GET → `{ streaming }`; POST `{ on }` presses OBS's Start or Stop Streaming → `{ streaming }` (already in that state is fine). 422 `obs_no_stream_key` when OBS has no stream service or key, `obs_unreachable` / `obs_password` |
| GET / POST | `/api/v1/obs/health` | GET → `{ health: Health \| null }` (the watcher's last reading, every 5 s while OBS is connected); POST reads now |
| GET / PUT | `/api/v1/obs/switcher` | `ScenesConfig` |

`Health = { at, streaming, cpu, memory_mb, disk_gb, fps, render_skipped_pct, encoder_skipped_pct, dropped_pct, bitrate_kbps, warnings: { id: "dropped" | "encoder" | "render" | "cpu" | "disk" | "reconnecting", level: "warn" | "error", message, hint }[] }`, percentages over the last 5 s. Chat (mods, when `chat_enabled`): `!scene <name>` (allowed scenes only, unique prefixes work), `!brb`, `!back`; `!clip` (mods, every 20 s at most, when `clip_command`). With `brb_on_ads`, a Twitch ad break switches to BRB and back when it ends.

**Moments.** `MomentsConfig = { big_tip /* main currency, major units */; big_gift_value /* bits/kicks */; big_sub_gift; raid_viewers; first_chatters; sound: "chime" | "coin" | "bell" | "none"; volume; markers; spike_factor /* 2–10 */; spike_min /* 5–500 per 30 s */; twitch_markers }`. `LaneItem = { id, kind, title, detail, platform, event_id, at }`; `Marker = { id, at, offset_ms /* into the stream */ | null, kind: "tip" | "superchat" | "bits" | "gift" | "raid" | "first_chatter" | "chat_spike" | "clip" | "manual", label, platform, clip }`.

| Method | Path | Body → Response |
|---|---|---|
| GET / PUT | `/api/v1/moments/config` | `MomentsConfig` |
| GET | `/api/v1/moments/lane` | → `LaneItem[]` newest first (50 kept, memory only) |
| POST | `/api/v1/moments/lane/{id}/dismiss` · `/lane/dismiss-all` | → `LaneItem[]`. 404 `lane_item_not_found` |
| GET / POST | `/api/v1/moments/markers?since=<ms>` | → `Marker[]` (default: this stream, else the last 24 h); POST `{ label? }` → 201 manual marker |
| DELETE | `/api/v1/moments/markers/{id}` | → 204. 404 `marker_not_found` |
| GET | `/api/v1/moments/markers/export?format=csv\|txt&since=` | a download: CSV `timecode,offset_seconds,kind,label,platform,clip,unix_ms` (formulas defused) or text lines `00:42:10  label` |

Live frames for Studio (dashboard socket, §5): `lane { items /* newest first */, added: LaneItem | null, sound, volume }` (play the sound for `added`, only in the dashboard), `marker Marker`, `counter { name, value }`, `show { show: ShowRun | null }`, `obs_health Health`, `brb { on, scene }`.

### Go live everywhere (2.0, free)
OBS streams once to Ferrocast's local server (127.0.0.1 only, this install's key); Ferrocast sends the same stream on to every ticked destination, each on its own (no re-encode). Keys are fetched from the platforms (Twitch `channel:read:stream_key`, YouTube liveStreams, Kick `streamkey:read`), never pasted; custom servers keep their key in the secret store. A destination that joins mid-stream starts at the next keyframe; one that fails retries with backoff for as long as OBS streams, saying why; one falling behind skips ahead without slowing the others. Plan: `docs/plan/LIVE-STUDIO.md`.

```ts
type Service = "twitch" | "youtube" | "kick" | "custom"
type Canvas = "main" | "vertical"   // OBS's normal stream, or its vertical canvas (A2.3)
type DestinationConfig = { id: string /* "twitch" | "youtube" | "kick" | "youtube_vertical" | "c_<hex>" */; kind: Service; name?: string; url?: string /* custom */;
                           enabled: boolean; canvas?: Canvas /* left out: "main" */ }
type PushHealth = ({ state: "waiting" | "connecting" | "live" | "stopped" }
                 | { state: "reconnecting"; attempt: number; retry_in_ms: number; reason: string }
                 | { state: "paused"; until_ms: number /* earliest it comes back */; reason: string /* upload protection */ })
                 & { kbps: number; dropped: number; since_ms: number }
type StreamStats = { kbps: number; keyframe_interval_ms: number | null; video_codec: "h264" | "hevc" | "av1" | "vp9" | null;
                     width: number | null; height: number | null; fps: number | null }   // measured from the stream OBS sends; empty while nothing streams
type DestinationView = { id: string; kind: Service; canvas: Canvas; name: string; account: string | null /* platform account, or a custom server's host */;
                         enabled: boolean; connected: boolean; health: PushHealth | null /* while it runs */;
                         blocked: string | null /* why it can't start, in words */; rules: string[] /* the platform's rules this stream breaks */ }
type UploadRow = { id: string; name: string; server: string | null; delay_ms: number | null; need_kbps: number; problem?: string }
type UploadCheck = { at: number; measured_by: "twitch" | "cloudflare"; rows: UploadRow[];
                     verdict: { upload_kbps: number; need_kbps: number; fits: boolean; fit_count: number; message: string } }
type GoLiveView = { ingest: string | null /* rtmp://127.0.0.1:<port>/live */;
                    obs: { connected: boolean; points_here: boolean; streaming: boolean; before: string | null /* "Twitch", "a custom server" */;
                           aitum: { name: string; platform: Service | null /* "custom" never; null when unknown */; live: boolean }[] | null /* Aitum Multistream's outputs, when OBS has it */;
                           vertical: { points_here: boolean; streaming: boolean; before: string | null /* host */ } | null /* Aitum Vertical, when OBS has it */ };
                    live: boolean; stats: StreamStats; vertical_live: boolean; vertical_stats: StreamStats;
                    destinations: DestinationView[] /* top first */; upload: UploadCheck | null;
                    protect: boolean /* upload protection, on by default */; aitum_ok: boolean /* "I use Aitum Multistream on purpose" */;
                    advanced: Advanced;
                    cut: { source: "canvas" | "cut"; config: CutConfig; state: CutState | null /* null while not needed */; ffmpeg: FfmpegState } }
type Advanced = { rules: { slow_pct: number /* 50–95, 70 */; trouble_s: number /* 2–30, 4 */; calm_s: number /* 5–120, 10 */;
                            first_pause_s: number /* 10–3600, 60 */; max_pause_s: number /* ≥ first, ≤ 7200; 600 */; first_to_pause: string | null };
                  keep_free_pct: number /* 0–50, 20: the upload check's reserve */; port: number /* the local server's, from the next start */;
                  per: { [destination id]: { server: string | null /* a platform's own address instead (region, rtmp or rtmps) */;
                                             first_retry_s: number /* 1–30 */; max_retry_s: number /* ≥ first, ≤ 600 */ } } }
type FfmpegState = { state: "ready" | "missing" | "unsupported" } | { state: "downloading"; percent: number } | { state: "failed"; reason: string }
type CutBox = { x: number; y: number; w: number; h: number }   // fractions (0–1) of the normal picture
type CutConfig = { layout: "camera_top" | "small_camera" | "half_half" | "game_only"; camera: CutBox; game: CutBox;
                   camera_share?: number /* 20–60 %, camera_top */; width?: 1080 | 720; height?: 1920 | 1280; fps?: 30 | 60; kbps?: number /* 1,000–12,000 */;
                   encoder?: "auto" | "processor" }
type CutState = { state: "waiting" | "starting" } | { state: "running"; encoder: string; on: string /* "your graphics card" */ }
              | { state: "failed"; reason: string /* in words; it tries again by itself */ }
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/golive` | → `GoLiveView` (asks OBS for its stream setting) |
| GET | `/api/v1/golive/ingest` | → `{ server, key, vertical_key }` for setting OBS up by hand (`vertical_key` for its vertical canvas). Denied to phones. 422 `golive_off` when the local server didn't start |
| PUT | `/api/v1/golive/destinations` | `{ destinations: [{ id, enabled }] }` (every destination once, top first) → `{ destinations: DestinationConfig[] }`. While live, a newly ticked one starts and an unticked one stops at once. 400 for a missing, repeated or unknown id |
| POST | `/api/v1/golive/destinations` | `{ name /* 1–40 */, url /* rtmp://, rtmps://, srt://, rist:// or https:// (WebRTC WHIP) */, key /* no spaces, ≤ 512 */, canvas?: Canvas }` → 201 `DestinationConfig` (ticked, added last; the key is never returned). SRT, RIST and WebRTC servers are sent straight from the Ferrocast Studio's main picture (its own libobs outputs, sharing the stream's encoding; WebRTC gets its own without B-frames and with Opus sound), not by the RTMP forwarders: their key is optional (SRT stream id, RIST passphrase, WHIP bearer token), they refuse `canvas: "vertical"`, their row carries `direct: "srt" | "rist" | "whip"` and the host as `account`, they show live health like the rest (and count for upload protection), and while live from outside OBS they're `blocked` with the reason. 400 names the field (`url: The server address must start with rtmp:// or rtmps://.`), 409 over 8. Denied to phones |
| DELETE | `/api/v1/golive/destinations/{id}` | → 204 (custom servers only; its key is deleted). 400 for twitch / youtube / kick (switch them off), 404 unknown. Denied to phones |
| POST | `/api/v1/golive/obs/setup` | Points OBS's stream setting at the local server, saving the one before (with its key, in the secret store) → `GoLiveView.obs`. 422 `obs_streaming` while OBS streams (OBS refuses; it would cut the stream), `obs_unreachable` / `obs_password` |
| POST | `/api/v1/golive/obs/restore` | Puts OBS's earlier stream setting back exactly → `GoLiveView.obs`. 422 `golive_nothing_saved`, `obs_streaming` |
| POST | `/api/v1/golive/upload-check` | ~8 s → `UploadCheck`: each ticked destination's server and connect delay, the upload speed (Twitch's bandwidth-test mode when Twitch is ticked: nothing goes live; else an upload to Cloudflare's speed test), each needing the stream's measured rate (6,160 kbps before anything streamed), and the verdict with 20% kept free. 422 `golive_none` (nothing ticked), `golive_upload`; 429 while one runs |
| PUT | `/api/v1/golive/protection` | `{ on: boolean }` → `{ destinations, protect }`. Off brings every paused destination back at once |
| POST | `/api/v1/golive/vertical/obs/setup` | Points Aitum Vertical's first stream output at the local server with the vertical key, keeping its own server and key (secret store) → `ObsView.vertical`. 422 `aitum_vertical_missing`, `aitum_vertical_no_output` (add a stream output in its settings first), `obs_streaming` (its canvas streams) |
| POST | `/api/v1/golive/vertical/obs/restore` | Puts Aitum Vertical's own server and key back → `ObsView.vertical`. 422 `golive_nothing_saved`, `obs_streaming` |
| GET | `/api/v1/golive/picture` | → `{ image /* JPEG data URL, 960 wide */, width, height /* the size OBS streams at */ }`: what's on air, for marking the camera and game. 422 `obs_unreachable`, `obs_refused` |
| PUT | `/api/v1/golive/vertical/source` | `{ source: "canvas" \| "cut" }` → `GoLiveConfig`. `cut` without ffmpeg starts fetching it (below); 422 `ffmpeg_unsupported` where there's no build to fetch |
| PUT | `/api/v1/golive/advanced` | `Advanced` → `GoLiveConfig`. The protection rule applies at once, the port from the next start. 400 names the field (`per.twitch.first_retry_s: 1 to 30 seconds.`) |
| POST | `/api/v1/golive/new-key` | A new local key (and vertical key): the local server restarts with it, and OBS and Aitum Vertical are pointed at it again if they pointed here → 204. 422 `golive_streaming` while a stream comes in |
| POST | `/api/v1/golive/ffmpeg` | Fetches ffmpeg (again, after a failure) → `FfmpegState`; progress in `GoLiveView.cut.ffmpeg` |
| PUT | `/api/v1/golive/vertical/cut` | `CutConfig` → `GoLiveConfig`; a running cut restarts with it. 400 names the field (`game: the box goes past the edge of the picture.`) |
| PUT | `/api/v1/golive/aitum` | `{ ok: boolean }` → `GoLiveConfig`. Hides (or shows again) the Aitum Multistream setup note |
| POST | `/api/v1/golive/resume/{id}` | "Resume now" on a paused destination → 204; it starts again at once. 422 `golive_not_paused` |

**Vertical (A2.3).** OBS's vertical canvas (Aitum Vertical, or anything else sending to `vertical_key`) streams to the same local server on its own key, into its own stream. Destinations with `canvas: "vertical"` get that stream and run while it's live, whatever the normal stream does. `youtube_vertical` (YouTube Shorts live) is made by Ferrocast: a reusable YouTube stream "Ferrocast vertical" once (about 150 units the first time), and before each vertical stream a broadcast bound to it that starts and ends by itself, with the title and privacy of the channel's normal live (2 units when it exists); the normal YouTube destination never picks that stream. Twitch and Kick take one live per channel, so vertical goes to YouTube or your own servers (`POST /golive/destinations` with `"canvas": "vertical"`). Upload protection counts vertical destinations after every normal one, so they pause first, each judged against its own stream's rate. The upload check plans 4,660 kbps for vertical before it has streamed.

**Cut from the stream (A2.4).** With `source: "cut"`, while a vertical destination is ticked, ffmpeg (`FERROCAST_FFMPEG`; the one Ferrocast fetched; next to the app, Homebrew or `PATH`) takes the normal stream from Ferrocast, crops the camera and game boxes, fills the layout's slots (scaled to cover, no bars), encodes H.264 on the graphics card when it can (VideoToolbox, NVENC, Quick Sync, AMF; else x264 veryfast), copies the audio and publishes to the vertical key, so the vertical destinations carry it like OBS's canvas. It waits for the normal stream and follows it; an encoder that won't start is skipped for the next; ffmpeg stopping says why in `state.reason` and is tried again after 3 s. The upload check plans `kbps` + 160 for it.

**Getting ffmpeg.** Ferrocast doesn't ship ffmpeg (20 to 30 MB most people never need). Picking `cut` (or `POST /golive/ffmpeg`) downloads one pinned build for this system from ffmpeg-static's GitHub release `b6.1.1` (ffmpeg 6, GPL), refuses it unless its SHA-256 matches the one in the app, unpacks it to `<data>/tools/ffmpeg-b6.1.1/` and starts the cut. Builds: macOS arm64 and x64 (VideoToolbox), Windows x64 (NVENC, Quick Sync, AMF), Linux x64 and arm64 (x264).

**Aitum Multistream (A2.2).** When OBS has the Aitum Multistream plugin (it answers the `aitum-multistream` / `get_outputs` vendor request), `GoLiveView.obs.aitum` lists its outputs: which are sending now (OBS's output `aitum_multi_output_<name>` is active) and the platform each goes to, from the plugin's settings file `<OBS config>/plugin_config/aitum-multistream/config.json` for OBS's current profile (only each output's name and server are read, never its key), else from its name. The dashboard warns when one goes to a platform Go live also sends to: before going live unless `aitum_ok`, and while both are live always.

**Upload protection.** While OBS streams and 2 or more destinations are live, a destination getting under 70% of the stream's rate, or skipping packets, for 4 seconds in a row (each destination is judged after its first 3 live seconds) pauses the bottom-most live one in the streamer's order, so the ones above stay smooth: its health becomes `paused` with the reason. After 10 calm seconds, and no earlier than `until_ms` (60 s for the first pause in a stream, doubling each time up to 10 minutes), the highest paused one comes back. The last live destination is never paused. A paused destination isn't restarted by ticking changes; unticking it ends the pause. When the stream ends, nothing stays paused.

Live frame `golive { live, stats, vertical_live, vertical_stats, destinations, cut_state, ffmpeg }` (§5): every second while OBS streams, and after every change.

### Auto-replies (free, local)
The bot answers chat that mentions something ("discord" → the invite), without a `!command`. Checked on live chat from this channel after moderation passed it; never for a message a command answers (custom, counter, module or points command), never for the streamer's own account (the bot speaks as it, R-28), the connected account, accounts with the bot role or well-known bots (Nightbot, StreamElements…), and never for text the bot itself sent in the last 2 minutes (loop guard). Quiet while the bot is off (`bot_enabled`) or panic mode is on. Rules are checked in order and the **first match wins** (a later rule never answers a message an earlier one matched, even while that one cools down). Each platform has a send budget for auto-replies (R-19: 6 per 30 s on Twitch and Kick, 2 per minute on YouTube); a platform's own rate limit pauses auto-replies there for its `Retry-After`.

```ts
type AutoReply = { id: string /* blank on POST */; name: string /* 1–60 */; enabled: boolean /* true */;
                   match: { kind: "contains" | "word" | "starts_with" | "regex" /* "contains" */; text: string /* 1–200 */; case_sensitive: boolean /* false */ };
                   reply: string /* template, 1–500 characters */; platforms: ChatPlatform[] /* [] = all */;
                   cooldown_s: number /* 30; per rule per platform chat, 0–86400, at least 5 s in practice */;
                   user_cooldown_s: number /* 120; per viewer, 0–86400 */;
                   min_role: "everyone" | "subs" | "vips" | "mods" /* "everyone"; "subscriber", "vip", "mod" are accepted */;
                   only_when_live: boolean /* true */ }
type AutoReplyTry = { matched: boolean; rule_id: string | null; rule_name: string | null; reply: string | null }
```
Matching folds both sides: Unicode NFKC (`ＤＩＳＣＯＲＤ` matches `discord`), whitespace runs as one space, lowercase unless `case_sensitive`. `word` needs a word boundary on each side whose needle edge is a letter, digit or `_` ("game" matches "what game?" and "new-game", not "gameplay"). `regex` is Rust `regex` syntax (linear time, no look-around or back-references), compiled once with a size cap; a pattern that matches the empty message is refused. Reply variables: `{user}` / `{name}` (who wrote it), `{platform}`, `{channel}` (the connected account), `{game}` and `{title}` (last set from Ferrocast; `{game}` falls back to "something fun"), `{uptime}`, `{uptime_text}`, `{count:<counter>}`. The reply is one line, ≤ 500 characters, never starting with `/` or `.`.

| Method | Path | Body → Response |
|---|---|---|
| GET / POST | `/api/v1/autoreplies` | → `AutoReply[]` in order; POST `AutoReply` (defaults above) → 201, added last. 400 names a bad field (`name:`, `match.text:`, `reply:`, `cooldown_s:`), **413 `reply_too_long`** over 500 characters, **422 `bad_pattern`** (`match.text: the pattern has a problem at character 6: unclosed group.` + hint), 422 `pattern_too_complex` / `pattern_too_broad`, **422 `too_many_autoreplies`** at 100, 409 for a taken id |
| PUT / DELETE | `/api/v1/autoreplies/{id}` | `AutoReply` → `AutoReply` (keeps its place, cooldowns start fresh); → 204. 404 `autoreply_not_found` |
| PUT | `/api/v1/autoreplies/order` | `{ ids: string[] }` (every id once) → `AutoReply[]`; 409 when the list changed |
| POST | `/api/v1/autoreplies/try` | `{ text, rule?: AutoReply, platform? }` → `AutoReplyTry`: the saved rules, or only the unsaved `rule` (name and reply may be blank; `reply` is then null). Ignores platform, role, live and cooldowns; renders with the viewer "viewer". A bad pattern answers the same 422 as a save |
| GET | `/api/v1/autoreplies/starters` | → the examples an empty list offers ("discord" → link, "what game" → `{game}`, "hello" → welcome) |

### Settings and data
GET/PUT `/api/v1/settings`: `{ theme: "system" | "dark" | "light", reduced_motion: "system" | "on" | "off", low_motion_overlays, lan_access (next start), never_update_while_live, launch_at_login, close_to_tray, language, currency (ISO 4217, the main currency), keep_chat_days, test_alerts, bot_enabled, auto_ban_on_sight }`. PUT saves only the fields sent (unknown ones are ignored, including the retired `update_channel`); 400 names a bad field (`lan_access: must be true or false.`).
POST `/api/v1/export` → a streamed `.ferropack` download (this install's own settings rows, `pro_status` and `history_upload`, are never in it, and an import never replaces them). POST `/api/v1/import` (multipart `file`, up to 2 GB) → `{ imported, media_files, backup }`; a backup is taken first. 415 `bad_pack` / `damaged_pack`, 422 `pack_too_new`, 413 `too_large`. GET `/api/v1/backups` → `Backup[]`, newest first; POST `/api/v1/backups` → 201 `Backup[]` (takes one now); POST `/api/v1/backups/{id}/restore` → `{ restart_needed, message }` (it's swapped in at the next start, keeping the current data as one more backup).

```ts
type Backup = { id: string; bytes: number; created_at: number;
                reason: "daily" | "update" | "migration" | "import" | "restore" /* the data a restore replaced */ | "manual" | "other";
                version: string | null /* the Ferrocast version that took it; null for older backups */ }
```
Backups are taken automatically: once a day (at start when the newest backup is more than 24 hours old, then every 24 hours while running), right before an update installs (`/updates/install` answers 422 and doesn't start the update when that backup fails), before a database migration and before every import. The newest 7 daily backups and the newest 10 others are kept.

### Importing from StreamElements and Streamlabs
Two steps: a **preview** reads everything and says what would happen (nothing is written), then **apply** imports it. Apply takes a backup first (restore it from Settings → Your data to undo), writes everything in one transaction, never deletes anything, and then the engine reloads commands, timers and points settings. One preview at a time can be applied, for 30 minutes; a new preview replaces it.

```ts
type ImportKind = "commands" | "timers" | "points" | "loyalty_settings" | "tips"
type ImportItem = { name: string; reason: string }
type NotImported = { what: string; count: number /* 0 = not counted */; reason: string }
type ImportPreview = { preview_id: string; source: "streamelements" | "streamlabs"; expires_at: number;
                       account: { name: string; platform: "twitch" | "youtube" | "kick" | null } | null /* StreamElements only */;
                       sections: { kind: ImportKind; found: number; new: number; conflicts: number;
                                   waiting: number /* points for people Ferrocast hasn't seen yet, paid when they show up */;
                                   needs_match: number /* YouTube points with only a name: never paid automatically */; skipped: number;
                                   conflict_items: ImportItem[]; skipped_items: ImportItem[]; needs_match_items: ImportItem[] /* up to 100 each; counts are exact */;
                                   notes: string[] }[];
                       not_imported: NotImported[] }
type ImportReport = { source: "streamelements" | "streamlabs"; backup: string /* backup id */; on_conflict: "keep" | "replace";
                      sections: { kind: ImportKind; found: number; added: number; replaced: number; kept: number; waiting: number;
                                  needs_match: number; skipped: number; skipped_items: ImportItem[]; needs_match_items: ImportItem[]; notes: string[] }[];
                      not_imported: NotImported[] }
```

| Method | Path | Body → Response |
|---|---|---|
| POST | `/api/v1/import/streamelements/preview` | `{ token }` (the JWT from StreamElements → Account → Channels → Show secrets; "Bearer " is fine) → `ImportPreview`. The token is used for this request only: never stored, logged or sent back. Ferrocast only makes GET requests to StreamElements |
| POST | `/api/v1/import/streamlabs/preview` | multipart: `platform` (`twitch` \| `youtube` \| `kick`, needed with `points`), `points` and/or `tips` files (CSV or Excel `.xlsx`, up to 10 MB and 200,000 rows each) → `ImportPreview` |
| POST | `/api/v1/import/apply` | `{ preview_id, on_conflict: "keep" \| "replace" }` → `ImportReport`. 404 `preview_expired` when the preview is gone (used, replaced or older than 30 minutes) |

What comes over:
- **StreamElements**: custom commands (name, aliases, reply, cooldowns, user level: everyone → everyone, subscriber → subs, regular and VIP → vips, moderator and above → mods; `${user}`, `${touser}`, `${1}`…`${9}`, `${1:}`, `${uptime}` become `{user}`, `{target}`, `{arg1}`…, `{args}`, `{uptime}`; a reply with any other variable is imported **switched off**, with a note), timers (first message, online interval, at least 5 minutes, chat lines), points balances (`/points/{channel}/top`; on a YouTube channel these usually carry names only, so they need a match), points settings (name, on/off, amount every 10 minutes, follow/sub/tip/host bonuses as follow/sub/tip/raid bonuses) and tip history. Overlays and custom widgets, loyalty store items and bot modules are listed in `not_imported` with the reason.
- **Streamlabs**: points from a Streamlabs Chatbot Currency export (or any sheet with a name column such as `Name`/`Username` and a points column such as `Points`/`Currency`), and tip history from a sheet with date, name, amount and currency columns (`message` and `id` optional). Commands, timers and widgets can't be exported from Streamlabs; `not_imported` says so.
- **Clashes** (`conflicts`): a command with the same name or alias, a timer with the same name, someone who already has points in Ferrocast, points settings already saved. `keep` leaves Ferrocast's version, `replace` overwrites it in place (same id). Entries listed twice, unusable names or replies, bots, zero balances and tips already imported are `skipped` with the reason.
- **Matching people** (never by display name, which anyone can set): by platform user id when the source has one (a StreamElements `providerId`, or a `User ID`/`Channel ID` column in a Streamlabs file), else on Twitch and Kick by the unique login, case-insensitive. People Ferrocast already knows get their balance now; the rest are `waiting` and paid the first time that id or login shows up (an account without a login can only be matched by id). YouTube names aren't unique: a YouTube balance with only a name is `needs_match`, listed in the report, kept (in `.ferropack` too) but never paid automatically; the streamer adds it by hand from the viewer card (`POST /api/v1/loyalty/viewers/{platform}/{user_id}/adjust`). Tip history is kept for records and `tips.csv`; it never fires alerts or changes goals, the recap or the activity feed.

Errors: 400 `invalid` (`token:` doesn't look like a JWT, `platform:`, no file), 401 `streamelements_token_rejected` (expired or not the JWT; the hint says where to copy a fresh one), 409 `conflict` (another import step is running), 413 `too_large` (a file over 10 MB or 200,000 rows), 415 `unsupported_type` (not CSV or .xlsx, including old .xls), 422 `bad_file` (the message names the file, row and column: `points file: row 14, column "Points": "lots" isn't a whole number of points (0 or more).`), 429 `streamelements_rate_limited` with `Retry-After`, 502 `streamelements_unavailable` (their 5xx) / `streamelements_unreachable` / `streamelements_bad_response` / `streamelements_error`. Nothing is imported when a preview fails.

## 5. Live socket for the dashboard

`GET /api/v1/live?session=<token>&since=<seq>` (WebSocket; `since` optional). With `since`, the `event` frames after that seq (the last 500 are kept) are replayed right after `hello`, `status` and `queue`, with their `held` marks; `hello.seq` is then `since` itself, and `resync { missed: 0 }` (0 = count unknown) comes first if some were already forgotten or `since` is from before a restart. Seqs only grow, across restarts too. Drop duplicate event ids. Server → client frames `{ v: 1, type, data }`:
- `hello { seq }`
- `event Event` (everything, including chat)
- `status { connectors }`
- `queue Queue`
- `tts_pending [...]`
- `notice { key?, level: "info" | "warn" | "error", message, action?: { label, href } }`. A notice replaces the shown one with the same `key` (one line per subject, never a stack): `panic`, `account:twitch` / `account:youtube` / `account:kick`, `browser` (desktop app only: it couldn't hand a page, such as a sign-in page, to the default browser), `discord`, `update`, `no-chat`, `alerts-folded` (one with `action: { label: "Reconnect", href: "#/settings/accounts" }` when a platform goes to `sign_in_needed` or `scopes_missing`)
- `goals Goal[]` (after any goal change)
- `held { event_id, reason }` (the chat filter held a message for a mod; its `event` frame comes first and it never reaches overlays)
- `held_resolved { event_id, outcome: "released" | "dropped" }` (a mod approved or removed a held message, from any dashboard; it has its own seq, so a reconnecting dashboard replays it even with a `since` from after the `held` mark)
- `panic { on, plan: PanicConfig }`
- `chatter { event_id, kind: "first" } | { event_id, kind: "returning", days }` (after that message's `event` frame; replayed with it on reconnect)
- `giveaway { giveaway: GiveawayView | null }` (on every change, at most every 250 ms while entries come in)
- `poll { poll: PollView | null }` (on start, end, removal, and at most every 250 ms while votes come in)
- `update UpdateView` (§4): on every change (a check starting or finishing, download progress at most 5 times a second, installing, ready, failed)
- `account AccountView` (§4 Ferrocast account): on every change (sign-in started, finished, failed or cancelled, signed out, a refresh that changed it). Never sent to phones.
- `golive { live, stats: StreamStats, vertical_live, vertical_stats: StreamStats, destinations: DestinationView[], cut_state: CutState | null, ffmpeg: FfmpegState }` (§4 Go live everywhere): every second while OBS streams to Ferrocast, and after every change
- `smart_mod SmartSuggestion` (§4 Smart features): a new suggestion, or one that changed (done, undone, dismissed, an error). Never sent to a mod's phone.
- `resync { missed }`: the socket fell behind; refetch status, queue and events

The client sends nothing but pings.

## 6. Overlay protocol

Page: `GET /o/<token>/` (the overlay bundle). Media: `GET /o/<token>/media/<id>`. Socket: `GET /o/<token>/ws`.

Server → overlay frames `{ v: 1, seq, type, data }`:
| type | data |
|---|---|
| `snapshot` | `{ overlay: Overlay, state: { goals: Goal[], labels: { [widget_id]: string }, viewers: { [platform]: number }, events: Event[] (last 25 alertable), chat: Event[] (last 50), poll: PollView \| null, giveaway: GiveawayCard \| null, show: { countdown_ends_at: number \| null }, music: MusicNow } }`, always first. `show.countdown_ends_at` is Start show's countdown end (ms): a text layer with `countdown` follows it unless `countdown_show: false`, restarts each time it comes on air unless `countdown_restart: false`, and counts up ("+01:23") past zero with `countdown_up` |
| `event` | `Event`: chat messages and alertable events the overlay's widgets need (filtered server-side) |
| `alert` | `RenderedAlert` |
| `alert_hide` | `{ alert_id }` |
| `state` | partial `{ goals?, labels?, viewers?, poll?, giveaway?, show? }` (`show` whenever Start show's countdown starts, ends or is cancelled) (`poll` and `giveaway` only to overlays with that widget) |
| `config` | `Overlay` (re-render without reloading) |
| `music` | `MusicNow` (§4 Music), `seq: 0` (never replayed): only to overlays with an `fx`, `music` or `sound` layer, when the song or playing changes at once and the loudness at most 15 times a second |
| `delete_message` | `{ platform, message_id }` |
| `delete_user` | `{ platform, user_id }` |
| `clear_chat` | `{ platform: Platform \| null }` |
| `tts` | `{ id, text, audio: string \| null }`: a mod approved a held TTS message. Play `/o/<token>/media/<audio>` when present, else speak `text` with the browser's voice |
| `revoked` | `{}`: this URL was regenerated or the overlay deleted; the server closes the socket next with code 4404 |

Overlay → server frames: `hello { since_seq }`, `ack { alert_id, phase: "started" | "finished" | "failed" }`, `visibility { visible }`. Anything else is ignored and logged. **An overlay can never create events** (R-36).

A socket opened with a token the engine doesn't know is closed at once with code **4404**; treat it like `revoked` (clear, stop reconnecting).

Test mode: `/o/<token>/ws?test` gets the snapshot (layout, goals and viewer counts; no real chat, events or names), then a sample `event` (and its `alert` when a rule covers it, ids `preview-<n>`, `test: true`) at once and every 6 s, plus `config`/`revoked` for its overlay. Nothing from the real queue reaches it, and its acks are ignored.

Build notes for `ferrocast-web`: the overlay bundle is served under `/o/<token>/`, so build it with Vite `base: "./"` (assets resolve to `/o/<token>/assets/…`). The dashboard is served under `/app/` (Vite `base: "/app/"`); unknown paths without a file extension fall back to `index.html`. Dashboard `<img>`/`<audio>` previews may pass the session as `?session=` on `GET /api/v1/media/{id}`.

## 7. Not in this build yet

`POST /accounts/{platform}/connect` answers 422 with a message when this build has no app registered for that platform (Twitch and YouTube need the build's client ids; Kick and tips need the relay). `POST /overlays/{id}/add-to-obs` answers 422 with a hint when OBS can't be reached. `/updates` has `latest: null` until a signed release is published (and always in the headless app, which doesn't update itself: `can_install: false`).

## 8. Relay: Ferrocast Pro billing

The relay's own HTTP API for Pro (`crates/relay/src/billing`), at `https://ferrocast-relay.clearcraftlabs.com`. App routes take the install's `Authorization: Bearer <install_id>.<install_secret>` (from `POST /v1/installs`); errors are `{error:{code,message,hint}}` with the statuses above (401 `unauthorized` without a known install).

| Method | Path | Body → Response |
|---|---|---|
| GET | `/v1/pro/status` | `?refresh=1` asks Razorpay first (at most every 10 s per install) → `{ pro, status, plan, subscription_id, current_end, access_until, cancel_at_period_end, refunded, installs, sales_open, prices, checked_at, features, quotas, warning? }`. `features` and `quotas` are as in §9, for every install (`quotas` is `{}` without a subscription) |
| POST | `/v1/pro/checkout` | `{ plan, email }` → 201 `{ subscription_id, checkout_url /* https://rzp.io/… */, key_id, plan, amount, currency }`. 409 `pro_not_on_sale` while `PRO_SALES_OPEN` is off. 10 per install per hour, one at a time; an earlier unpaid checkout is cancelled at Razorpay first (if Razorpay says it was paid meanwhile: 409 `already_pro`). |
| POST | `/v1/pro/cancel` | → status; cancel at the end of the cycle |
| POST | `/v1/pro/manage-link` | → 201 `{ url, expires_at }` (`/pro/manage/<token>`, 30 minutes, token stored hashed) |
| POST | `/v1/pro/restore` | `{ subscription_id, email }` → status. 5 tries per install and 20 per address per hour. |
| GET | `/pro/manage/{token}` | HTML: plan, status in words, renewal or end date, "Stop renewing". `no-store`, `no-referrer`, no scripts. 404 page once expired. |
| POST | `/pro/manage/{token}/cancel` | HTML result (409 page if already cancelled) |
| POST | `/billing/razorpay/webhook` | Razorpay → relay. `X-Razorpay-Signature` = hex HMAC-SHA256 of the raw body with `RAZORPAY_WEBHOOK_SECRET` (constant-time compare) else 401; `X-Razorpay-Event-Id` required (400); body must be JSON with `event` (400). Each event id is handled once (`{status:"duplicate"}`), recorded in the same transaction as its effect. `subscription.*` re-reads the subscription from Razorpay and copies that state; `subscription.charged` also records the payment id and amount; `refund.*` re-reads the payment's refunds and ends Pro (and cancels the subscription now) once refunds that didn't fail add up to the payment. Subscriptions and payments that aren't Pro's (the Razorpay account is shared with other products) → 200 `{status:"ignored"}`, no Razorpay call. Razorpay unreachable → 502, storage → 503, without recording the event, so Razorpay's retry gets another go. 503 `billing_unavailable` until the secret is set. |

Data kept (R-51): subscription id, plan, status, period end, cancel/refund flags, an HMAC-SHA256 of the lowercased checkout email keyed with `PRO_EMAIL_KEY` (the checkout email itself never; a signed-in account's own verified email is kept with the account, see ACCOUNTS.md §8), which account it belongs to, which installs share it, payment ids and amounts, handled event ids (30 days), hashed manage tokens. Never card, UPI, name or phone. A daily re-check re-reads unfinished subscriptions and the refunds of the last 60 days' payments, in case webhooks were lost.

## 9. Relay: Pro features

Every Pro feature (docs/plan/PRO.md F1–F7) runs on the relay under `/v1/pro/<feature>/…` with the same install `Authorization` and error shape as §8. This is the contract each one follows as it ships; its routes are added as §9.x (§9.1 stream history, §9.2 tip branding, §9.3 cloud backups, §9.4 voices, §9.5 the dashboard from anywhere (online and, M10, offline), §9.6 the Discord bot, §9.7 the cloud bot (with YouTube from the web, 1.4) and §9.8 the weekly email are built; §9.9 is the owner's stats, not a Pro feature; §9.10 the smart features, 1.4).

**The gate.** Every Pro feature route checks, first:

| Status | Code | When |
|---|---|---|
| 401 | `unauthorized` | No `Authorization`, or an install the relay doesn't know |
| 402 | `pro_required` | The install has no current Pro: never bought, not paid yet, lapsed past `access_until` (every renewal failed, or the period ended) or refunded. `message`: "This runs on Ferrocast's servers and is part of Pro.", `hint` points to Settings › Pro |

Access is the same rule as `/v1/pro/status` `pro` (paid period, 3-day grace for a late renewal, none after a full refund), so Settings › Pro and a feature never disagree. A lapsed subscription stops cloud work at the next hourly sweep, keeps its data 30 days, and its export stays open (R-04).

**Shared errors** (besides each feature's own):

| Status | Code | When |
|---|---|---|
| 413 | `too_large` (or the feature's own code) | `message` states the limit |
| 415 | `unsupported_type` | `message` says which types are allowed |
| 429 | the feature's quota code (e.g. `tts_quota`) | A per-subscription counter is used up. Headers: `X-Quota-Reset` (when it starts again, Unix ms) and `Retry-After` (seconds). The hint gives the date |
| 503 | `vault_unavailable` | The relay has no vault key yet (features that keep secrets), or a saved sign-in can't be read (hint: connect that account again) |
| 503 | `storage_unavailable` | The relay can't store files right now |

**Features and quotas in the status.** `/v1/pro/status` `features` says which features this relay has, built *and* configured (`cloud_bot`, `remote`, `tts`, `discord`, `backups`, `tip_brand`, `history`, `weekly_email`, `tip_domain`, `smart`; `smart` when the relay has a free-tier model (`CLOUDFLARE_AI_ACCOUNT_ID` + `CLOUDFLARE_AI_TOKEN`, or `GEMINI_API_KEY`); `tip_domain` when the relay has `CF_SAAS_TOKEN`, `CF_ZONE_ID` and `RELAY_VAULT_KEY`; `history` is always on, `remote` when the relay has `RELAY_REMOTE_URL` and a pepper, `backups` and `tip_brand` are on when the relay has a file store, `tts` when it has a voice provider (`GOOGLE_TTS_KEY`), the rest are `false` until each ships). `quotas` lists the subscription's counters as `{ [name]: { used, limit, period: "day" | "month", reset_at } }`; quotas are per subscription, shared by its installs, and count in UTC calendar days or months.

**The dashboard.** A Pro feature's panel is hidden unless the feature is in `features` and `active || sales_open`. Not Pro (but on sale): one calm line, "Runs on Ferrocast's servers, part of Pro.", and a link to Settings › Pro, never a nag. Pro: the feature itself.

### 9.1 Stream history (F7; free accounts too, 1.4)

`crates/relay/src/history`, migration 4 (`streams`, `stream_samples`). One row per stream **per platform**. **Who keeps it** (1.4): Pro keeps **a year** (rows owned by the subscription, so the PCs sharing Pro share one history); a **free Ferrocast account** keeps the **last 30 days** (rows owned by the account id, `ac_…`, shared by the account's PCs); a PC with neither gets 403 `account_required` ("Stream history needs a free Ferrocast account on this PC.", hint: sign in to Ferrocast on this PC). A PC reads every key it has: its subscription's (a lapsed one too, while kept) and its account's, so what a free account kept shows after buying Pro; a stream begun free and finished on Pro is under both and the newer copy is shown. These routes are under `/v1/pro/history` for compatibility; for a free account they don't need Pro. The engine (`crates/engine/src/history.rs`) counts each live platform while Pro is active or the PC is signed in to an account, on a relay that has `features.history` (the remembered status and the keychain, no network check), uploads the stream when it starts and again with its totals when it ends, and a viewer sample every 5 minutes while live. Uploads wait in a queue in the local store (settings `history_upload`) and are retried with backoff (30 s doubling to 15 min); 400/404/409/413/415/422 answers are dropped (retrying can't help), 401/402/429/5xx and no connection keep them. A stream the app restarted in the middle of ends where it was last seen once its platform isn't live for 10 minutes. Only counts are stored, never chat, names or viewers (R-51).

```ts
type HistoryStream = {
  platform: "twitch" | "youtube" | "kick"
  key: string                    // the engine's id for it: "<platform>:<started_at>"
  started_at: number; ended_at: number | null   // null while live
  title: string | null; category: string | null // category: the last one set from Ferrocast
  peak_viewers: number; avg_viewers: number
  chat_lines: number; chatters: number; new_chatters: number   // chatters counted once each; new = first message ever
  follows: number; subs: number                  // subs include gifted ones
  tips: { currency: string; minor: number }[]    // per currency, never converted (R-32)
  platform_value: { unit: string; amount: number }[]   // "bits", "kicks", "jewels"
  updated_at: number
}
type HistoryWeek = { week_start: number /* Monday 00:00 UTC */; streams: number; by_platform: { twitch: number; youtube: number; kick: number };
                     minutes: number; peak_viewers: number; avg_viewers: number /* weighted by length */; follows: number; subs: number; chat_lines: number }
```

| Method | Path | Body → Response |
|---|---|---|
| POST | `/v1/pro/history/streams` | `HistoryStream` without `updated_at` (`platform`, `key`, `started_at` required; the rest default to 0 / empty) → 201 `{stream}` new, 200 `{stream}` replaced. Same `platform` + `key` is the same stream (a retry changes nothing); the peak never goes down. 409 `ended_stream_locked` once it ended over 24 h ago (a first upload of an old stream is still taken). |
| POST | `/v1/pro/history/samples` | `{ platform, key, samples: [{ at, viewers }] }`, 1 to 100 → `{ saved }`. Same `at` again keeps the newer count. While the stream is live its peak and average follow the samples. 404 `stream_not_found`, 409 `ended_stream_locked`, 413 `too_many_samples` over 100, 422 `invalid_sample` (empty, negative, or `at` outside the stream ± 1 minute). |
| GET | `/v1/pro/history` | `?from&to&platform` (default the last 28 days; up to 400 days) → `{ from, to, streams /* newest first, at most 2,000 */, truncated, weekly /* every week from from's to to's, empty ones too, oldest first */, plan: "free" \| "pro", keep_days: 30 \| 365, earliest /* ms: the oldest start this plan shows */, limited /* the asked from was older than earliest: the dashboard shows a calm Pro note */ }`. `from` is moved up to `earliest` |
| GET | `/v1/pro/history/samples` | `?platform&key` → `{ platform, key, samples: [{ at, viewers }] }` oldest first (the per-stream chart). 422 `invalid_platform` / `invalid_key`, 404 `stream_not_found` (not in any of this PC's history) |
| GET | `/v1/pro/history/export.csv` | Same filters (default everything kept, 400 days) → `text/csv; charset=utf-8`, `Content-Disposition: attachment; filename="ferrocast-history.csv"`, `no-store`. Columns `platform, started_utc, ended_utc, minutes, title, category, peak_viewers, avg_viewers, chat_lines, chatters, new_chatters, follows, subs, tips, platform_value`; text is quoted and a leading `= + - @` gets a `'` (no spreadsheet formulas). **Open after Pro ends** while the history is kept (R-04), and to a free account (everything it keeps): 403 `account_required` only with no account and no subscription, or a lapsed one with nothing kept. |

Errors (besides the gate's 401 / 403 `account_required`, which every route checks before the body): 400 `invalid` (not JSON, a missing or wrong-typed field, a `from`/`to` that isn't a number; the message names it), 415 `unsupported_type` (no `Content-Type: application/json`), 413 `too_large` (over the relay's 256 KB), 422 `invalid_stream` (the message names the field: unknown platform, bad key, a start in the future, or older than the plan keeps (30 days free, a year on Pro), an end before the start or over 31 days after it, negative counts, `avg_viewers` over `peak_viewers`, `new_chatters` over `chatters`, a currency that isn't three capitals, two entries for one currency), 422 `invalid_range` (`to` not after `from`, over 400 days), 422 `invalid_platform`, 429 `history_rate` (60 requests a minute per subscription, every route together; `X-Quota-Reset`, `Retry-After`; not a quota, so not in `quotas`), 503 `storage`.

**The retention sweep** (hourly housekeeping): a free account's streams older than 30 days go (an account with Pro now keeps a year, a deleted account's all go); a subscription's streams older than a year go while it grants Pro, and all of them 30 days after its access ended (period over, every renewal failed, refunded, or the subscription is gone); samples go with their stream, and any older than 400 days for everyone. Titles and categories lose control and direction characters and are cut to 300 / 120 characters.

### 9.2 Tip page branding and goal (F6)

`crates/relay/src/tips/{brand,goal}.rs`, migration 3 (`tip_brand`, `tip_goals`, `tip_goal_payments`). Owned by the install (its tip page). `features.tip_brand` is true when the relay has a file store; `quotas.tip_images` counts image uploads (30 per subscription per UTC day). The engine passes each call through unchanged (the dashboard's paths are `/api/v1/tips/brand…` and `/api/v1/tips/goal`, same bodies and answers; the engine checks an image's type and size before sending it).

```ts
type TipBrand = { accent: string | null; background: string | null; text: string | null  // "#rrggbb", all null = default look
                  avatar_url: string | null; banner_url: string | null                    // https://<relay>/t/a/<sha256>.webp
                  applied: boolean                                                         // the tip page shows it (Pro now)
                  changes: { field: "text" | "accent"; from: string; to: string; contrast_before: number; contrast_after: number; message: string }[] }
type TipGoal = { title: string; target: number /* minor units */; currency: string; digits: number; starts_at: number; ends_at: number | null; ended: boolean
                 raised: number /* minor units */; tips: number; page_url: string | null; overlay_url: string | null /* page_url + "?overlay=1" */ }
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/v1/pro/tips/brand` | → `TipBrand` (`changes` empty) |
| PUT | `/v1/pro/tips/brand` | `{ accent, background, text }` (`#rgb` or `#rrggbb`) → `TipBrand`. Text under 4.5:1 on the background, or an accent under 3:1, is moved towards white or black until it passes; the background never changes. `changes` says what moved, with a sentence to show. |
| DELETE | `/v1/pro/tips/brand` | → 204, the default colours again (images stay) |
| PUT | `/v1/pro/tips/brand/{avatar\|banner}` | Raw image body, `Content-Type: image/png \| image/jpeg \| image/webp` → `TipBrand`. Avatar ≤ 1 MB, banner ≤ 3 MB, at most 4096 × 4096 pixels. Decoded, turned upright, shrunk to fit 512 × 512 / 1500 × 500 and re-encoded as lossless WebP from the pixels only (EXIF, GPS, ICC and XMP never kept). Stored by its SHA-256. |
| DELETE | `/v1/pro/tips/brand/{avatar\|banner}` | → 204 (the file goes when nothing else uses it) |
| GET | `/v1/pro/tips/goal` | → `TipGoal`, or 404 `not_found` without one |
| PUT | `/v1/pro/tips/goal` | `{ title /* 1–60 */, target /* 1 to 10^12 minor units */, currency /* 3 letters */, ends_at? /* future, within 366 days */ }` → `TipGoal`. The same currency keeps the goal and what it raised; another currency starts the count again. |
| DELETE | `/v1/pro/tips/goal` | → 204, the goal and its count are deleted |

Errors (the gate's 401 / 402 come before the body): 400 `invalid` (the message names the field), 404 `not_found` (unknown image name), 413 `too_large` ("Avatars can be up to 1 MB.", "Banners can be up to 3 MB.", or too many pixels), 415 `unsupported_type` (by the header or by the file's own bytes; "… can be PNG, JPEG or WebP."), 422 `image_unreadable` (broken or empty file), 422 `no_tip_page` (a goal needs a tip page), 429 `tip_image_quota` (+ `X-Quota-Reset`, `Retry-After`), 503 `storage_unavailable`.

Public pages (script-free, the tip page's CSP with `img-src 'self' data:`, `no-referrer`):

| Path | What |
|---|---|
| `GET /t/a/<sha>.webp` (relay and tip host) | A stored image: `image/webp`, `Cache-Control: public, max-age=31536000, immutable`, `nosniff`. 404 for anything else. |
| `GET /t/<handle>` · `<tip host>/<handle>` | The tip page; while the install is Pro, in its colours with its banner, avatar and the goal's progress (linking to the goal page). Not Pro: the default look. |
| `GET /t/<handle>/goal` · `<tip host>/<handle>/goal` | Title, progress bar (`role=progressbar`), raised of target, percent, end date, a Tip button back to the page, and "Counts verified tips in <currency> since <date>. Tips in other currencies aren't counted. Refunds come off." 404 page without Pro or a goal. `?overlay=1`: transparent background, just title, bar and numbers, reloads itself every 30 s (meta refresh) for an OBS browser source. |

Counting: the relay counts each tip it verifies and forwards for this install (§4 Tips) while the install is Pro, the goal is running, and the tip is in the goal's currency (Ko-fi's decimal amounts and Stripe's hundredths are converted to the currency's minor unit). A full refund or a dispute takes that tip off; a tip whose refund arrived first is never counted (R-32).

Data kept (R-51): the three colours and two image hashes per install; the image files (pixels only); the goal's title, target, currency and dates; per counted tip only `(goal_id, SHA-256 of install|provider|payment id, amount_minor)`, never a name, message or payment id. Payment rows (and the goal) are deleted 60 days after the goal ends, or with the goal. An install without Pro keeps all of it 30 days after the hourly sweep first sees the lapse (Pro again clears that), then it's deleted with its images.

#### Your own address (custom tip domain)

`crates/relay/src/tips/domains/{mod,hostname,cloudflare,proof,tests}.rs`, migration 12 (`tip_domains`, after M10's 11 and independent of it). A streamer points a subdomain they own (`tips.mychannel.com`) at their tip page: the relay adds it to Cloudflare for SaaS as a custom hostname on our zone (`trait CustomHostnames`: create with `ssl: { method: "http", type: "dv", settings: { min_tls_version: "1.2" } }` (no `custom_metadata`: Enterprise only on custom hostnames), read, find by exact name, delete) and the streamer adds **two records**: a CNAME to the tip host, and a TXT record `_ferrocast.<hostname>` whose value proves this claim (`ferrocast-proof-` + 32 hex of HMAC-SHA256 over the install id and hostname, keyed by HKDF from `RELAY_VAULT_KEY` with its own label). Owned by the install (its tip page); **one per subscription**, 100 on the relay. `features.tip_domain` is true once `CF_SAAS_TOKEN`, `CF_ZONE_ID` and `RELAY_VAULT_KEY` are set; `quotas.tip_domain_changes` counts adds and removals (10 per subscription per UTC day; removing is never refused). The engine passes each call through at `/api/v1/pro/tips/domain`, with the relay's 200, 201 or the 409 that carries `proof` (an owner's phone may GET it, never change it).

```ts
type TipDomain = { hostname: string                                  // ASCII (IDN as punycode), lowercase
                   hostname_unicode: string                           // what to show (tips.bücher.de)
                   status: "pending" | "verifying" | "live" | "failed" | "proof_missing"
                     // waiting for the records | records work, certificate on its way | working | Cloudflare gave up | it worked, then the TXT record went
                   hostname_status: string; ssl_status: string        // Cloudflare's own words
                   dns: { type: "CNAME"; name: string /* full */; host: string /* "tips" */; target: string /* the tip host */ }[]
                   proof: { type: "TXT"; name: string /* _ferrocast.<hostname> */; host: string /* "_ferrocast.tips" */; value: string; found: boolean }  // required, and stays
                   verification: { type: "TXT"; name: string; value: string; why: "ownership" | "certificate" }[]  // Cloudflare's, optional; empty once live
                   errors: string[]                                   // what's missing (the TXT proof first, then Cloudflare's words), no repeats; empty once live
                   page_url: string; goal_url: string                 // https://<hostname>, …/goal
                   created_at: number; checked_at: number             // when Cloudflare last answered a check
                   expires_at: number | null                          // not live: removed then (7 days after added or last live)
                   lapsed_at: number | null; delete_at: number | null }  // Pro ended: works until delete_at
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/v1/pro/tips/domain` | → `TipDomain`, checking again (Cloudflare and the TXT record over DNS-over-HTTPS) when nobody did in the last minute (else the stored answer; Cloudflare or DNS down keeps the stored one and `checked_at`). 404 `not_found` without one (also once it was removed: expired or claimed by someone else). The dashboard polls it every 15 s while pending or verifying. |
| PUT | `/v1/pro/tips/domain` | `{ hostname }` → 201 `TipDomain` (status `pending`). The same address again → 200 (nothing new is made). `https://`, a trailing `/` or `.` and capitals are dropped. An address someone else added that isn't live 24 hours after it was added (or last worked) is taken over: theirs is deleted from Cloudflare and here (their panel just shows no address). |
| DELETE | `/v1/pro/tips/domain` | → 204: deleted from Cloudflare and here; the usual tip link keeps working |

Errors (gate first, then the feature, then the body; every one has a message and a hint): 400 `invalid` (no `hostname`), 404 `not_found`, 409 `domain_limit` (this tip page already has another address, or another PC of the subscription has one: remove it first), 409 `domain_taken` (someone else's here and live or under 24 hours old; or already on Cloudflare (Cloudflare says duplicate) and this install's TXT proof isn't published right now: "That address is set up somewhere else on Cloudflare.", with a hint to add the TXT record and try again, and the record itself in the body, `{ error, proof: { type: "TXT", name, host, value } }`, since it's needed before it exists. Once `_ferrocast.<hostname>` holds this install's value (read fresh, not from the cache), the existing hostname is adopted with 201 and counts as proven: only the domain's DNS owner can publish it), 422 `invalid_hostname` (not a name, a wildcard, an IP, a port or path, `localhost`/`.local`/other non-internet names, `clearcraftlabs.com` or the relay's own hosts, a label mixing scripts (UTS #39), a label confusable with our own domains' names (`clearcraftlabs`, skeleton compare), or Cloudflare refused that hostname: our own words, never Cloudflare's), 422 `apex_not_supported` (a bare domain by the Public Suffix List with private entries: `mychannel.com`, `mychannel.co.uk`, `nova.github.io`, `nova.pages.dev`; the message suggests `tips.<it>`), 422 `no_tip_page`, 429 `tip_domain_quota` (+ `X-Quota-Reset`, `Retry-After`), 502 `cloudflare_unavailable` (no answer, 5xx or 429: nothing changed), 503 `domains_full` (100 on the relay), 503 `domains_unavailable` (no Cloudflare settings or vault key, or a zone-level Cloudflare error: bad zone id, refused token, Cloudflare for SaaS off; logged once).

**Serving (fails closed).** Every request's `Host` (HTTP/2: the `:authority`) is normalised (lowercase, no port, no trailing dot); none, or one that can't be read, is a plain-text 400. Ferrocast's own hosts (the relay, the tip host, the remote host and anything under their registrable domains or `clearcraftlabs.com`, IPs, single-label names, `localhost`; worked out once at start) never reach the lookup and behave as before, so container health checks by IP or `localhost` are unaffected. A **live** address (Cloudflare's hostname status `active` and the TXT proof matching) answers `GET /` with that install's tip page (the goal card links to `/goal`), `GET /goal` (and `?overlay=1`) with the goal page (its Tip button goes to `/`), `/t/a/<sha>.webp` as on the tip host, and a 404 tip page for everything else (no API on someone's address). A pending address, or a live one without a tip page, gets a small script-free 404 "Not set up yet" page (nothing from the request in it); a visit to a pending one checks again in the background (at most once a minute). **Every other host gets that page on every path and method.** Same CSP and headers as the tip page. Host → install answers are kept in memory 30 s, "nobody's address" 5 s in a separate bounded map (so made-up hosts never push out a working address); a generation counter goes up on every add, removal and status change, and a lookup only stores what it read if none came in between. TXT answers are kept 30 s.

**Sweep and lapse.** The hourly sweep checks every address (Cloudflare and the TXT record, live ones too). A live address whose TXT record is missing three sweeps running stops (`proof_missing`) until the record is back (any check that finds it restores it; only sweeps count misses). An address that isn't live 7 days after it was added or last worked is deleted from Cloudflare and here; only live addresses and unfinished ones younger than that count towards the 100. A lapsed install's address keeps working (the free look) 7 days after the hourly sweep first sees the lapse (Pro again clears it), then it's deleted from Cloudflare and here (a Cloudflare error keeps it for the next sweep).

Data kept (R-51): per install the hostname, Cloudflare's id and statuses, its optional TXT records and messages, the TXT proof's last-seen time and miss count, and the dates (added, checked, last live, lapsed). Nothing about visitors.

### 9.3 Cloud backups (F5)

`crates/relay/src/backups`, migration 5 (`cloud_backups`, `cloud_backup_parts`). Backups belong to the subscription (its PCs share the list). `features.backups` is true when the relay has a file store; `quotas.backup_uploads` counts uploads started (6 per subscription per UTC day). The relay only ever holds **ciphertext**: the engine (`crates/engine/src/cloud_backup.rs`) exports the same `.ferropack` as `POST /api/v1/export` (no tokens or secrets), encrypts it on the PC with [age](https://age-encryption.org) to an X25519 key made on first use and kept in the secret store as `cloud-backup-key`, and uploads that.

The file (`.ferrobackup`): a line `ferrocast-backup v1`, an optional line `passphrase <base64 of an age scrypt file holding the AGE-SECRET-KEY line>` (when a passphrase is set: a new PC can then restore with the passphrase alone), an empty line, then an age file encrypted to the X25519 recipient holding the pack. Unknown header lines are skipped (later versions).

```ts
type CloudBackup = { id: string /* bk_ + 24 hex */; bytes: number /* encrypted */; sha256: string; created_at: number /* completed */
                     app_version: string; from_install: string; this_install: boolean; reason: "automatic" | "manual" }
```

| Method | Path | Body → Response |
|---|---|---|
| POST | `/v1/pro/backups` | `{ bytes, sha256 /* of the ciphertext */, app_version, reason: "automatic" \| "manual" }` → 201 `{ id, part_size /* 8388608 */, parts }`. One upload at a time per subscription: another PC's upload that moved in the last hour is 409 `upload_in_progress`; this PC's own unfinished upload (it restarted) or a stalled one is replaced. 413 `backup_quota` when it can't fit even after pruning (over 250 MB alone, or only backups made by hand left). 429 `backup_upload_quota` (+ `X-Quota-Reset`, `Retry-After`). |
| PUT | `/v1/pro/backups/{id}/parts/{n}` | Raw body, `Content-Type: application/octet-stream` only (415 otherwise), parts numbered from 0, each exactly `part_size` bytes except the last (the rest) → `{ part, bytes }`. 413 `too_large` over `part_size` (by `Content-Length` or while streaming), 422 `invalid_part` (number out of range, wrong size), 400 `invalid_part` (the body broke off), 409 `already_complete`. Sending a part again replaces it. |
| POST | `/v1/pro/backups/{id}/complete` | → 201 `{ backup: CloudBackup, pruned: string[] }` (200 if already complete). Joins the parts and checks size and SHA-256: 409 `hash_mismatch` **deletes the upload**; 409 `parts_missing` keeps it (the message names the part). To make room the oldest automatic backups are deleted first (`pruned`). |
| GET | `/v1/pro/backups` | → `CloudBackup[]`, newest first (complete ones only). |
| GET | `/v1/pro/backups/{id}` | → the ciphertext, streamed: `application/octet-stream`, `Content-Length`, `X-Content-SHA256`, `Content-Disposition: attachment; filename="ferrocast-<id>.ferrobackup"`, `no-store`. |
| DELETE | `/v1/pro/backups/{id}` | → 204 (a finished backup or an upload in progress). |

Limits per subscription: 7 backups and 250 MB in all. Errors (the gate's 401 / 402 come first): 400 `invalid`, 404 `not_found`, 415 `unsupported_type`, 422 `invalid_backup` (the message names the field), 503 `storage_unavailable`. **After Pro ends** the list, download and delete stay open while backups are kept (R-04: 402 only without a subscription, or a lapsed one with nothing kept); uploads need Pro. Unfinished uploads are deleted a day after their last part; everything is deleted 30 days after Pro ends (the hourly `backups::sweep`).

Data kept (R-51): per backup the id, the install that made it, size, SHA-256, app version, reason and dates, and the encrypted file. Never the key, the passphrase or anything readable from the pack.

**The dashboard** (engine routes, Settings › Your data › Cloud backups):

```ts
type CloudBackupsView = { key: { exists: boolean; saved: boolean /* confirmed before the first upload */; passphrase: boolean }
  auto: boolean; busy: { phase: "packing" | "encrypting" | "uploading" | "downloading" | "decrypting" | "restoring"; done: number; total: number } | null
  last: { at: number; ok: boolean; what: "upload" | "restore"; message: string | null } | null; last_upload_at: number | null
  uploading: { parts_done: number; parts: number } | null      // an upload that carries on after a restart
  limits: { bytes: 250000000; backups: 7; uploads_per_day: 6 }; backups: CloudBackup[] }
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/pro/backups` | → `CloudBackupsView` (the relay's errors pass through: 402, 502 `relay_unreachable`, …) |
| POST | `/api/v1/pro/backups` | Back up now, waiting for the upload → 201 `CloudBackupsView`. Always packs the data as it is now (an unfinished automatic upload is dropped, not resumed). 409 `key_not_saved` until the key is confirmed, 409 `backup_running`, 413 `backup_quota` (the encrypted pack is over 250 MB), then the relay's errors. |
| GET | `/api/v1/pro/backups/key` | → `text/plain` attachment `ferrocast-recovery-key.txt` (`no-store`): a few comment lines and the `AGE-SECRET-KEY-1…` line. Makes the key the first time. |
| POST | `/api/v1/pro/backups/key/saved` | The streamer saved the key → `CloudBackupsView` without `backups`. 409 `no_key` before there is one. Bound to that key (its public key is kept): if the stored key ever changes, `saved` is false again and uploads answer 409 `key_not_saved`. The key is made once even when several first uses arrive together. |
| PUT / DELETE | `/api/v1/pro/backups/passphrase` | `{ passphrase }` (12+ characters; 400 names the field) / none → `CloudBackupsView` without `backups`. Applies to new backups; older ones keep theirs. |
| PUT | `/api/v1/pro/backups/settings` | `{ auto: boolean }` → `CloudBackupsView` without `backups`. |
| POST | `/api/v1/pro/backups/{id}/restore` | `{ key?, passphrase? }` → `{ imported, media_files, backup }` (the import report; a local backup is taken first, then the engine reloads). Opens with this PC's key, then `key`, then `passphrase`. 422 `key_needed`, `wrong_key`, `wrong_passphrase`, `no_passphrase`, `damaged_backup`; 400 for a `key` without an `AGE-SECRET-KEY-1` line; 502 `download_damaged` (hash didn't match); 415 `damaged_pack`, 422 `pack_too_new`. A PC whose own key was never used takes the key that worked. |
| DELETE | `/api/v1/pro/backups/{id}` | → 204 |

Automatic uploads: while Pro is active (the remembered status) with the key saved and `auto` on, after the daily local backup (and on an hourly check) the pack's content hash (`store::pack::content_hash`: tables, tip CSV and media, without the export time, the remembered Pro status or the history queue) is compared with the last upload's; once a day at most, and only when it changed. Downloads have no total time limit, only a 20 s connect timeout and a 60 s idle timeout per read; each part upload has 10 minutes. An interrupted upload (crash, restart, no connection) carries on from the next part (`<data>/cloud-backup/state.json` + the ciphertext next to it). None of this ever runs without Pro.

### 9.4 Ferrocast voices (F3, Google Cloud TTS)

`crates/relay/src/tts/{mod,provider,google,fake,store,tests}.rs`, migration 6 (`tts_usage`). A `TtsProvider` (`voices()`, `synthesize(text, voice)` → Ogg Opus or `TtsError { Quota, Unavailable, BadVoice, TooLong }`): `GoogleTts` in production (REST `v1/text:synthesize`, `audioEncoding: OGG_OPUS`, the key from `GOOGLE_TTS_KEY` sent as the `X-Goog-Api-Key` header, never in a URL or a log; 6 s per call), `FakeTts` in tests. `features.tts` is true only with a provider. `quotas.tts_chars` counts characters (Unicode characters, not bytes) per subscription per UTC calendar month: 20,000.

**Money safety** (the owner must never get a bill). Before Google is called, in this order: 20 requests a minute per subscription and 60 a minute for the whole relay (429 `tts_rate`), the subscription's month (429 `tts_quota`), then the relay-wide month `TTS_GLOBAL_MONTHLY_CHARS` (default 800,000; never above 1,000,000, Google's free Neural2 tier, whatever the variable says), counted in `tts_usage` in the same statement that checks it. Once that cap would be passed every request is a 503 and Google isn't called again until the 1st. A failed call gives the subscription its characters back; the relay-wide count is given back only when Google refused before doing the work (its own 429, a bad voice, too long), never after a timeout or a 5xx (it may have been billed).

```ts
type CloudVoice = { id: string /* "google:hi-IN-Neural2-A" */; name: string /* "Hindi, female (Neural2)" */; language: string /* BCP 47 */; gender: "female" | "male"; family: "Neural2" | "WaveNet" }
```

Voices (Hindi and Indian English first): `hi-IN` Neural2 A (female) and B (male), WaveNet D (female) and C (male); `en-IN` the same four; `en-US` Neural2 F (female) and D (male).

| Method | Path | Body → Response |
|---|---|---|
| GET | `/v1/pro/tts/voices` | → `{ voices: CloudVoice[], usage: { used, limit, period: "month", reset_at }, resting: boolean /* the relay-wide month is used up */, max_chars: 300 }`, `no-store` |
| POST | `/v1/pro/tts` | `{ text, voice }` (JSON) → 200 `audio/ogg` (Opus), `no-store`, `X-Tts-Chars-Used`, `X-Tts-Chars-Limit`, `X-Quota-Reset`. Control and direction characters become spaces and the text is trimmed first. |

Errors (the gate's 401 / 402 come first): 400 `invalid` (not JSON, no `text` or `voice`), 415 `unsupported_type`, 422 `empty_text` (nothing left to read), 413 `too_long` ("Ferrocast voices read up to 300 characters at a time."), 404 `voice_not_found`, 429 `tts_rate` (+ `X-Quota-Reset`, `Retry-After` to the next minute), 429 `tts_quota` (+ `X-Quota-Reset`, `Retry-After`, the hint gives the date), 503 `tts_unavailable`: no provider ("Ferrocast voices aren't set up on this relay yet."), the relay-wide month used up ("Ferrocast voices are resting until next month.", `X-Quota-Reset` and `Retry-After` to the 1st), Google busy (`Retry-After: 60`) or down; 503 `storage`. Every 503 hint says the PC's own voice reads instead.

Data kept (R-51): the per-subscription counter in `quota_usage` and one `tts_usage` row a month (characters and requests for everyone together). Never the text, the voice picked or the audio.

**The engine** (`crates/engine/src/speech/cloud.rs`). Only a line the local rules already passed is sent (R-48: TTS on, the minimum, the word filter and length, the approval list, panic pause; a held line only once a mod approves it, a rejected one never). The audio is saved to the media library as `speech.ogg` and attached to the alert (or the approved line) exactly like the system voice's WAV. With no Pro (the remembered status), a line over 300 characters, the relay refusing, unreachable or slow (8 s for the whole call), the system voice picked in the TTS settings reads it instead, and the dashboard gets one `notice` (key `tts-cloud`, "… Reading with this PC's voice (…) for now.", action Voice settings) until a Ferrocast voice works again. The alert queue never waits for it. Settings `tts_cloud` keeps the picked Ferrocast voice (`{ voice }`); the TTS policy's `voice` stays the system voice (the backup).

```ts
type TtsCloudView = { voice: string | null; available: boolean /* Pro active and features.tts */; resting: boolean
  usage: { used: number; limit: number; reset_at: number } | null; fallback: string | null /* why the last line used the PC's voice */
  max_chars: 300; voices: CloudVoice[] }
type TtsPreview = { media_id: string; url: string /* /api/v1/media/<id> */; voice: string | null; cloud: boolean; notice: string | null }
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/tts/cloud` | → `TtsCloudView` (asks the relay for the voices and usage; the last list when it can't) |
| PUT | `/api/v1/tts/cloud` | `{ voice: string \| null }` → `TtsCloudView`. A Ferrocast voice needs Pro (402 `pro_required`) and one the relay lists (404 `voice_not_found`); `null` (back to the PC's voice) always works |
| POST | `/api/v1/tts/preview` | `{ voice? }` (default: what alerts use) → `TtsPreview`: a short sample line in the voice's language through the same path as alerts. A Ferrocast voice's preview is made once per run and reused (no characters spent twice). 422 when the computer has no voice at all |

The dashboard (Settings › Text-to-speech): one Voice picker with a "Ferrocast voices (Pro)" group (Hindi and Indian English first) above "On this computer", a Preview button, a Backup voice picker when a Ferrocast voice is picked, and the month's usage ("4,120 of 20,000 characters this month"). Not Pro but on sale: the calm line and Settings › Pro.

### 9.5 Dashboard from anywhere (F2, online proxy and offline mode)

`crates/relay/src/remote/{mod,limits,scope,store,tunnel,proxy,app,offline,tests,offline_tests}.rs`, migration 7 (`remote_devices`, `remote_codes`, `remote_audit`) and 11 (`remote_audit.source`), the cloud bot's moderation in `crates/relay/src/bot/moderate.rs`; on the PC `crates/engine/src/remote.rs` (the tunnel), `crates/server/src/remote.rs` (runs tunnelled requests through the same router) and `crates/server/src/routes/remote_scope.rs` (the role rules). Through the PC's tunnel while it's on; while it's off the relay answers a small set itself (**Offline**, M10, below). `features.remote` is true when the relay has `RELAY_REMOTE_URL` and a pepper (`RELAY_REMOTE_PEPPER`, else derived from `RELAY_VAULT_KEY`); without them every route here answers 503 `remote_unavailable` (or 404 on the phone host). Devices belong to the PC (install) that paired them; audit lines also keep the subscription.

**Roles** (the same table on the relay, the PC and in `packages/api` `remote.ts`; a server test keeps the Rust copies equal). Paths are the dashboard API's (§4) under `/api/v1`. Any path that isn't plain (`..`, `//`, an encoded `/` `\` `.` or control character) is refused for every role.

| Role | May |
|---|---|
| `owner` | Every dashboard route **except** `/accounts*`, `/session/*`, `/pro` and `/pro/*` (billing, Pro features, pairing more phones), `PUT\|DELETE /tips` (tip provider secrets), `/discord*` (the webhook), `PUT /obs` and `/obs/detect` (the OBS password), `/export`, `/import*`, `/designs/import`, `/designs/{id}/export`, `/backups/{id}/restore`, `POST /media` and `/tips/brand/{kind}` (uploads), `/updates/check`, `/updates/install` and `/updates/restart` (`/updates/*`; `GET /updates` is fine), and `/live` as a request. `PUT /settings` can't change `lan_access` (403). A new route is open to owners unless it's added to the deny list. Allowed although under `/pro` (`OWNER_ALLOWED`): `GET /pro/history/email` (§9.8), `GET /pro/history` (§9.1, M10), `GET /pro/bot` and `PUT /pro/bot/enabled` (§9.7). |
| `mod` | Only `GET /status`, `GET /chat`, `POST /mod/delete\|timeout\|ban\|unban`, `GET /mod/bans`, `GET /mod/held`, `POST /mod/held/{id}/release\|drop`, `GET\|POST /mod/panic`, `GET /viewers/{platform}/{user_id}[/note]` |

Live frames: owners get every frame; mods get `hello`, `resync`, `status`, `held`, `held_resolved`, `panic`, `chatter`, `notice` with key `panic`, and `event` only for `chat_message`, `message_deleted`, `user_messages_cleared`, `ban`. Checked on the PC and again on the relay.

```ts
type RemoteRole = "owner" | "mod"
type RemoteDevice = { id: string /* dv_ + 16 hex */; name: string; role: RemoteRole; created_at: number; last_used_at: number; expires_at: number /* 30 days after the last use */ }
type RemoteAuditLine = { device_id: string; device_name: string; action: string /* "POST /mod/held/{event_id}/drop", "PAIR device", "REVOKE device", "FORGET device" */; status: number; at: number
                         source: "pc" | "cloud" /* cloud: answered by the relay while the PC was off (M10, migration 11) */ }
type RemoteLink = { code: string /* 10 of 23456789ABCDEFGHJKMNPQRSTUVWXYZ */; url: string /* <RELAY_REMOTE_URL>/app/#pair=<code> */; expires_at: number; expires_in: number /* seconds left when answered (600): count down from receipt, not from expires_at */; role: RemoteRole; name: string }
```

**The PC's side** (install `Authorization`):

| Method | Path | Body → Response |
|---|---|---|
| POST | `/v1/pro/remote/codes` | `{ role, name? /* ≤ 40 characters; default "My phone" / "Mod" */ }` → 201 `RemoteLink`. Pro required (402). Works once, for 10 minutes; stored as a peppered hash. 409 `too_many_devices` at 5 own devices or 10 mods (unused, unexpired links count; counted and added in one statement, so two at once can't pass the limit), 429 `rate_limited` over 30 links an hour (token bucket, `Retry-After` in seconds), 400 `invalid`, 415. |
| GET | `/v1/pro/remote/devices` | → `{ devices: RemoteDevice[] /* newest first */, audit: RemoteAuditLine[] /* newest 50 */, limits: { owner: 5, mod: 10 }, pc_online }`, `no-store`. No Pro needed (a lapsed streamer can still see and remove devices). |
| DELETE | `/v1/pro/remote/devices/{id}` | → 204; the token stops working at once and its live sockets close (code 4401). 404 `not_found` (not this PC's). No Pro needed. |
| GET (WS) | `/v1/remote/tunnel` | The PC's outbound tunnel (Pro; 401 / 402 / 503 before the upgrade). One per PC, a new one replaces the old. Frames are JSON text: relay → PC `{t:"hello"\|"devices", devices, codes}`, `{t:"req", id, method, path /* "/api/v1/…?query" */, headers /* [[name, value]]: content-type, accept, range */, body_b64, device_id, role}`, `{t:"live_open", sock, device_id, role, since}`, `{t:"live_close", sock}`; PC → relay `{t:"res", id, status, headers, body_b64}` (answers up to 8 MB; at most 32 requests wait on one PC, the PC's own limit) and `{t:"live", to? /* one phone socket */, role? /* "owner": owners only */, data /* the frame as text */}`. The relay pings every 25 s and drops a tunnel silent for 75 s; Pro is checked again every 5 minutes. |

The engine opens the tunnel only while Pro (remembered status) has `features.remote` and a phone is paired or a link is waiting (settings `remote_tunnel`; a `hello`/`devices` frame with none closes it until the next link), reconnects with backoff (1 s doubling to 60 s, jitter; back to the start after a connection that held 30 s), and answers each `req` in a task of its own (never awaited in the tunnel's loop: 32 at once, else 503 `pc_busy` "Your PC is busy, try again in a moment." at once; 14 s each, else 504 `pc_timeout`; all of a connection's tasks stop with it). A new phone's first frames (`hello`, `status`, the queue, missed events) are built off the loop too, 4 at once; the PC tracks at most 64 phone sockets. A media `GET` passes `Range` to the router cut to 4 MB (`bytes=0-` becomes `bytes=0-4194303`), so a video streams in parts under the 8 MB per answer; `Content-Range` and `Accept-Ranges` come back. It answers each `req` through its own router with a `RemotePrincipal { device_id, role }` request extension instead of a session token (which only the tunnel handler can set). Its dashboard routes, never reachable from a phone (`/pro/**` is closed):

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/pro/remote` | → the relay's devices answer plus `tunnel: { state: "off" \| "connecting" \| "connected" \| "retrying", detail, since }` and `available` (Pro with the feature) |
| POST | `/api/v1/pro/remote/codes` | `{ role, name? }` → 201 `RemoteLink` (the tunnel opens so the phone finds the PC); the relay's 402 / 409 / 429 pass through |
| DELETE | `/api/v1/pro/remote/devices/{id}` | → 204; 404 `not_found` |

**The phone's side**, on `RELAY_REMOTE_URL`'s host only (404 anywhere else). A present `Origin` must be that origin (403 `forbidden`); there is no CORS at all. Every answer has `Cache-Control: no-store`, `nosniff`, `no-referrer` and `Content-Security-Policy: default-src 'none'; frame-ancestors 'none'; sandbox`.

| Method | Path | Body → Response |
|---|---|---|
| GET | `/app/`, `/app/{*path}` | The dashboard build from `RELAY_DASHBOARD_DIR` (503 page without one). `index.html` gets `<script src="/app/ferrocast-remote.js">` first, which sets `window.__FERROCAST__ = { api: "<origin>/r", remote: true }`. CSP: scripts and everything else from this origin only, images also `https:` (avatars, emotes), `connect-src 'self' wss://<host>`, no frames. Paths without an extension are the app; nothing outside the folder or starting with `.` is served. |
| POST | `/r/v1/pair` | `{ code }` (case, spaces and dashes ignored) → `{ token /* fcr_<device id>.<secret> */, device_id, role, name, expires_at }`. **Any miss is the same 404 `not_found`** (unknown, used, expired, or the PC's Pro ended). 5 tries per address (`client_ip`; IPv6 per /64), then one more every 2 minutes (token bucket, no reset at a window edge), every try counts: 429 `rate_limited` with `Retry-After` in seconds and "Wait N minutes". |
| GET | `/r/v1/me` | Bearer device token → `{ device_id, name, role, expires_at, pc_online, cloud, cloud_paused, cloud_moderation: { twitch, kick, youtube }, history }` (`cloud`: the cloud bot is switched on with a ready channel, so chat (and with the opt-in bans) work while the PC is off; `cloud_paused`: a ready channel but switched off; `cloud_moderation`: per platform, the cloud bot is on and has the moderation opt-in with every scope it needs there (Twitch `moderator:manage:banned_users` + `moderator:manage:chat_messages`, Kick `moderation:ban` + `moderation:chat_message:manage`, YouTube `youtube.force-ssl`), so the phone offers ban, time out and delete only there; `history`: an owner's phone gets History (Pro F7, while the relay has `features.history`)). 401 unpaired (the page forgets its token), 401 `device_moved` when the PC's Pro now runs on another subscription than the one the phone was paired under (hint: pair this phone again; every phone route, the live socket closes 4401 at its next check), 402 `pro_required` when the PC's Pro is paused (the device stays paired; the page says "Pro is paused" and keeps its token). |
| POST | `/r/v1/ticket` | Bearer device token, `{ for: "live" \| "media" }` → `{ ticket, expires_in }`. `live`: opens one live socket, once, within 60 s (kept hashed in memory). `media`: `mt1.<device>.<expiry>.<HMAC>`, 5 minutes, opens only `GET /media/{id}` previews (`<img>`, `<video>`, reusable for ranges; nothing stored). Counts in the device's 20 a second. 400 `invalid`, 401, 402, 429, 503 `busy`. **The 30-day device token is never put in a URL.** |
| POST | `/r/v1/forget` | Bearer device token → 204, this phone removes itself |
| ANY | `/r/api/v1/*` | Bearer device token, or for `GET /media/{id}` only a media ticket in `?ticket=` (`?session=` is refused; tickets are never passed on). In order: 401 `unauthorized` (unknown, wrong or expired token; an expired one is deleted), 402 `pro_required`, 429 `remote_rate` over 20 requests a second per device (`Retry-After`), 403 `not_allowed_for_role`, 413 `too_large` over 2 MB, without a tunnel the **offline** answers below (anything else 503 `pc_offline`), 503 `pc_busy` ("Your PC is busy, try again in a moment.", `Retry-After: 2`) over 32 waiting requests or a full tunnel queue (answered at once, never waiting for room), 504 `pc_timeout` after 15 s, 502 `too_large` for an answer over 8 MB; otherwise the PC's own answer (status and body) with only `Content-Type`, `Content-Disposition`, `Content-Range`, `Accept-Ranges`, `Retry-After` and `X-Quota-Reset` kept. `Range` is passed on. Audit: changes (not reads) with their status; refusals of changes only (a refused `GET` isn't logged), at most one line per device and route a minute; a route the PC answered 404/405 for, or a refused one not in the known list, is logged as `METHOD {unknown}`, so a phone can't write words of its choosing into the log. |
| GET (WS) | `/r/api/v1/live?ticket=<live ticket>&since=<seq>` | The dashboard's live socket (§5) through the tunnel, plus the relay's own frame `remote { pc_online, role, cloud, cloud_paused, cloud_moderation, history }` first and whenever the PC comes or goes or the cloud bot is switched (the socket stays open while the PC is off; offline frames below). Opening counts in the device's 20 a second; at most 3 sockets per device (tabs): a 4th closes the oldest with **4409** (the page stops reconnecting there: "Open in another tab"). Close codes: **4401** removed or expired (stop, forget the token, "Pair this phone"), **4402** Pro paused (stop, keep the token), **4408** fell behind (its queue of 512 filled: it's taken out at once and reconnects with `since`, then refetches). A close never waits behind queued frames (a separate channel; 2 s to send it, then the socket is dropped). |

**Offline (M10).** While a phone's PC has no tunnel the relay answers these itself (the role table above still applies first: 401, 402, 429 `remote_rate` (the device's 20 a second counts them), 403 `not_allowed_for_role`, 413), in `crates/relay/src/remote/offline.rs`:

| Method | Path | Needs | Answer |
|---|---|---|---|
| GET | `/r/api/v1/chat?limit=` | the cloud bot on | `Event[]` (§3), oldest first, up to `limit` (default 200, at most 500), from the cloud bot's in-memory ring (the last 200 lines per channel, every platform merged; never stored, R-51). The same shape as the PC's `GET /chat`: `kind.type: "chat_message"` with the mapped actor and content, the platform's message id in `source_id`, an id of its own `cl-<platform>-<message id>`, `seq: 0`, no `raw`. |
| POST | `/r/api/v1/mod/delete` | the cloud bot on | The PC's body `{ platform, message_id }` → 204. Phones of that PC get an `event` `message_deleted` frame. |
| POST | `/r/api/v1/mod/timeout` | the cloud bot on | `{ platform, user_id, seconds /* 1 s to 14 days; Kick 7 days */, reason? }` → 204 |
| POST | `/r/api/v1/mod/ban` | the cloud bot on | `{ user: { platform, user_id, login?, display_name? }, reason?, everywhere? }` → `{ banned: [platform], failed: [] }`: offline a ban is on that platform only (linked accounts live on the PC). |
| GET | `/r/api/v1/pro/history` | owner | The same answer as `GET /v1/pro/history` (§9.1) for the subscription, with its `history_rate`. |
| PUT | `/r/api/v1/pro/bot/enabled` | owner, `features.cloud_bot` | `{ enabled }` → the cloud bot's view (§9.7) plus `pro_available: true` and `sync { state: "synced" \| "off", version, synced_at, error: null }`; 422 `no_platform`, 400, 415, `bot_rate`. |

"The cloud bot on" means switched on, Pro, with a ready (bound, unblocked) channel on a platform this relay has. Moderation goes out with the cloud bot's own sign-in (Twitch: Ban User / Delete Chat Messages with the bot account's user token, refreshed from the sealed refresh token and kept in memory for an hour; Kick: `POST /public/v1/moderation/bans` and `DELETE /public/v1/chat/{id}` with its token), only with the opt-in scopes (§9.7 `moderation`; the needed one per action: Twitch `moderator:manage:banned_users` for bans and timeouts, `moderator:manage:chat_messages` for deletes; Kick `moderation:ban`, `moderation:chat_message:manage`; YouTube (M12) `youtube.force-ssl` with the opt-in: `liveChatMessages.delete` any time, `liveChatBans.insert` (permanent, or temporary with `banDurationSeconds`) only while the relay has the live chat, 409 `youtube_not_live` otherwise, each 50 units from the day's budget, 429 `youtube_quota` with `X-Quota-Reset` when spent; YouTube message ids may contain `.` and be up to 256 characters). Each action counts in the channel's 20 a minute (`bot_messages_<platform>`, shared with replies). Errors, each with a hint: 403 `needs_moderation_scope` ("The cloud bot can't moderate Twitch yet.", hint: turn on "Let the cloud bot moderate when this PC is off" on the PC's Bot page), 422 `cloud_not_connected` (no ready channel for that platform), 400 `invalid` (the field), 415, 429 `cloud_rate` (the channel's minute; `Retry-After`, `X-Quota-Reset`), 403 `bot_not_moderator` (the platform says the bot isn't a moderator there; Twitch's hint says `/mod <bot>`), 422 `platform_refused` (the platform's reason), 404 `message_not_found` / `viewer_not_found`, 409 `platform_busy`, 429 `platform_rate` (`Retry-After`), 409 `cloud_signed_out` (Twitch also marks the channel `twitch_signed_out`), 502 `platform_unreachable`. Changes answered here are audited like online ones (method + route template, status, device) with `source: "cloud"`; a 503 isn't.

Everything else offline: 503 `pc_offline` with what still works for this phone (hint: settings and everything else need the PC) while the cloud bot is on, by role and whether it can moderate anywhere (`cloud_moderation`): owner "Your PC is off. Chat, bans and history still work." / "Your PC is off. Chat and history still work.", mod "Your PC is off. Chat and bans still work." / "Your PC is off. Chat still works."; else the plain "Your streaming PC is off.".

Offline details: a validation error names the field only (`"message_id: missing."`, `"user.user_id: must be text."`, at most 200 characters, never the parser's words). `POST /mod/delete` with a Ferrocast id (`cl-…`/`cl_…`, not the platform's `source_id`) is 422 `invalid_message` (hint: send the `source_id`; the dashboard offers Delete only for lines that have one). Twitch or Kick saying the viewer is already banned or timed out is a success (200 / 204: a second phone, or a retry after a lost answer). The bot account's Twitch token is refreshed one at a time per subscription (phones moderating at once share one refresh); Twitch refusing a fresh token too marks the channel `twitch_signed_out` (409 `cloud_signed_out`).

**Offline bans reach the PC.** A ban or timeout the cloud bot made for a phone is queued for that phone's PC on its link (§7, like `bot_counter`) as kind `cloud_moderation`: `{ platform: "twitch" | "kick" | "youtube", action: "ban" | "timeout", target_id, target_login: string | null, target_name: string | null /* cleaned, ≤ 64 */, duration: number | null /* timeout seconds */, reason: string | null, by_device /* the device's name */, by_role: "owner" | "mod", at }`. Deletes and refusals aren't queued. When the PC is back the engine records it without calling the platform again: a ban joins the shared ban list (`offline: true`, `by` the mod's device name, none for the streamer's own phone; already listed: not added twice; Unban works as usual and does call the platform), a timeout counts on the viewer's card; then it acks the item (resent until acked). The ban list shows it "From your phone while the PC was off".

Live socket offline: the first frame is `remote { pc_online: false, cloud, cloud_paused }`; with the cloud on, a `resync { missed: 0 }` follows (fetch `GET /chat`), and every line the cloud bot keeps goes out as the PC's own `event` frame (`seq: 0`, so the phone's resume point stays the PC's; mods get what `MOD_FRAMES` allows). When the cloud bot is switched on or off the phones of a PC that's off get a new `remote` frame (and a `resync` when on). When the tunnel comes back, phones get `remote { pc_online: true }` and the PC is asked for each socket's frames from 0 (it answers `hello` and `resync`, so the phone refetches from the PC, and cloud lines stop); when it goes again, `remote { pc_online: false, cloud: true }` and `resync`.

Device tokens last 30 days from the last use (the expiry slides at most once a minute) and are kept only as `HMAC-SHA256(pepper, secret)`, compared in constant time; pairing codes the same way. The hourly `remote::sweep` deletes expired devices and links and audit lines older than 30 days.

**Data kept (R-51):** per device its id, PC, subscription, role, name (from the PC), token hash and three times; per unused link its hash, role, name and times (10 minutes); per change made from a phone (not reads) and per refusal the method and route template (ids never), status, device id and name, and the time, for 30 days. Request and answer bodies, queries and live frames pass through the relay's memory only and are never stored or logged.

**Offline on the phone (M10).** `/r/v1/me` with `cloud` opens the limited dashboard straight away: a warn banner "Your PC is off" with what still works for this role and capability (the relay's 503 words without "Your PC is off.", e.g. "Chat and bans still work." for a mod; one line at 375 px, only the words are a live region) (owners: Pause bot, icon only on a phone, with a confirm), screens Chat and Moderation (owners also History, as a bottom tab), everything else opens Chat. Chat shows the cloud's lines; delete (only lines with a `source_id`), time out and ban (on that platform only, and only on platforms in `cloud_moderation`; a line with nothing to offer has no toolbar or sheet) work, replies, viewer cards, the held filter, the composer ("Sending messages needs your PC.") and the events pane don't; Moderation says to moderate from chat and that the ban list, held messages, filters and panic need the PC; History is the numbers without Export. The client refuses offline whatever the relay won't answer before sending (`remoteOfflineGuard`, the same 503 words). A `remote` frame switches modes in place (the PC back: full dashboard, the PC's `resync` loads everything boot does: status, chat, events, queue, TTS waiting, session stats and a media ticket; off with the cloud: limited); the cloud bot paused while limited shows the gate. Gate "Your streaming PC is off." (bigger, bolder) and, for an owner with `cloud_paused` (set at connect from `/r/v1/me`), "Switch cloud bot on"; its 10 s look reloads on any change of state, paused ↔ off included. History in an owner phone's nav (and the route) only with `history`. The full-height Chat screen measures the sticky header (strip included) into `--fc-top` (ResizeObserver) instead of a fixed height. Desktop activity lines from the cloud read "…, while the PC was off, 3h ago".

**The dashboard.** On the PC: Settings › Dashboard anywhere (in a Pro panel): Add my phone and Invite a mod (a name first) open a QR code and the link with a countdown, and close by themselves when the phone pairs; paired devices with Owner / Mod chips and Remove; "Phones can reach this PC right now"; the latest changes from phones ("Banned someone, by Meera (mod), 3h ago"). On a phone (`window.__FERROCAST__.remote`): the device token is kept in `localStorage` (`fc-remote`); a strip on top says "Through Ferrocast's servers · PC online" or "PC offline"; screens a role can't use are hidden, not greyed (a mod sees Chat and Moderation only, without the composer, the events pane, the filters and the panic plan; an owner's phone hides Import and the Accounts, OBS, Discord, Your data, Pro and Advanced settings). Not paired: "Pair this phone"; PC off: "Your streaming PC is off.", looked at quietly every 10 s (`/r/v1/me`; the screen only changes when the answer does, so focus stays and it's announced once) with Check now; Pro paused: "Pro is paused" (token kept); too many tabs: "Open in another tab". The phone's client never makes a call its role can't (`remoteAllowed` in the client: refused locally with 403, so boot reads like `/pro`, the queue or the panic plan never reach the relay), never shows Settings › Updates' install button ("Update on the PC") or first-time setup, and gets a live ticket per connect and a media ticket for previews (refreshed every minute when under 1 minute is left). The countdown on the PC's link dialog runs from `expires_in` on receipt and the 3 s "paired yet?" poll stops at expiry.

### 9.6 The Ferrocast bot for Discord (F4)

`crates/relay/src/discord/{mod,api,perks,live,pages,interactions,store,tests}.rs`, migration 8 (`discord_guilds`, `discord_rules`, `discord_links`, `discord_grants`, `discord_posts`). REST only, no gateway: Discord's API sits behind a `DiscordApi` trait (`HttpDiscord` in production, a fake in tests). `features.discord` is true only with all four of `DISCORD_APP_ID`, `DISCORD_BOT_TOKEN`, `DISCORD_PUBLIC_KEY`, `DISCORD_CLIENT_SECRET` (missing: 503 `discord_unavailable`, the free webhook keeps working). Everything belongs to the subscription. Limits: 1 server, 10 rules, 5,000 linked members (totals, so they're in the view's `limits`, not in `quotas`); 60 requests a minute per subscription over every route (429 `discord_rate`, not a quota).

```ts
type DiscordRule = { id: string /* dr_ + 12 hex */; kind: "sub" | "sub_tier" | "member" | "tip_min"
  platform: "twitch" | "youtube" | "kick" | null /* null: any; member is always youtube; sub/sub_tier never youtube */
  tier: string | null      /* sub_tier: "1" | "2" | "3", that tier or higher (Prime = 1); member: a YouTube level name (any case) or null = any */
  min_minor: number | null; currency: string | null   /* tip_min only: ≥ 1 minor unit, 3 capitals; other currencies never count */
  role_id: string; keep_days: number /* 0 = for good, up to 366; default 31 */; created_at: number }
type DiscordProView = { connected: boolean; guild: { id; name; icon_url: string | null } | null; channel_id: string | null
  channels: { id; name }[]            /* text and announcement channels, by position */
  roles: { id; name; color: number; assignable: boolean }[]   /* highest first; @everyone and integration roles left out; assignable = below Ferrocast's own highest role */
  go_live: { enabled: boolean; message: string | null /* ≤ 300, shown above the embed */ }
  rules: DiscordRule[]; linked: number; link_url: string | null /* https://<relay>/d/<code>/link */
  limits: { guilds: 1; rules: 10; links: 5000 }; warning: string | null /* Discord unreachable, or the bot was removed */
  perk_issue: { rule_id: string; code: "missing_permission" | "role_missing" | "guild_missing"; message: string; hint: string; at: number } | null
    /* the last time a role couldn't be given; cleared when that rule gives a role again, is edited or deleted, or the server is reconnected */
  connected_by: { pc_code: string; this_pc: boolean; at: number } | null   /* which install added the bot, as its PC code */
  pc_code: string   /* this install's code ("7F3A-90C2"): HMAC of the install id with the client secret, never the id */ }
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/v1/pro/discord` | → `DiscordProView` (`no-store`; channels and roles read from Discord at most once a minute) |
| POST | `/v1/pro/discord/connect` | → 201 `{ invite_url, expires_at }`: `https://discord.com/oauth2/authorize?client_id=…&scope=bot applications.commands&permissions=268454912` (Manage Roles, Send Messages, Embed Links, View Channels) `&response_type=code&redirect_uri=<relay>/discord/callback&state=…`. The state is `<purpose>.<install>.<expires>.<nonce>.<HMAC-SHA256>` keyed with the client secret, 10 minutes. 409 `already_connected` |
| PUT | `/v1/pro/discord/channel` | `{ channel_id }` → view. 409 `not_connected`, 404 `channel_not_found` (not a text/announcement channel the bot sees) |
| PUT | `/v1/pro/discord/go-live` | `{ enabled, message? }` → view. 422 `no_channel` (on without a channel), 400 `invalid` (message over 300) |
| PUT | `/v1/pro/discord/rules` | a rule without `id`/`created_at` (they're ignored if sent; unknown fields 400) → 201 view. 422 `invalid_rule` (the message starts with the field: `min_minor: …`), 409 `too_many_rules`, 404 `role_not_found`, 403 `missing_permission` (the role is above Ferrocast's) |
| PUT / DELETE | `/v1/pro/discord/rules/{id}` | same body → view / → 204. 404 `rule_not_found`. Roles a deleted rule gave keep their end date |
| DELETE | `/v1/pro/discord` | → 204: the bot leaves the server (failures ignored) and the server, rules, links, grants and posts are deleted. Open after Pro ends (R-04): 402 only without any subscription |
| POST | `/v1/pro/discord/perk-events` | `[{ platform, platform_user_id, kind: "sub" \| "member" \| "tip", amount_minor?, currency?, tier?, at }]`, up to 100 → 200 `{ events, granted, blocked, skipped, invalid, not_linked, not_member, role_missing, too_old, guild_missing, issue: { rule_id, code } \| null }`. Each event is judged alone: a bad one is skipped and counted in `invalid`, an `at` in the future (a PC clock ahead) counts as now. Each grant stands alone: 50013 (role above Ferrocast's) counts in `blocked`, a deleted role in `role_missing`, 50001 Missing Access or an unknown server sets `guild_missing` and the rest count as `blocked` without asking Discord; the first problem is kept as the view's `perk_issue`. Only 429 `discord_rate`, 502/503 `discord_unavailable` stop the batch (send it again). 400 `invalid` (not a JSON array), 413 `too_many_events`, 409 `not_connected` |
| POST | `/v1/pro/discord/live` | `{ platform, live, name?, title?, game?, url?, thumbnail_url?, started_at, peak_viewers?, ended_at? }` → `{ posted, edited, reason: null \| "off" \| "no_channel" \| "not_connected" \| "recent" \| "resumed" \| "no_post" }`. `ended_at` (with `live: false`): when it really ended, for a queued end (clamped between the start and now). 404 `channel_not_found` (deleted in Discord). One report at a time per subscription and platform |

Discord's own answers, on every route that calls it (perk events: see above): 403 `missing_permission` (Discord 50013, or 50001 Missing Access with a "let Ferrocast see the channel, or connect again" hint; otherwise the hint "Move Ferrocast's role above the perk roles in Server Settings › Roles."), 429 `discord_rate` with Discord's `retry_after` as `Retry-After`, 404 `guild_not_found` (the bot was removed), 502 `discord_unavailable` (down, refused), 503 `discord_unavailable` (the bot token is wrong: logged for the owner).

**Connecting.** `GET /discord/callback` (the one OAuth redirect for both flows) checks the state, trades the code with the client secret and takes the server from **Discord's token answer** (never the `guild_id` in the URL, which anyone can edit), revokes the token at once, and records the server: 409 page when another subscription has it, or this one already has another (then the bot leaves the server it was just added to; never one another subscription has). An install whose Pro isn't active gets a 402 page and the bot leaves too (the code is exchanged first to learn the server). A small script-free page says "Connected to <server> for the Ferrocast PC with the code <pc_code>" and to remove the bot if Settings › Discord on their PC shows another code (someone else's install link).

**Go-live posts.** One embed in the chosen channel: title (or "<name> is live on <Platform>"), "<name> is live on <Platform>.", the game, the platform's preview image with `?t=<ms>` so Discord fetches a fresh one, the stream link as the embed URL and a "Watch on <Platform>" link button, and the optional message above. Links are kept only for twitch.tv, youtube.com, youtu.be, kick.com over https; images only from static-cdn.jtvnw.net, i.ytimg.com, images/files.kick.com. `allowed_mentions` is always empty (never pings) and nothing says Ferrocast (R-06). At the end the same message is edited: grey, "Streamed for 2 h 14 min", "Peak viewers 132", the button removed. Online again while the post is live and `started_at` is within 30 minutes of the post's: `recent`; an open post whose stream started over 30 minutes before this one (its end never arrived) or that is over a day old is closed (edited to ended, without a length) and a new one is posted; back within 30 minutes of the end: the same message shows live again (`resumed`); later: a new post. An end whose `started_at` is over 30 minutes before the open post's leaves it alone (`no_post`); a saved post that can't be read is marked ended (`no_post`), never an error. Once Discord accepted a post or a resume, the answer says so even if saving it failed (logged), so the desktop's webhook never posts a second one.

**Perks.** Each event finds the Discord users linked to `platform_user_id` in the server; every matching rule gives its role (`PUT /guilds/{g}/members/{u}/roles/{r}`, audit reason "Ferrocast perk"), and the grant is kept until `at + keep_days` (the later end wins, "for good" beats any date). Events over 30 days old, or whose role would already have ended, give nothing. A viewer who left the server (`not_member`) or a deleted role (`role_missing`) is counted, not an error. The hourly `discord::sweep` removes ended roles (500 at a time, paced, the longest-waiting first by retry time; a rate limit stops the round; any other failure backs that grant off 1 h, 2 h, 4 h … up to a day, and after 5 failures it is forgotten and the role stays; a member or role that's gone is just forgotten; a grant renewed while its role was being removed gets the role back), deletes a server's data and leaves it 30 days after Pro ended, and forgets go-live posts after 30 days.

**Viewer linking** (viewer-started; Ferrocast never posts it). `GET /d/<code>/link`: what linking does and what's kept, and a "Link with Discord" button to `/d/<code>/link/start` → 303 to Discord's sign-in with `identify connections`. The callback reads the viewer's id and their **verified** Twitch and YouTube connections, revokes the token, and replaces that viewer's links (an account linked before by someone else moves to them; none connected removes their links, which is also how to unlink). 409 page when the server already has 5,000 linked members. 20 sign-ins per address per 10 minutes (429 page). Inactive (404 page) for unknown codes and servers without Pro. Pages: no scripts, CSP `default-src 'none'; style-src 'unsafe-inline'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'`, `no-store`, `no-referrer`, every value escaped. Kick isn't a Discord connection: a `/ferrocast link` slash command is a later step.

**Interactions.** `POST /discord/interactions` checks `X-Signature-Ed25519` over `X-Signature-Timestamp` + the raw body with `DISCORD_PUBLIC_KEY` (`ed25519-dalek`, strict), and refuses timestamps more than 5 minutes off: 401 `bad_signature`. PING → `{type: 1}`; anything else (no commands are registered yet) gets a private ("ephemeral") "Ferrocast's commands aren't ready yet."

Data kept (R-51): the server's id, name and icon hash, the channel id, the go-live switch and line, a random link code, which install connected it, the last perk problem (rule id, reason, time); the rules; per linked viewer the Discord user id and the ids of their verified Twitch / YouTube accounts with the time (never names, email or tokens: the sign-in is revoked at once); per role given the user, role, rule and end (and, once ended, failed removals and the next try); the last go-live post per platform (message id, title, game, links, times) for 30 days. Deleted on disconnect, or 30 days after Pro ends.

**The engine** (`crates/engine/src/discord/pro.rs`). While Pro (remembered status) has `features.discord`: a platform going live is reported to `/live` (name, title, the category last set from Ferrocast as the game, the channel link, a Twitch preview URL); when the bot posted or edited, or answered `recent` for a stream this desktop reported, the free webhook post is skipped; otherwise (`off`, `no_channel`, `not_connected`, `recent` for a stream it didn't report, the relay down or refusing) the webhook posts as before, and a 403 also sends a `discord-pro` notice. Streams reported live are saved (`started_at`, peak, when viewers were last counted): "online" again within 30 minutes of the last sign of life is the same stream (same `started_at`); later, an end for the old one is queued first. The stream end (with `ended_at` and the peak) is queued in settings `discord_pro` (up to 20, a day at most), sent at once and every round until the relay takes it (400/403/404/409/413/415/422 drop it), and always before the next go-live. Any successful bot view also marks the bot on, so `pro_go_live` is right at once. Subs (Twitch, Kick), YouTube memberships and paid messages with a platform user id become perk events (tip-page tips have no platform account and never do), queued in settings `discord_pro` with a sequence number (up to 1,000, older than 30 days dropped) and sent 100 at a time every 30 s with backoff (30 s to 15 min; Discord's `Retry-After`); an upload removes exactly the numbers it sent (events added or dropped meanwhile are safe). A 400/422 batch is sent again one event at a time and only the refused ones are dropped; 404/409/413/415 drop the batch; 200 with `blocked` sends one notice per run; 403 (older relays) keeps them and waits 15 minutes; 401/402/429/5xx keep them.

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/pro/discord` | → `DiscordProView` (the engine remembers whether the bot posts go-live: `DiscordView.pro_go_live`) |
| POST | `/api/v1/pro/discord/connect` | → 201 `{ invite_url, expires_at }`: open it in the browser |
| PUT | `/api/v1/pro/discord/channel`, `/go-live`, `/rules` (201), `/rules/{id}` | as on the relay → `DiscordProView` |
| DELETE | `/api/v1/pro/discord/rules/{id}`, `/api/v1/pro/discord` | → 204 (a malformed rule id is 404 `rule_not_found` without asking the relay) |

The relay's status, code, message and hint pass through (403 included), plus 502 `relay_unreachable`.

The dashboard (Settings › Discord, hidden on a phone through Ferrocast's servers like the webhook form): the free webhook form as before (one quiet line when `pro_go_live`), then "Ferrocast bot" in a Pro panel: Connect server (the link is offered again after its 10 minutes; this PC's code to compare on the "connected" page), "Added from this PC, code …", channel picker, "Post when I go live" and the message (the box shows the line as saved), the rules list with an add/edit dialog (roles Ferrocast can't give are shown disabled with the hint) and the `perk_issue` line with the rule marked "Not given", "Members linked", the viewer link with Copy, Disconnect (a failure keeps the dialog open with the reason). Tip amounts use the engine's currency exponents (`money.rs`).


### 9.7 The 24/7 cloud bot (F1: Twitch, Kick, YouTube beta)

`crates/relay/src/bot/{mod,twitch,kick,youtube,lease,store,tests,youtube_tests}.rs`, migration 10 (`bot_channels` with `last_drop` and `live_since`, `bot_tokens`, `bot_config`, `bot_counter_changes` with `op`/`delta`, `bot_kv` for the Twitch conduit's id); on the PC `crates/engine/src/cloud_bot.rs`; the shared rules in `crates/core/src/cloudbot.rs`. The cloud bot answers the streamer's own commands, counters, timers and auto-replies while their PC is off, with **the same rules as the desktop bot** (`ferrocast-core`: `command::run`, `Timer::due`, `autoreply::run`, `chatbot::counter_op_for`, the same cooldowns and send budget). Built-in modules that need the PC's data (points, quotes, duels, followage, `!game`/`!title`, OBS commands) stay on the PC. YouTube is a beta (M12, **YouTube** below). Everything belongs to the subscription.

`features.cloud_bot` is true when the relay has a vault key and at least one platform set up: Twitch with all three of `TWITCH_CLOUD_CLIENT_ID`, `TWITCH_CLOUD_CLIENT_SECRET`, `TWITCH_CLOUD_EVENTSUB_SECRET` (a second, confidential Twitch app "Ferrocast Cloud"), Kick with the relay's existing `KICK_CLIENT_ID` / `KICK_CLIENT_SECRET`, YouTube with both of `YOUTUBE_CLOUD_CLIENT_ID` / `YOUTUBE_CLOUD_CLIENT_SECRET` (a Google OAuth client "Ferrocast Cloud", type Web). Each platform is listed in the view's `available`. Optional `TWITCH_CLOUD_DESKTOP_CLIENT_ID`: the desktop app's public Twitch client id (default: the one the relay was built with); a PC's `owner_token` must be issued to it (without either, any app's token for the channel is accepted, logged at start).

**Whose channel (both Twitch steps).** `connect` for Twitch carries `owner_token`, this PC's own desktop-app Twitch token (the engine adds it; never kept or logged). The relay asks Twitch (`oauth2/validate`) whose it is and which app it was issued to, and signs that account into the step's state. The channel step's callback refuses (and revokes) a sign-in by any other account, so a channel is only ever bound by the PC signed in to it. A proven owner takes a channel another subscription still has: that binding is unbound (its EventSub subscriptions deleted, its channel sign-in revoked, error `twitch_channel_taken` there). Kick's channel is always the one the install owns in `kick_owners` (checked again at the callback); a binding another subscription kept for it is removed (sign-in revoked) when its owner connects. Both sign-in pages show the PC code (`pc_code` in the view, HMAC of the install id, "7F3A-90C2") and the bot page names whose chat the account will speak in, with a warning when that isn't its own channel.

**Who answers (the lease, R-11), per platform.** A PC with the cloud bot on and its relay link open sends `{type: "bot_seen", platforms: ["twitch", "kick", "youtube"], lease: 2, youtube_live?: boolean}` on the link every 30 s (`youtube_live`: whether its YouTube reader is in a live chat, when YouTube is signed in there), naming the chat platforms it's connected to (connected or reconnecting; a plain `bot_seen` from an older PC counts for all; a frame with `platforms` but without `lease: 2`, from a PC before M12 that never names YouTube although it may answer there, counts for the platforms it names plus YouTube). While the relay has heard from one of the subscription's PCs for a platform in the last 90 s, the PC answers there and the cloud **stands by**; after 90 s of silence for that platform the cloud **takes over** it (so it covers a platform the PC isn't connected to), and the next `bot_seen` naming it hands it straight back. Each channel's `duty` says who has it; `duty.who` is `cloud` when the cloud has any. The free desktop bot never waits for the relay: it answers whatever the relay says, so a PC that comes back may overlap the cloud for up to one message. The cloud never answers one platform message twice (deduped by the platform's message id, including Twitch/Kick retries), never answers its own messages (the last 256 message ids Send Chat / Kick returned per channel, and a line from the bot account with exactly a text it sent in the last 5 s, for an echo that beats the answer or a platform without ids; the streamer's own lines are answered when the bot is their own account), never answers a Shared Chat message from a partner channel, and only posts timers while that channel is live (Twitch `stream.online/offline`, Kick `livestream.status.updated`; kept on the channel as `live_since`, read from Helix Get Streams at switch-on, Try again and the hourly sweep, so timers and `{uptime}` carry on after a relay restart). A takeover counts as each timer's last post, so a timer the PC just posted isn't posted again at the handover. While switched off or after Pro ends nothing of the chat is kept or answered (Twitch's EventSub subscriptions are deleted when it's switched off and made again when it's switched on).

**YouTube (beta, M12).** One sign-in step, the streamer's own channel (the cloud speaks as the channel, as the desktop bot does on YouTube). `connect` carries `owner_token`, the PC's desktop YouTube access token (the engine adds it from Accounts; never kept or logged): the relay asks Google's `tokeninfo` which client it was issued to (must be `YOUTUBE_CLOUD_DESKTOP_CLIENT_ID`, default the official desktop app's public id `701866487620-p0npjp4t20ivffdcn89s5jhro9vik5cl.apps.googleusercontent.com`) and `channels?mine=true` which channel it is (billed to the desktop app's project), and signs that channel id into the state (`<y|ym>.<install>.<expires>.<nonce>.<channel id>.<HMAC>`, an HKDF key from the cloud client's secret, 10 minutes, single use via `pending_oauth`). Google's consent page asks only `https://www.googleapis.com/auth/youtube.force-ssl` (reading, posting and moderating live chat all need it), `access_type=offline`, `prompt=consent select_account`. The callback (`GET /v1/bot/youtube/callback`) trades the code, checks the scope was granted and a refresh token came, reads the channel with the cloud's token (`channels?mine=true`, 1 unit) and refuses any channel but the PC's (403 page naming the PC's channel). Google's revoke (`oauth2.googleapis.com/revoke`) ends the **whole grant** of that Google account for Ferrocast Cloud, including a newer sign-in for the same channel, so: a refusal revokes the fresh grant only when no subscription has that channel bound and YouTube named the channel (otherwise it's left to expire, never stored); a sign-in replacing one for the **same** channel (the moderation opt-in, connecting again) only overwrites the stored one; one replacing **another** channel's is revoked first; a proven owner takes the channel from another subscription by dropping that binding without a revoke (the same grant). Only Disconnect, the lapse sweep and a different channel revoke. The moderation opt-in (`moderation: true`) signs in again with the same scope; it only marks that a phone may moderate. Errors: 409 `youtube_not_linked` (no token, or Google doesn't take it), 403 `wrong_youtube_app`, 409 `youtube_no_channel`, 429 `youtube_rate`, 503 `youtube_quota`, 502 `youtube_unreachable`.

*Quota.* Every YouTube call spends the Google project's units (10,000 a day, reset at **midnight Pacific time**, with daylight saving). The relay counts each call's units **before** making it, per subscription (`YOUTUBE_CLOUD_DAILY_UNITS_PER_SUB`, default the relay's budget, so one streamer can read at full speed: about 1,800 units a live hour since YouTube ends each stream after about 10 s) and relay-wide (`YOUTUBE_CLOUD_DAILY_UNITS`, default 9,000), per Pacific day in `quota_usage` (`yt_units` and `yt_units_<detect|read|send|moderate|channel>`; the relay-wide row's owner is `*relay`): live check `liveBroadcasts.list` 1, opening the chat stream (gRPC `liveChatMessages.streamList`, the desktop's own reader) 5 per opened stream, a reply `liveChatMessages.insert` 50, a delete or ban 50, the callback's channel read 1. Units are taken once the sign-in is at hand (a failed refresh costs nothing) and before the call; a call Google refuses with 401 keeps 1 unit and its retry is counted again. **Reserves:** replies, timers and moderation stop once `used + cost > limit − reserve`, per subscription (300, or a fifth of a smaller budget) and relay-wide (10% of `YOUTUBE_CLOUD_DAILY_UNITS`); reading and live checks may use the reserve. **Live checks** have their own cap: 150 units a subscription a day (or a tenth of a smaller budget) and 30% of the relay's budget (`yt_units_detect` rows, also kept for `*relay`). Over budget nothing is called: replies stop (the channel shows `youtube_quota` "YouTube's daily limit for the cloud bot is used up; it's back at 07:00 UTC." with `reset_at` and `relay_wide`, never blocking; the checks' cap says "The cloud bot has used today's YouTube live checks, so it won't notice you going live until …") while reading goes on as long as the budget allows; the notice clears on the first pass after `reset_at`. Google's own `quotaExceeded` stops every YouTube call of the relay until midnight Pacific, kept in `bot_kv` (`yt_google_out`) across restarts. Timers on YouTube: at most one timer post every 15 minutes (`YOUTUBE_CLOUD_TIMER_GAP_MIN`, 0 to 1,440, 0 = only the timers' own intervals); other due timers wait their next turn. Bring-your-own Google project is a follow-up.

*Fair share (1.4).* Google's 10,000 units are one pot for every streamer, so on a busy day nobody takes more than an even part: a subscription's limit today is `min(YOUTUBE_CLOUD_DAILY_UNITS_PER_SUB, max(600, YOUTUBE_CLOUD_DAILY_UNITS ÷ subscriptions that spent units today, itself included))`, with the reply reserve inside it. Alone or with a few others nothing changes; with ten streamers each gets 900, never below 600 (a long stream's checks and chat and a few replies). The view's `quota` gains `limit` (today's), `fair_share: true` when it's cut, and `sharing_today`.

**YouTube from the web, without a PC (1.4).** A signed-in **Pro account** connects its channel on the web dashboard (the Account page) and the cloud bot reads and answers its live chat from the relay with no Ferrocast PC at all (`crates/relay/src/bot/youtube/web.rs`). The channel is bound to the **account's Pro subscription** (the same key its PCs use: one cloud bot, one config, one YouTube budget per account). The bot's commands, timers and auto-replies are the config the account's PCs last pushed to the cloud bot (`PUT /v1/pro/bot/config`); an account that never had a PC answers nothing until cloud mode's Bot page can edit it (docs/CLOUD.md §11). Web host, cookie session (ACCOUNTS.md §1):

| Method | Path | Body → Response |
|---|---|---|
| GET | `/r/v1/pro/youtube` | → `{ available, pro, bot_on, channel: { id, title, moderation, error, live, from_web, connected_at } \| null, quota /* as above */ \| null }` |
| POST | `/r/v1/pro/youtube/connect` | CSRF. `{ moderation? }` → `{ flow: "browser", url }`: Google's consent page (the same client, scope and redirect as the PC flow), a signed single-use state naming the account (owner `web`). 402 `pro_required` (hint: Twitch and Kick are free in cloud mode, YouTube keeps working in the app), 503 `platform_unavailable` (no `YOUTUBE_CLOUD_*`) / `vault_unavailable`, 429 `rate_limited` (20 an hour) |
| — | `/v1/bot/youtube/callback` (API host) | Trades the code, reads the channel (1 unit), checks the scope and the refresh token, seals the sign-in for the account under a one-time code (10 minutes) and sends the browser on (303) to the web host's finish. Every refusal gives the fresh grant back to Google unless a binding uses that channel |
| GET | `/r/v1/pro/youtube/finish?code=` | Binds **only for the same signed-in account** that still has Pro (the web cookie the API host can't see) → 303 `/app/#/account?youtube=connected`. Another account's or a signed-out browser: 403 page "Nothing was connected", the grant revoked. 410 used or expired, 402 Pro ended meanwhile |
| PUT | `/r/v1/pro/youtube/bot` | CSRF. `{ enabled }` → the view; 422 `no_platform` without a ready channel, 422 `invalid` without `enabled` |
| DELETE | `/r/v1/pro/youtube` | CSRF → the view: unbound and revoked at Google (open after Pro ends, R-04) |

*Only while it matters.* Nothing is asked of YouTube unless the cloud bot is on, Pro is active and the cloud holds YouTube's lease (the PC quiet for 90 s on YouTube). Then it looks for a live broadcast at once (or after 2 minutes when the PC's last `bot_seen` said `youtube_live: false`); while offline it slows down: every 2 minutes for the first half hour after the takeover or the last time it was live (or while the PC last saw it live), every 5 until 2 hours, every 15 until 6 hours, then hourly (about 65 units for a day without a stream). Free signals wake it at once (at most once per 30 s after the last look): the PC's `youtube_live` turning true, the same subscription going live on Twitch or Kick, and "Check now" on the Bot page (`connect {retry: true}`). These checks run in their own loop, at most 8 channels at once, so a slow Google call never holds up a Twitch or Kick timer. Live: the stream is opened (from the last page token, so nothing is read twice) and each line goes through the same path as Twitch/Kick (dedupe by message id, the in-memory ring, answers, counters, timers while live). A stream YouTube closes (it does after about 10 s, even while live) is opened again at once from the page token, so a line is answered in about a second; only a stream that closes within 2 s without a chat line waits, 1 s doubling for each such close in a row to at most 30 s; the chat ending (`offlineAt`, `liveChatEnded`) marks it offline. The gRPC connection pings every 30 s (HTTP/2 keepalive, 10 s to answer, also while idle) and a stream silent for 90 s is opened again from the page token (a second silence in a row counts as a failure). Failed opens wait 30 s doubling (at most an hour); the streak is kept across reader restarts, and after 6 in a row reading stops with `youtube_unavailable` (retryable, "Try again" starts over) until a live check at least an hour later finds it still live; the next good batch clears the error. The PC saying `bot_seen` for YouTube again stops the reader at the next pass (20 s). The sign-in is refreshed hourly (no quota); one Google refuses marks `youtube_signed_out` (blocking). The view's YouTube channel has `beta: true` and `quota: { used, limit, reset_at, exhausted /* no room for one more reply */, by_call }`; `limits.youtube_daily_units`.

**Counters.** A counter the cloud changes (a mod's `!deaths +` while the PC is off) is kept as a `bot_counter_changes` row and sent to each of the subscription's PCs as link item `bot_counter { id, name, op: "add" | "set", delta, value, at }`. The PC applies the op (`add`: its own value plus `delta`, so a change made on the PC meanwhile stays; `set`: `value`; an item without `op` sets) and reports the highest `id` it applied as `counters_seen` with its next config push; a push never overwrites a newer cloud change (the relay applies changes above `counters_seen` to the pushed values in order) and rows at or below it are deleted.

```ts
type CloudBotPlatform = "twitch" | "kick" | "youtube"
type CloudBotChannel = {
  platform: CloudBotPlatform
  state: "needs_channel" /* Twitch: the bot account is in, the channel hasn't allowed it yet */ | "ready" | "error"
  bot: { id: string; login: string } | null        // the account that speaks
  channel: { id: string; login: string } | null    // whose chat it is
  moderation: boolean                              // moderator scopes were granted (opt-in): a phone's bans, timeouts and deletes work while the PC is off (§9.5 Offline)
  connected_at: number; last_reply_at: number | null
  error: { code: string; message: string; hint: string; at: number; retry: boolean } | null
    // only `*_signed_out` and `twitch_revoked` stop the channel (state "error", connect again); the rest
    // (`twitch_unavailable`, `twitch_not_allowed`, `<platform>_refused`, `twitch_channel_taken`) show with
    // `retry: true` when bound ("Try again"), and a message that goes out clears them
  last_drop: { reason: string; at: number } | null  // the platform took a reply but didn't show it (AutoMod, slow mode, Kick 400): noted, never an error
  live: boolean                                    // the channel is live
  duty: "off" | "desktop" | "cloud"                // who answers this channel now
  beta?: true                                      // YouTube
  quota?: { used: number; limit: number; reset_at: number /* midnight Pacific */; exhausted: boolean
            by_call: { detect: number; read: number; send: number; moderate: number; channel: number } }  // YouTube's units today
}
type CloudBotView = {
  enabled: boolean
  duty: { who: "off" | "desktop" | "cloud"; since: number | null; desktop_seen_at: number | null
          this_pc: boolean /* the last bot_seen came from the asking install */ }
  pc_code: string                                   // this install's code ("7F3A-90C2"), as on the sign-in pages
  version: number; config_updated_at: number | null
  counts: { commands: number; timers: number; counters: number; auto_replies: number }
  channels: CloudBotChannel[]                       // at most one per platform
  available: { twitch: boolean; kick: boolean; youtube: boolean }
  limits: { commands: 500; timers: 50; config_bytes: 262144; messages_per_minute: 20; channels_per_platform: 1; takeover_after_s: 90
            youtube_daily_units: number | null }
}
type CloudBotConfig = {   // what the PC pushes; the desktop's own shapes (§4 Bot, Studio counters, Auto-replies)
  bot_on: boolean                                   // the desktop's bot switch: off means the cloud is quiet too
  commands: Command[]; timers: Timer[]; auto_replies: AutoReply[]
  counters: { name: string; command: string | null; reply: string; enabled: boolean; value: number }[]
}
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/v1/pro/bot` | → `CloudBotView` (`no-store`) |
| PUT | `/v1/pro/bot/config` | `{ version /* the version this PC last saw, 0 at first */, config: CloudBotConfig, counters_seen? }` → `{ version /* version + 1 */, updated_at }`. 409 `stale_version` when another PC pushed since ("The bot settings changed on another PC."; GET for the current version, then push again), 413 `too_large` over 256 KB, 413 `too_many_commands` over 500, 413 `too_many_timers` over 50, 422 `invalid_config` (the message names the item: `commands[3]: …`), 415 `unsupported_type`, 400 `invalid` |
| POST | `/v1/pro/bot/{platform}/connect` | `{ account?: "bot" \| "channel", moderation?: boolean, owner_token?: string, retry?: boolean }` → 201 `{ auth_url, expires_at }` (open it in the browser; 10 minutes). Twitch needs `owner_token` (above): 409 `twitch_not_linked` without one or when Twitch doesn't take it ("Connect Twitch in Accounts on this PC first."), 403 `wrong_twitch_app` (another app's token), 502 `twitch_unreachable`, 429 `twitch_rate`. `{retry: true}` tries a bound channel's error again (Twitch: subscribes it again and reads the live status; Kick: clears the error; YouTube: clears a retryable error, resets the chat stream's failure streak and looks for a live broadcast on the next pass, at most once per 30 s after the last look, which is also "Check now") → 200 `CloudBotView`; 409 `not_connected`, 409 `connect_again` for a blocking error. Twitch has two steps: `bot` (default) signs in the account that will speak with `user:bot user:read:chat user:write:chat` (plus `moderator:manage:banned_users moderator:manage:chat_messages` only with `moderation: true`), `channel` signs in the streamer's own account with `channel:bot` (the same account twice is fine). Kick: one step with `user:read chat:write` (plus `moderation:ban moderation:chat_message:manage` with `moderation`), through the relay's Kick sign-in (`/v1/kick/callback`); it needs Kick connected in Accounts first (409 `kick_not_linked`), because Kick's events reach the cloud through that connection. 404 `platform_not_found`, 409 `already_connected` (disconnect first to use another account; the one exception is the moderation opt-in: `{ account: "bot", moderation: true }` over a channel without it signs the bot account in again with the moderator scopes and keeps the channel), 409 `bot_account_first` (Twitch `channel` before `bot`), 400 `invalid` (unknown `account` or fields), YouTube: one step (`account` may be omitted, `bot` or `channel`), `owner_token` required as above. 503 `platform_unavailable` (not set up on this relay), 503 `vault_unavailable` |
| DELETE | `/v1/pro/bot/{platform}` | → 204: the platform's sign-ins are revoked there (failures ignored), Twitch's EventSub subscriptions deleted, the rows deleted. With no channel left the cloud bot switches off. Open after Pro ends (R-04): 402 only without any subscription. 404 `platform_not_found` |
| PUT | `/v1/pro/bot/enabled` | `{ enabled }` → `CloudBotView`. 422 `no_platform` ("Connect Twitch, Kick or YouTube for the cloud bot first.") to switch on without a ready channel |
| GET | `/v1/bot/twitch/callback` | Twitch's OAuth redirect (state `<purpose>.<install>.<expires>.<nonce>.<owner>.<HMAC-SHA256>` keyed with an HKDF key derived from the cloud app's secret for this label, 10 minutes, single use: the nonce is kept in `pending_oauth` and taken here; the account comes from Twitch's own token check, never the URL). Script-free pages: connected (with the account's name, whose chat, the PC code and what's next), cancelled, expired or already used (400), 402, 403 (the channel step signed in by another account than the PC's). Every refusal after the code is traded revokes that sign-in; a sign-in a step replaces is revoked first |
| GET | `/v1/bot/youtube/callback` | Google's OAuth redirect (YouTube, above): script-free pages like Twitch's: connected (the channel, the PC code, "remove Ferrocast Cloud at myaccount.google.com/permissions" if it isn't yours), cancelled, expired or already used (400), missing scope (400), no lasting sign-in (502), 402, 403 (another channel than the PC's), 409 (no YouTube channel) |
| POST | `/v1/bot/twitch/eventsub` | EventSub's Conduit webhook shard. Checks `Twitch-Eventsub-Message-Signature` = `sha256=` HMAC-SHA256(`TWITCH_CLOUD_EVENTSUB_SECRET`, message id + timestamp + raw body) in constant time (403 `bad_signature`), refuses a timestamp more than 10 minutes off (403 `stale_message`), answers a message id seen in the last day with 204 and does nothing (replays and retries; one `INSERT OR IGNORE`). A bad signature is logged as a warning once a minute, debug after. `webhook_callback_verification` → 200 `text/plain` with the challenge; `revocation` → 204: `authorization_revoked` / `user_removed` unbind the channel (subscriptions deleted, channel sign-in revoked and dropped, error `twitch_revoked`, "Allow it in your channel" again), other reasons show `twitch_unavailable`; `notification` → 204 at once, the work runs after in a bounded queue (1,024 waiting, 16 at a time; more is dropped and counted, as for Kick's webhooks) |

Errors (the gate's 401 / 402 come first): as listed, plus 429 `bot_rate` (120 requests a minute per subscription over these routes, `Retry-After`; not a quota), 503 `storage`. **Limits:** one channel per platform, 20 messages a minute per channel (over it the cloud stays quiet until the minute ends; auto-replies also keep the desktop's own budget, 6 per 30 s), 50 timers, 500 commands.

Data kept (R-51): per platform the bot account's and channel's id and login, the granted scopes, Twitch's EventSub subscription ids, when it connected and last replied, the last error, the last dropped reply's reason and time, when the channel went live (and the relay's Twitch conduit id); the sign-ins' **refresh tokens, encrypted** in the vault (AAD `<subscription>|bot_<platform>_<bot|channel>`, never logged, revoked on disconnect); the pushed config (commands, timers, auto-replies, counter names and values) and its version; counter changes until a PC took them. For YouTube: the channel's id and title, the granted scope, its sealed refresh token, the units counted per Pacific day (62 days, with the other quota rows); the access token, the live chat id and the stream's page token live in memory only. **Chat is never stored**: the cloud keeps the last 200 lines per channel in memory only (for the offline dashboard, M10) and forgets them on restart. Everything goes on disconnect, or 30 days after Pro ends (the hourly `bot::sweep`, which also checks each Twitch sign-in once an hour and marks a revoked one `twitch_signed_out`, subscribes again channels marked `twitch_unavailable` or with no subscriptions, reads the live status, deletes Twitch subscriptions of switched-off or lapsed subscriptions, and points the conduit's shard here: it's GET first and PATCHed only when not enabled or pointed elsewhere; the conduit id is kept in `bot_kv`, and when Twitch hands back another one every bound Twitch channel is subscribed again on it). EventSub subscription ids are saved as each is made; one that already exists (409) is looked up (`GET /eventsub/subscriptions?user_id=`). Kick refreshes run one at a time per subscription (cache re-checked under the lock, the new refresh token sealed before it goes; a refused refresh is retried once when the sealed token changed meanwhile, else `kick_signed_out`).

**The engine** (`crates/engine/src/cloud_bot.rs`). While Pro (remembered status) has `features.cloud_bot` and the streamer switched the cloud bot on: every second it compares a hash of the bot's config (commands, timers, auto-replies, counter definitions, the bot switch; counter values are sent but not hashed, so a counting stream doesn't push every second); a change that stays put for 2 s is pushed with the last seen version; failures back off (2 s doubling to 5 min, `Retry-After` honoured); a 409 `stale_version` reads the relay's version and pushes again once (this PC's change is newer); 413/415/422 stop pushing that config until it changes and show the error. It keeps the relay link open and sends `bot_seen` with its connected chat platforms (and `lease: 2`) every 30 s, applies `bot_counter` items once (the op: adds the delta or sets, remembers the id), reads the relay's switch every 5 minutes while Pro has the feature (so a second PC that never opened the panel still says `bot_seen` and never answers alongside the cloud), and keeps `{ enabled, version, pushed_hash, synced_at, counters_seen, error }` in settings `cloud_bot`.

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/pro/bot` | → `CloudBotView` plus `pro_available` (Pro with the feature, from the remembered status) and `sync: { state: "off" \| "pending" \| "synced" \| "error"; version; synced_at: number \| null; error: { code, message, hint } \| null }` |
| PUT | `/api/v1/pro/bot/enabled` | `{ enabled }` → the same. Switching on pushes the config first |
| POST | `/api/v1/pro/bot/{platform}/connect` | `{ account?, moderation? }` → 201 `{ auth_url, expires_at }`; Twitch and YouTube get this PC's own token added as `owner_token` (one in the request is dropped). `{ retry: true }` → 200 the same view as GET (Try again) |
| DELETE | `/api/v1/pro/bot/{platform}` | → 204 |

The relay's status, code, message and hint pass through, plus 502 `relay_unreachable`. A paired phone (§9.5): an owner may `GET /pro/bot` and `PUT /pro/bot/enabled` (look and switch it), nothing else under `/pro`; a mod nothing.

The dashboard (Bot page, a Pro panel "Keep the bot on when this PC is off"): per platform Connect (Twitch: the bot account, then "Allow it in your channel"), the account names, Disconnect; the on/off switch (off until a channel is ready); the status line ("Standing by: this PC is answering", "Another of your PCs is answering", "Your PC is answering" on a phone, "On duty since 21:40" with which platforms each answers when split, "Off"), read again every 30 s while visible, the last reply time, a dropped reply ("A reply wasn't shown at 21:40: …"), the sync state, each error with its hint and "Try again" when `retry`, and this PC's code. YouTube's row is marked "Beta", shows "Uses about 212 of 9,000 YouTube units today" and, when the budget is spent, "YouTube's daily limit for the cloud bot is used up; it's back at <time>" in the viewer's own clock (from `reset_at`); while the cloud is on duty for YouTube and it isn't live, "Check now" (`connect {retry: true}`) with "While you're offline, Ferrocast looks less often. Going live on Twitch or Kick wakes it."; a relay without YouTube in `available` shows no YouTube row. The split status line lists platforms as "Twitch, Kick and YouTube". Per ready channel: "Let the cloud bot moderate when this PC is off" (Turn on: `connect { account: "bot", moderation: true }`, the sign-in link as for Connect; On: the bot account's name; a phone only shows it). Not Pro but on sale: the calm line and Settings › Pro.

### 9.8 The weekly email (F7)

`crates/relay/src/mail/{mod,mailer,brevo,fake,report,pages,weekly,store,tests}.rs`, migration 9 (`mail_addresses`, `mail_sent`, `mail_budget`). A short report of the week's streams, built from the stored history (§9.1) and sent through Brevo's transactional API behind a `Mailer` trait (`BrevoMailer`: `POST https://api.brevo.com/v3/smtp/email`, the key from `BREVO_API_KEY` in the `api-key` header, never in a URL or a log; `FakeMailer` in tests). Sender `MAIL_FROM` (default `Ferrocast <reports@mail.clearcraftlabs.com>`, the domain authenticated in Brevo). `features.weekly_email` is true with `BREVO_API_KEY`, the vault key and a link key (`MAIL_TOKEN_KEY`, else derived from `RELAY_VAULT_KEY`); otherwise every route answers 503 `mail_unavailable`. One confirmed address per subscription, plus at most one new one waiting for its confirm (the confirmed one keeps getting the report until then). At most `MAIL_DAILY_BUDGET` emails a UTC day relay-wide (default 270, under Brevo's free 300), confirms and reports together.

```ts
type WeeklyEmailView = { status: "none" | "pending" | "on" | "paused"   // on/paused: the confirmed address; pending: only a new one
  address_hint: string | null          // "a•••@gmail.com" (first character, the domain) of the address `status` is about; null if unreadable
  pending_hint: string | null          // a new address waiting for its confirm, also next to "on"
  expires_at: number | null            // when the pending address's confirm link stops working
  day: "Monday" | … | "Sunday"         // this subscription's day
  next_at: number | null               // on: the next report moment (04:00 UTC on `day`)
  last_sent_at: number | null
  paused_reason: "rejected" | "unreadable" | null }   // paused only: Brevo refused the recipient, or only this address didn't open
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/v1/pro/history/email` | → `WeeklyEmailView`, `no-store` |
| PUT | `/v1/pro/history/email` | `{ address }` → 202 `WeeklyEmailView` (`pending`). Sends the confirm email first and saves the address (sealed) only once it went, so a failure never replaces a working address. The same address while `on`: 409 `already_on`. A new address waits next to the confirmed one, which keeps getting the report until the new one's link is pressed (then they swap); a paused address is dropped (saving again starts over). Saving again while pending is Resend: the newer link replaces the older. |
| DELETE | `/v1/pro/history/email` | → 204, both addresses deleted (confirmed and pending). Open after Pro ends: 402 only without any subscription |

Errors (the gate's 401 / 402 come first): 400 `invalid` (not JSON, no `address`), 415 `unsupported_type`, 422 `invalid_email` ("That email address doesn't look right." or, when Brevo refused it, "That address can't get email from us."; hint: check for typos), 409 `already_on`, 429 `mail_rate` (3 confirm emails per subscription per UTC day, resends included; failed sends don't count; `X-Quota-Reset`, `Retry-After`, hint: look in spam), 502 `mail_failed` (Brevo down, or its answer lost: "The confirm email couldn't be sent right now.", hint: try again in a few minutes), 503 `mail_busy` (the relay's daily budget used up, or Brevo said its daily limit is reached: "Ferrocast has sent all the email it can for today.", hint "Try again tomorrow."; nothing counted against the three), 503 `mail_unavailable` (not set up here, or Brevo refused the relay's own key or request), 503 `vault_unavailable`, 503 `storage`.

Public pages (no scripts, CSP `default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'`, `no-store`, `no-referrer`, `nosniff`, every value escaped). **A GET never changes anything** (mail scanners and link previews fetch links); only the page's button POSTs.

| Path | What |
|---|---|
| `GET /mail/confirm?t=…` | The confirm link (`c1.<sub>.<expires>.<nonce>.<HMAC-SHA256>`, 24 hours, single use: only the nonce's SHA-256 is kept and a newer link replaces it). A page with a Confirm button (POST); 404 page for a tampered, used or replaced link, 410 once expired ("press Resend"), "You're all set" when it's on already |
| `POST /mail/confirm?t=…` | Turns the address on: 200 "You're in" with the day; 402 page when the subscription has no Pro at that moment |
| `GET /mail/unsubscribe?t=…` | The link in every report (`u1.<sub>.<address tag>.<HMAC>`, no expiry). The tag is HMAC(link key, the normalized, lowercased address), so saving the same address again keeps old links working and a different address never matches. A page with a Stop emails button; for an address no longer saved: "This link is out of date — use the newest email or stop emails in the dashboard." |
| `POST /mail/unsubscribe?t=…` | RFC 8058 one-click (`List-Unsubscribe: <…>`, `List-Unsubscribe-Post: List-Unsubscribe=One-Click` on every report), and the page's button: the address the link was sent to is deleted (confirmed or pending, matched by tag), nothing else. Any body; 200 with the out-of-date page when nothing matched; 404 for a bad link |

**The report.** Each subscription's day is `SHA-256(sub_id) % 7` (0 = Monday; stable across restarts, unlike a seeded hasher), at 04:00 UTC (09:30 in India). The hourly job sends it once that moment has passed (up to 2 days late after an outage, never later: a Sunday moment missed until Monday still goes, in last week's slot), to `on` addresses confirmed before that moment, while the subscription has Pro. It covers the 7 days before the moment: hours live, average viewers (weighted by length), peak viewers, streams, new followers and subs (when there were any), tips per currency (never converted), the best stream (most average viewers: title, platform, date, length) and each number against the 7 days before ("up 12% on last week"; "nothing to compare yet" without streams then). **A week without a stream sends nothing** (looked at again until 2 days after the moment, so a stream uploaded late still gets its report; final after that). HTML with tables and inline styles only (one column, light, solid colours that still read when Gmail's dark mode inverts them, `lang`, `role="presentation"`), plus a plain-text part. Stream titles are escaped. `mail_sent (sub_id, week)` is claimed before the send and the address read again after the claim (one stopped or changed meanwhile is left alone), so a restart or a second relay process never sends a week twice. Brevo's answers: no connection or 503 (not accepted) gives the claim back for the next hour; a read timeout or another 5xx keeps it (it may have gone: counts as sent, never twice); a 400 clearly about the recipient (`invalid_parameter`/`missing_parameter` naming `to` or the recipient, or a blocked/blacklisted contact) pauses that address (`paused`, `rejected`; saving again starts over), but two different addresses refused one after the other stop the run and the second isn't paused; any other 400, 401, 403, 404 (our account or request) stops the run, gives the claim back, logged; 402, a 403 about credits or a limit, and 429 stop for the day (claims given back, confirms answer `mail_busy`), the next day carries on inside the 2-day window. The daily budget works the same way: the run stops at it and gives the claim back. An address the vault can't open is never paused while nothing opens (wrong key: the claim is given back, logged, the run stops); only one that alone fails while others open is paused `unreadable`.

Data kept (R-51): the addresses only sealed with the vault (AES-256-GCM, AAD `sub_id|mail_address`) plus each one's tag (a keyed HMAC, for unsubscribe links), never in plain text, never logged (`Email`'s `Debug` and every error leave it out; Brevo's own error message is dropped); its status, when it was saved/confirmed, the confirm link's nonce hash and expiry; per (subscription, week) when the report went (120 days); emails sent per UTC day (7 days). Deleted on Stop emails or unsubscribe, a week after an unconfirmed link expired (only the pending address: a confirmed one next to it stays), and 30 days after Pro ended (the hourly sweep). Brevo keeps its own delivery logs (its privacy terms).

**The engine** (`crates/engine/src/weekly_email.rs`, `crates/server/src/routes/weekly_email.rs`) passes each call through:

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/pro/history/email` | → `WeeklyEmailView` |
| PUT | `/api/v1/pro/history/email` | `{ address }` → 202 `WeeklyEmailView` (400 without a text `address`) |
| DELETE | `/api/v1/pro/history/email` | → 204 |

The relay's status, code, message and hint pass through (429 with `Retry-After`, 503 `mail_busy`), plus 502 `relay_unreachable`. A paired phone (§9.5): an owner may `GET` it (`OWNER_ALLOWED`); changing the address stays on the PC; a mod nothing.

The dashboard (`#/history`, at the end of the history panel, shown when `features.weekly_email`): the day and time in the viewer's own time zone (from `next_at`, or the next 04:00 UTC on `day`); no address: "Your week, in your inbox", the address field and Save (a typo is caught before sending); pending: "Check your inbox", the hint, until when the link works (or that it expired), Resend (same session) and Change address, and it looks again every 10 s while visible; on: "Weekly email is on", "Every Sunday to a•••@gmail.com", the next date, Change address and Stop emails, plus "Waiting for n•••@… to be confirmed. Until then, reports keep going to a•••@…" with a new address pending; paused: "Emails paused" saying why (refused by the mail server, or "couldn't read the saved address") with Use another address / Save your address again. A hint the relay couldn't read shows as "your address". Errors show inline with their hint.

### 9.9 The Control Center's stats (owner only)

`crates/relay/src/admin.rs`. The owner's admin panel (ops.clearcraftlabs.com, the separate Control Center repo) reads one route, server to server. **Aggregate counts only**: never an install id, subscription id, email or its hash, handle, server or channel name, token, or any tip amount.

| Method | Path | Response |
|---|---|---|
| GET | `/admin/v1/stats` | `Authorization: Bearer <RELAY_ADMIN_TOKEN>` → `AdminStats`, `Cache-Control: no-store` |

```ts
interface AdminStats {
  relay: { version: string; started_at: number /* ms */; uptime_seconds: number; now: number }
  billing: { mode: "live" | "test" | null; sales_open: boolean }
  features: Features                      // as /v1/pro/status features
  installs: { total: number; active_7d: number; active_30d: number; new_7d: number; new_30d: number }
  pro: {
    active: number; monthly: number; yearly: number; other: number  // paying subscriptions with Pro now
    cancelling: number                    // of those, set to stop at the period end
    currency: "INR"; mrr_paise: number    // monthly × ₹199 + yearly × ₹1,999 / 12, rounded once
    prices: { monthly: number; yearly: number }
    new_this_month: number                // first charge in this UTC month
    ended_this_month: number              // paid once, Pro stopped this UTC month (cancelled, refunded, renewals failed)
    installs: number                      // installs on those subscriptions
    comp: { active: number; installs: number }  // owner's test grants (pro-grant.sh), never in the numbers above
  }
  usage: {                                // only subscriptions with Pro now (test grants included)
    tts: { month: string /* "2026-10" */; chars: number; requests: number; cap: number }
    cloud_bot: { on: number; channels: number; twitch: number; kick: number; youtube: number }
    discord: { guilds: number }
    weekly_email: { opted_in: number }    // addresses confirmed and on
    cloud_backups: { subscriptions: number }  // with a finished backup
    remote: { devices: number }           // paired phones not expired
    tips: { pages: number; branded: number; goals: number; goal_tips: number }  // counts, never amounts
    smart: { provider: "cloudflare" | "gemini" | null; calls_today: number; daily_cap: number; resting: boolean; resets_at: number }
    history: { free_accounts: number; pro_subscriptions: number }   // with stream history kept
    // Relay-wide, not only Pro: every subscription's calls and emails together.
    youtube: { units_today: number; daily_cap: number | null; resets_at: number }  // YouTube Data API units the cloud bot used today (quota_usage yt_units, Pacific day, Google's); cap YOUTUBE_CLOUD_DAILY_UNITS (default 9,000), null while YouTube from the cloud is off
    mail: { sent_today: number; daily_budget: number; resets_at: number }          // emails sent through Brevo today (UTC; confirms and weekly reports); budget MAIL_DAILY_BUDGET (default 270, Brevo's free plan is 300)
  }
  backup: { last_ok_at: string | null }   // the nightly R2 backup's last good run (UTC), from /data/backup-last-ok
}
```

Active installs: seen (any signed request) in the last 7 or 30 days, after the day they signed up. Pro uses the same rule as `/v1/pro/status` `pro` (§9).

Errors, in the order they're checked:

| Status | Code | When |
|---|---|---|
| 403 | `internal_only` | The request came through Cloudflare or another proxy (`CF-Connecting-IP`, `CF-Ray`, `CDN-Loop`, `X-Forwarded-For`, `Forwarded`), or from an address outside the server: the route answers only on the relay's Docker network (`http://ferrocast-relay:8787`). Doesn't count against the limit |
| 429 | `rate_limited` | More than 60 requests in a minute, relay-wide. `Retry-After` (seconds) |
| 503 | `admin_off` | `RELAY_ADMIN_TOKEN` isn't set, or is shorter than 32 characters; the hint names `control-center-key.sh` |
| 401 | `unauthorized` | No bearer token, or the wrong one (SHA-256 of both compared in constant time). `WWW-Authenticate: Bearer` |
| 503 | `storage` | The database couldn't be read |

The token is made on the server by `deploy/control-center-key.sh` (deploy/README.md), which gives the Control Center the same value.

### 9.10 Smart features (Pro, 1.4)

`crates/relay/src/smart/{mod,prompts,provider,tests}.rs`; on the PC `crates/engine/src/smart.rs` (§4 Smart features). The PC sends text; the relay asks a language model on a **free tier** and checks its answer; the PC decides what happens in chat. Never called "AI" in the product. **Off until the owner sets a key** (`deploy/pro-keys.sh --ai`): without one `features.smart` is false and every route answers 503 `smart_off`.

**The model** (`provider.rs`): **Cloudflare Workers AI** when `CLOUDFLARE_AI_ACCOUNT_ID` and `CLOUDFLARE_AI_TOKEN` are set (model `SMART_MODEL`, default `@cf/meta/llama-3.1-8b-instruct`; free 10,000 Neurons a day, and on the Workers Free plan it stops instead of billing; Cloudflare doesn't train on what it's sent), else **Google Gemini** with `GEMINI_API_KEY` (`SMART_GEMINI_MODEL`, default `gemini-3.5-flash-lite`; the key must come from a Google project **without billing**, and on the free tier Google may use prompts to improve its products, so viewer chat would go to Google: Cloudflare is preferred). **With both set, Gemini is the backup**: once Cloudflare rests (its free day used up, or the relay's `SMART_DAILY_CALLS`) or fails, the call goes to Gemini, which has its own day (`SMART_BACKUP_DAILY_CALLS`, default 500, at most 1,000; counted as `smart_backup_calls`). `smart_resting` only when both rest; a backup that's down answers 502 `smart_unavailable`. `GET /v1/pro/smart` `provider` names the one answering now; the admin view adds `backup`, `backup_calls_today` and `backup_daily_cap`. Keys go in headers, never in a URL, a log or `Debug`. Nothing of the messages, facts, questions or answers is stored or logged (R-51); only counters.

**Caps, checked in this order before the model is asked:** the gate (401, 402 `pro_required`, 503 `smart_off`), the body (415, 400, 413, 422), 10 calls a minute per subscription (429 `smart_rate`), the subscription's day (429 `smart_quota` with `X-Quota-Reset` / `Retry-After` and the date: `smart_moderation` 30 checks, `smart_replies` 20, `smart_recaps` 3, listed in `/v1/pro/status` `quotas`), the relay-wide minute (12, 429 `smart_rate`) and day (`SMART_DAILY_CALLS`, default 140, at most 1,000; at 140 the bounded inputs stay under Cloudflare's free 10,000 Neurons even in the worst case). Once the relay's day is used up, or the provider says its free allowance is (Cloudflare 429 / 3036 / 4006, Gemini 429), nothing is asked of it until 00:00 UTC: 503 `smart_resting` ("Smart features are resting until tomorrow.", hint: back at 00:00 UTC, the streamer's own filters, commands and auto-replies keep working). A failed call gives the subscription's count back; 502 `smart_unavailable` for a provider that's down or an answer that can't be read. 14 s per call.

| Method | Path | Body → Response |
|---|---|---|
| GET | `/v1/pro/smart` | → `{ provider, resting, max_messages: 20, quotas: { smart_moderation, smart_replies, smart_recaps } }` |
| POST | `/v1/pro/smart/moderate` | `{ messages: [{ id, text, user?, first_time? }] /* 1–20, ids unique, text cut to 300 */, rules? /* the streamer's extra words, ≤ 500 */ }` → `{ results: [{ id, action: "none" \| "delete" \| "timeout" \| "ban", category, reason, confidence }] }`, one per message in order. The prompt reads English, Hindi and Hinglish, spelling tricks and context (banter like *bhai*, *yaar*, *pagal*, *noob* is fine; slurs at a person or group, caste, religion, gender or region, sexual harassment, threats, doxxing, scams and spam are not); viewer text goes in as quoted JSON after the instructions. The answer is read strictly: unknown ids dropped, a skipped one is `none`, actions and categories from fixed lists, confidence 0 to 1. 413 `too_many_messages` |
| POST | `/v1/pro/smart/reply` | `{ question /* ≤ 300 */, about /* the approved facts, 1–2,000 */, user?, channel?, title?, game? }` → `{ reply: string \| null }`. `null` when the facts don't answer it (the model says `NO_ANSWER`), or the answer has a link the facts don't contain, `@everyone` / `@here`, or starts like a command. At most 280 characters |
| POST | `/v1/pro/smart/recap` | `{ minutes, title?, game?, peak_viewers, follows, subs, messages, chatters?, bits, tips: ["INR 500.00"], raids: [], thanks: [] /* first 15 used */, previous? }` → `{ summary }` (3 to 5 sentences, who to thank, two ideas for next time; ≤ 1,200 characters). 422 when there's no stream in it |

The Control Center's stats show `usage.smart` (§9.9).
