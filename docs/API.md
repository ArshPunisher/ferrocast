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
| GET | `/api/v1/updates` | → `{ current, latest: string \| null, channel: "stable" \| "beta", blocked_while_live, notes_url }`; never offer an update while `blocked_while_live` (R-60) |
| POST | `/api/v1/updates/install` | → 202 `{ installing }`; the desktop app downloads it, checks its signature, installs and restarts. 404 `no_update` when nothing newer was found, 409 while live with "never update while live" on (R-60) |

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
                 design: { id: string; scene: string; branded: boolean } | null /* the gallery design it came from (§4 Designs) */ };
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
- `layer` (a static piece of a scene): `{ type: "image" | "text" | "shape" | "frame" | "webcam" | "brand", anim?: { in: AnimIn, delay_ms: 0…10000, loop: "none" | "float" | "pulse" | "shimmer" }, css?: string }` plus, by type:
  - `image`: `{ media: MediaRef, fit?: "cover" | "contain" | "fill", radius? }`
  - `text`: `{ text (≤500 chars, shown as is), font?: FontFamily, size? 6…400, weight? 100…900, color?, align?: "left" | "center" | "right", uppercase?, italic?, letter_spacing? -0.2…1, shadow?: "none" | "soft" | "hard" | "glow", outline? 0…20, outline_color? }`
  - `shape`: `{ shape?: "rect" | "ellipse" | "line" | "slant" | "triangle", fill?, fill2? (gradient end), angle? 0…360, stroke?, stroke_width? 0…50, radius? }`
  - `frame` / `webcam` (a frame with a see-through middle for the camera): `{ style?: "line" | "double" | "corners" | "notch" | "glow", color?, accent?, thickness? 1…40, radius? 0…400, label? ≤40 }`
  - `brand`: `{ variant?: "mark" | "badge" | "watermark", color? }`: the small Spark mark of a built-in design (below)
- Any live widget may carry a style from a widget design: `config.style = { variant?, font?, text?, accent?, background?, track?, muted?, radius? 0…64, font_size? 8…120, uppercase?, brand?: BrandSpot }`, with `config.design_id` and `config.branded`.

Colours are `#rgb`, `#rrggbb`, `#rrggbbaa` or `transparent`. `MediaRef` is a media library id or `asset:<path>` (a file bundled with the app, lowercase `a-z 0-9 / _ . -`). `FontFamily` is letters, digits, spaces and hyphens (1–60). Every check fails with 400 naming the field (`layer 'title': config.color: "red;}" isn't a colour; use #rrggbb.`).

PUT `/overlays/{id}` can't change `design`. An overlay made from a **built-in** design (`design.branded`) keeps its `brand` layer, and a widget styled by a built-in widget design keeps `config.branded: true`: an edit that removes, hides or unbrands it is a 409 `branding` ("Duplicate the design in the gallery to make an unbranded copy").

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
  anim: { in: AnimIn; out: AnimOut; text: "none" | "wave" | "wiggle" | "bounce" | "pop" | "type" | "glow"; in_ms: 0–3000; out_ms: 0–3000; text_delay_ms: 0–10000 };
  sound: { ref: MediaRef | null /* plays instead of builtin */; builtin: "none" | "chime" | "coin" | "whoosh" | "boing" | "bass" | "sparkle"; volume: 0–100 };
  duration_ms: 1000–60000 /* more than in_ms + out_ms */;
  tts: { enabled: boolean; min_amount: number /* major units; 0 reads all */ };
  brand: "corner" | "divider" | "watermark" | "notch" | "badge" | "edge" /* where the mark sits when the design is branded */;
  css: string /* ≤20,000; no @import, url() only for data:image/… */ }
type AnimIn = "none" | "fade" | "rise" | "drop" | "slide_left" | "slide_right" | "zoom" | "pop" | "bounce" | "flip" | "wipe" | "magnet"
type AnimOut = "none" | "fade" | "sink" | "lift" | "slide_left" | "slide_right" | "zoom" | "shrink" | "flip" | "wipe"

type ThemeContent = { scenes: Scene[] /* 1–8, keys unique */; alert_design: string | null /* the alert design that goes with it */ }
type Scene = { key: "in_game" | "just_chatting" | "starting_soon" | "brb" | "ending" | `custom_${number}`; name: string /* 1–40 */;
               width: number; height: number; background: string | null; layers: Widget[] /* checked exactly like an overlay's widgets */ }
type WidgetContent = { widget: WidgetKind /* not "layer" */; w: 40–1920; h: 20–1080; config: object /* starting settings, no style */; style: WidgetStyle }

type AlertDesignView = { design_id: string; variation_id: string; event_key: EventKey; branded: boolean; look: AlertLook;
                         title_parts: { text: string; hl: boolean }[]; body_parts: { text: string; hl: boolean }[] }
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
| GET / PUT | `/api/v1/designs/active-alert` | `{ design_id: string \| null }`. PUT 404 for an unknown design, 400 for a design that isn't an alert design |
| POST | `/api/v1/designs/{id}/export` | → a `.ferrodesign` download (`application/zip`). 409 `built_in` (duplicate it first) |
| POST | `/api/v1/designs/import` | multipart `file` (up to 100 MB) → 201 `{ designs: Design[], media_files: number, notes: string[] }` |

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
type BanEntry = { platform; user_id; login; display_name; reason: string | null; banned_at: number; everywhere: boolean; by: string | null }
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
GET `/api/v1/tts/voices` → `{ id, name, language, pro: false }[]` (the computer's own voices). GET `/api/v1/tts/pending` and the `tts_pending` frame → `{ id, event_id, platform, name, text, amount: string | null ("₹500", "1,000 bits"), received_at }[]`. POST `/api/v1/tts/{id}/approve` · `/reject`.

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
type ProFeatures = { cloud_bot: boolean; remote: boolean; tts: boolean; discord: boolean; backups: boolean; tip_brand: boolean; history: boolean }
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
| GET | `/api/v1/pro/history` | `?from&to&platform` (ms; `twitch` \| `youtube` \| `kick`) → the relay's `{ from, to, streams, truncated, weekly }` (§9.1). 400 for a bad `from`/`to`/`platform`, then the relay's errors |
| GET | `/api/v1/pro/history/export.csv` | Same filters → `text/csv` download (`ferrocast-history.csv`) |
| GET, POST, … | `/api/v1/pro/backups…` | Cloud backups: list, back up now, recovery key, passphrase, restore, delete (§9.3) |

Errors pass the relay's code, message and hint through: 409 `pro_not_on_sale` (hint: launches after version 1), 409 `already_pro`, 409 `already_cancelled`, 409 `not_cancellable`, 404 `no_subscription`, 404 `not_found` (restore: id and email don't match, always the same answer), 422 `invalid_plan` / `invalid_email` / `invalid_subscription`, 429 `rate_limited`, 502 `razorpay_unavailable`, 503 `billing_unavailable` (no keys on the relay yet) / `billing_misconfigured`, 503 `relay_unrecognised` (the relay forgot this install; it re-registers by itself), 502 `relay_unreachable`. Pro feature calls (§9) also pass through 402 `pro_required`, 413 `too_large`, 415 `unsupported_type`, 429 with the relay's `Retry-After`, 503 `vault_unavailable` / `storage_unavailable`. 400 for a body that isn't JSON or misses a field, 415 without `Content-Type: application/json`.

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
                     enabled: boolean; template: string /* 1–1,500 chars; {name}, {platform}, {title}, {url} */; mention_everyone: boolean }
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/discord` | → `DiscordView` |
| PUT | `/api/v1/discord` | `{ webhook_url?: string \| null, enabled?, template?, mention_everyone? }` → `DiscordView`. Only fields sent change; `null` removes the webhook. The URL must be `https://discord.com/api/webhooks/<id>/<token>` (or `discordapp.com`, or `/api/v10/…`), nothing else: 400 `webhook_url: …` |
| POST | `/api/v1/discord/test` | → 204 after posting a test message (never pings). 422 without a webhook; 422 "Discord says this webhook was deleted…" (Discord 404); 422 "Discord refused this webhook…" (401/403); 429 with `Retry-After` when Discord rate-limits; 422 when Discord can't be reached |

Defaults: on, `{name} is live on {platform}! {title|default:"Come hang out."} {url}`, no @everyone ping (`mention_everyone` lets the message ping). One post per platform per 30 minutes (a stream that drops and comes back isn't news). A failed go-live post becomes one `notice` with a link to the Discord settings.

### OBS
GET `/api/v1/obs` → `{ state, version: "31.1.2" | null, scene: string | null, host, port, has_password, auto, detail }`; PUT `/api/v1/obs` `{ host, port, password }` (password goes to the secret store; absent keeps it, `null` clears it).

The engine keeps OBS connected by itself: it checks a working connection every 5 s, retries a closed OBS every 3 s, and sends one `status` frame whenever the state changes. `auto: true` means Ferrocast follows OBS's own settings (below): each attempt re-reads them, so switching on OBS's WebSocket server (or changing its password) connects within seconds, no click needed, and the password is never copied out of OBS's file. The details become the streamer's own (`auto: false`) when they PUT `/obs` with anything different from what's in use; from then on detected values never replace them, until POST `/obs/detect`. Settings saved before `auto` existed count as the streamer's own only if the address or port differ from `127.0.0.1:4455`. With `auto` and no OBS settings on this computer, a failed attempt is `state: "off"` (nothing set up), not `disconnected`.

Auto-detect reads OBS's own WebSocket settings (`plugin_config/obs-websocket/config.json` in OBS's config folder: `~/Library/Application Support/obs-studio` on macOS, `%APPDATA%\obs-studio` on Windows, `$XDG_CONFIG_HOME` or `~/.config/obs-studio` and the Flatpak folder on Linux), so nobody types the port or password:
- GET `/api/v1/obs/detect` → `{ found, port: number | null, has_password, server_enabled, in_use, reason: string | null }`. `in_use`: Ferrocast already uses these (following OBS, or the typed details match), so there is nothing to offer. Never the password. A missing or damaged file is `found: false` with a plain `reason`; `server_enabled: false` comes with how to switch it on.
- POST `/api/v1/obs/detect` ("Use these") → the OBS view (as GET `/obs`) after switching back to following OBS's settings (`auto: true`; a typed password is forgotten). 422 with the reason when nothing usable was found.

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
                    tags?: string[] /* ≤10, letters and digits, ≤25 each; [] clears */; language?: string /* "en" */ }
type PlatformResult = { platform; ok: boolean; applied: string[]; skipped: string[] /* "language: Kick has no stream language setting" */;
                        error: { code: "not_connected" | "reconnect_needed" | "rate_limited" | "refused" | "unreachable" | "no_broadcast"
                                       | "unknown_category" | "not_supported"; message; hint } | null }
type StreamInfoView = { last: HistoryEntry | null; presets: Preset[]; history: HistoryEntry[] /* newest first, 20 */;
                        platforms: { platform; connected; can_edit; needs: string | null; supports: { title, category, tags, language } }[] }
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

Permissions: Twitch needs `channel:manage:broadcast` (Modify Channel Information, stream markers) and Kick `channel:write`; both are asked for at sign-in from this version. Accounts connected earlier show `can_edit: false` with `needs` ("Reconnect Twitch…") and get `reconnect_needed` instead of a silent failure (R-22). YouTube uses the existing `youtube.force-ssl` sign-in and changes the live (or next scheduled) broadcast's video, keeping its description; a game category becomes YouTube's Gaming category.

**Show (go-live checklist).**

```ts
type ShowConfig = { preset_id: string | null; starting_scene: string | null; countdown_s: number /* 0 or 10–3600 */; live_scene: string | null;
                    alerts_on: boolean; discord: boolean; bot_on: boolean; replay_buffer: boolean; ending_scene: string | null;
                    recap_message: string | null /* {thanks} {follows} {subs} {duration} */; mic_sources: string[]; cam_sources: string[];
                    min_disk_gb: number; platforms: Platform[] }
type Check = { id; label; status: "ok" | "warn" | "fail" | "skip"; detail; fix: { label, href /* "#/settings/obs" or "action:unmute:Mic" */ } | null }
type ShowRun = { id; phase: "starting" | "countdown" | "live" | "ended"; started_at; countdown_ends_at: number | null; live_scene; ended_at;
                 steps: { id, label, status: "done" | "failed" | "skipped" | "waiting", detail }[] }
```
| Method | Path | Body → Response |
|---|---|---|
| GET / PUT | `/api/v1/show/config` | `ShowConfig` (400 `countdown_s: …`, `preset_id: …`) |
| GET | `/api/v1/show/preflight` | → `{ checks: Check[], ready }`: OBS, scenes exist, mics not muted, cameras shown, disk space (OBS's recording drive), platforms connected, stream-info permission, Discord webhook |
| POST | `/api/v1/show/preflight/fix` | `{ action: "unmute:<input>" \| "show:<scene>/<source>" \| "start_replay_buffer" }` → the new pre-flight |
| GET | `/api/v1/show` | → `{ show: ShowRun \| null }` |
| POST | `/api/v1/show/start` | `{ force?: boolean }` → 201 `{ show }`. Runs: preset → replay buffer → starting scene → alerts on → bot on → Discord post → countdown (switches to the live scene at zero). 409 while a show runs, 422 `preflight_failed` (unless `force`) |
| POST | `/api/v1/show/countdown/skip` · `/countdown/cancel` · `/live-scene` | → `{ show }`. 404 `no_countdown` |
| POST | `/api/v1/show/end` | → `{ show }`: ending scene, thank-you in chat, an "Show ended" marker. Never stops the stream itself |

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

### Settings and data
GET/PUT `/api/v1/settings`: `{ theme: "system" | "dark" | "light", reduced_motion: "system" | "on" | "off", low_motion_overlays, lan_access (next start), update_channel: "stable" | "beta", never_update_while_live, launch_at_login, close_to_tray, language, currency (ISO 4217, the main currency), keep_chat_days, test_alerts, bot_enabled, auto_ban_on_sight }`. PUT saves only the fields sent (unknown ones are ignored); 400 names a bad field (`lan_access: must be true or false.`).
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
- `notice { key?, level: "info" | "warn" | "error", message, action?: { label, href } }`. A notice replaces the shown one with the same `key` (one line per subject, never a stack): `panic`, `account:twitch` / `account:youtube` / `account:kick`, `discord`, `update`, `no-chat`, `alerts-folded` (one with `action: { label: "Reconnect", href: "#/settings/accounts" }` when a platform goes to `sign_in_needed` or `scopes_missing`)
- `goals Goal[]` (after any goal change)
- `held { event_id, reason }` (the chat filter held a message for a mod; its `event` frame comes first and it never reaches overlays)
- `held_resolved { event_id, outcome: "released" | "dropped" }` (a mod approved or removed a held message, from any dashboard; it has its own seq, so a reconnecting dashboard replays it even with a `since` from after the `held` mark)
- `panic { on, plan: PanicConfig }`
- `chatter { event_id, kind: "first" } | { event_id, kind: "returning", days }` (after that message's `event` frame; replayed with it on reconnect)
- `giveaway { giveaway: GiveawayView | null }` (on every change, at most every 250 ms while entries come in)
- `poll { poll: PollView | null }` (on start, end, removal, and at most every 250 ms while votes come in)
- `resync { missed }`: the socket fell behind; refetch status, queue and events

The client sends nothing but pings.

## 6. Overlay protocol

Page: `GET /o/<token>/` (the overlay bundle). Media: `GET /o/<token>/media/<id>`. Socket: `GET /o/<token>/ws`.

Server → overlay frames `{ v: 1, seq, type, data }`:
| type | data |
|---|---|
| `snapshot` | `{ overlay: Overlay, state: { goals: Goal[], labels: { [widget_id]: string }, viewers: { [platform]: number }, events: Event[] (last 25 alertable), chat: Event[] (last 50), poll: PollView \| null, giveaway: GiveawayCard \| null } }`, always first |
| `event` | `Event`: chat messages and alertable events the overlay's widgets need (filtered server-side) |
| `alert` | `RenderedAlert` |
| `alert_hide` | `{ alert_id }` |
| `state` | partial `{ goals?, labels?, viewers?, poll?, giveaway? }` (`poll` and `giveaway` only to overlays with that widget) |
| `config` | `Overlay` (re-render without reloading) |
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

`POST /accounts/{platform}/connect` answers 422 with a message when this build has no app registered for that platform (Twitch and YouTube need the build's client ids; Kick and tips need the relay). `POST /overlays/{id}/add-to-obs` answers 422 with a hint when OBS can't be reached. `/updates` has `latest: null` until a signed release is published (and always in the headless app, which doesn't update itself).

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

Data kept (R-51): subscription id, plan, status, period end, cancel/refund flags, an HMAC-SHA256 of the lowercased checkout email keyed with `PRO_EMAIL_KEY` (never the email), which installs share it, payment ids and amounts, handled event ids (30 days), hashed manage tokens. Never card, UPI, name or phone. A daily re-check re-reads unfinished subscriptions and the refunds of the last 60 days' payments, in case webhooks were lost.

## 9. Relay: Pro features

Every Pro feature (docs/plan/PRO.md F1–F7) runs on the relay under `/v1/pro/<feature>/…` with the same install `Authorization` and error shape as §8. This is the contract each one follows as it ships; its routes are added as §9.x (§9.1 stream history, §9.2 tip branding and §9.3 cloud backups are built).

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

**Features and quotas in the status.** `/v1/pro/status` `features` says which features this relay has, built *and* configured (`cloud_bot`, `remote`, `tts`, `discord`, `backups`, `tip_brand`, `history`; `history` is always on, `backups` and `tip_brand` are on when the relay has a file store, the rest are `false` until each ships). `quotas` lists the subscription's counters as `{ [name]: { used, limit, period: "day" | "month", reset_at } }`; quotas are per subscription, shared by its installs, and count in UTC calendar days or months.

**The dashboard.** A Pro feature's panel is hidden unless the feature is in `features` and `active || sales_open`. Not Pro (but on sale): one calm line, "Runs on Ferrocast's servers, part of Pro.", and a link to Settings › Pro, never a nag. Pro: the feature itself.

### 9.1 Stream history (F7)

`crates/relay/src/history`, migration 4 (`streams`, `stream_samples`). One row per stream **per platform**, owned by the subscription (the PCs sharing Pro share one history). The engine (`crates/engine/src/history.rs`) counts each live platform while Pro is active (the remembered status, no network check), uploads the stream when it starts and again with its totals when it ends, and a viewer sample every 5 minutes while live. Uploads wait in a queue in the local store (settings `history_upload`) and are retried with backoff (30 s doubling to 15 min); 400/404/409/413/415/422 answers are dropped (retrying can't help), 401/402/429/5xx and no connection keep them. A stream the app restarted in the middle of ends where it was last seen once its platform isn't live for 10 minutes. Only counts are stored, never chat, names or viewers (R-51).

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
| GET | `/v1/pro/history` | `?from&to&platform` (default the last 28 days; up to 400 days) → `{ from, to, streams /* newest first, at most 2,000 */, truncated, weekly /* every week from from's to to's, empty ones too, oldest first */ }` |
| GET | `/v1/pro/history/export.csv` | Same filters (default everything kept, 400 days) → `text/csv; charset=utf-8`, `Content-Disposition: attachment; filename="ferrocast-history.csv"`, `no-store`. Columns `platform, started_utc, ended_utc, minutes, title, category, peak_viewers, avg_viewers, chat_lines, chatters, new_chatters, follows, subs, tips, platform_value`; text is quoted and a leading `= + - @` gets a `'` (no spreadsheet formulas). **Open after Pro ends** while the history is kept (R-04): 402 only without a subscription, or a lapsed one with nothing kept. |

Errors (besides the gate's 401 / 402, which every route checks before the body): 400 `invalid` (not JSON, a missing or wrong-typed field, a `from`/`to` that isn't a number; the message names it), 415 `unsupported_type` (no `Content-Type: application/json`), 413 `too_large` (over the relay's 256 KB), 422 `invalid_stream` (the message names the field: unknown platform, bad key, a start in the future or over 400 days ago, an end before the start or over 31 days after it, negative counts, `avg_viewers` over `peak_viewers`, `new_chatters` over `chatters`, a currency that isn't three capitals, two entries for one currency), 422 `invalid_range` (`to` not after `from`, over 400 days), 422 `invalid_platform`, 429 `history_rate` (60 requests a minute per subscription, every route together; `X-Quota-Reset`, `Retry-After`; not a quota, so not in `quotas`), 503 `storage`.

Kept while Pro lasts. The hourly housekeeping deletes a subscription's history 30 days after its access ended (period over, every renewal failed, refunded, or the subscription is gone), and viewer samples older than 400 days for everyone. Titles and categories lose control and direction characters and are cut to 300 / 120 characters.

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
