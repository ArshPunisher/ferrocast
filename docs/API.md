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
                       duration_ms: number; layout: string; css: string; tts_text: string | null; tts_audio: string | null; test: boolean };
```

## 4. Routes

### Status and session
| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/status` | → `{ version, port, live: { twitch, youtube, kick }, connectors: { twitch: ConnectorState, youtube, kick, obs, relay }, setup_done: boolean }` |
| POST | `/api/v1/session/pair` | `{ code }` → `{ token }` (no auth needed) |
| POST | `/api/v1/session/pair-code` | → `{ code, url, expires_at }` (makes an "open in browser" link) |

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
type Overlay = { id: string; name: string; url: string; width: number; height: number; widgets: Widget[]; updated_at: number };
type Widget = { id: string; kind: "alerts" | "chat" | "events" | "goal" | "label" | "viewers";
                x: number; y: number; w: number; h: number; config: object };
```

| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/overlays` | → `Overlay[]` |
| POST | `/api/v1/overlays` | `{ name, preset?: "starter" \| "alerts_only" \| "chat_only" \| "blank" }` → `Overlay` |
| GET / PUT / DELETE | `/api/v1/overlays/{id}` | `Overlay` (PUT replaces name, size, widgets) |
| POST | `/api/v1/overlays/{id}/regenerate-url` | → `Overlay` (old URL stops working at once) |
| POST | `/api/v1/overlays/{id}/add-to-obs` | → `{ scene, source }` (422 if OBS isn't connected) |

Widget `config` per kind:
- `alerts`: `{ theme: string, rule_ids: string[] | null }` (null = all rules)
- `chat`: `{ platforms: Platform[], max_messages: number, fade_after_s: number | null, show_badges: boolean, show_platform: boolean, hide_commands: boolean, hide_bots: boolean, font_size: number, theme: string }`
- `events`: `{ kinds: string[], max_items: number, theme: string }`
- `goal`: `{ goal_id: string, theme: string }`
- `label`: `{ source: "latest_follower" | "latest_sub" | "latest_tip" | "top_tip_session" | "session_follows" | "session_subs", template: string, theme: string }`
- `viewers`: `{ platforms: Platform[], theme: string }`

### Alerts
| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/alert-rules` | → `AlertRule[]` (see `core::alert`) |
| PUT | `/api/v1/alert-rules/{id}` | `AlertRule` → `AlertRule` (400 with the validation message) |
| POST | `/api/v1/alert-rules` | `AlertRule` → `AlertRule` |
| DELETE | `/api/v1/alert-rules/{id}` | → 204 |
| POST | `/api/v1/test` | `{ kind, platform? }` → `{ event_id }` (fires a `test` event through the real pipeline) |
| GET | `/api/v1/queue` | → `{ paused, playing: { alert, started_at } \| null, waiting: { alert, priority, enqueued_at }[] }` |
| POST | `/api/v1/queue/pause` · `/resume` · `/skip` · `/clear` | → queue |
| POST | `/api/v1/queue/{alert_id}/remove` | → queue |
| POST | `/api/v1/events/{event_id}/replay` | → `{ event_id }` |
| GET | `/api/v1/events?before=<seq>&limit=50&kinds=follow,tip` | → `{ events: Event[], next_before: number \| null }` |
| GET | `/api/v1/events/{event_id}/why` | → `{ alerted: boolean, reasons: string[] }` ("No alert rule for Kick raids", "Twitch hid this follow", "Below the ₹100 minimum") |

### Goals
`Goal = { id, title, kind: "follows" | "subs" | "tips" | "bits" | "custom", target: number, currency: string | null, starts_at, ends_at: number | null, current: number }`
GET/POST `/api/v1/goals`, PUT/DELETE `/api/v1/goals/{id}`, POST `/api/v1/goals/{id}/adjust { delta }`.

### Chat and moderation
| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/chat?limit=200` | → `Event[]` (recent chat_message events) |
| POST | `/api/v1/chat/send` | `{ text, platforms: Platform[] \| "all", reply_to?: { platform, message_id } }` → `{ sent: Platform[], failed: { platform, reason }[] }` |
| POST | `/api/v1/mod/delete` | `{ platform, message_id }` → 204 |
| POST | `/api/v1/mod/timeout` | `{ platform, user_id, seconds, reason? }` → 204 |
| POST | `/api/v1/mod/ban` | `{ user: { platform, user_id, login }, reason?, everywhere: boolean }` → `{ banned: Platform[], failed: [...] }` |
| POST | `/api/v1/mod/unban` | `{ platform, user_id }` → 204 |
| GET | `/api/v1/mod/bans` | → shared ban list |
| POST | `/api/v1/mod/panic` | `{ on: boolean }` → `{ on, applied: { platform, modes: string[] }[] }` |
| GET / PUT | `/api/v1/mod/filters` | word list, link rules, caps/emote/symbol limits, actions |
| GET / PUT | `/api/v1/viewers/{platform}/{user_id}/note` | `{ text }` |

### Bot
GET/POST `/api/v1/commands`, PUT/DELETE `/api/v1/commands/{id}`; GET/POST `/api/v1/timers`, PUT/DELETE `/api/v1/timers/{id}`.
`Command = { id, name: "!discord", aliases: string[], response: string (template), roles: "everyone" | "subs" | "vips" | "mods", cooldown_s: number, user_cooldown_s: number, platforms: Platform[] | [], enabled }`
`Timer = { id, name, message, every_min: number, min_chat_lines: number, platforms: Platform[] | [], enabled }`

### TTS
GET/PUT `/api/v1/tts` (`{ enabled, voice, min_amount, max_chars, hold_for_approval, read_names }`), GET `/api/v1/tts/voices`, GET `/api/v1/tts/pending`, POST `/api/v1/tts/{id}/approve` · `/reject`.

### Tips
GET/PUT `/api/v1/tips` (`{ enabled, display_name, page_url, currency, min_amount, providers: { razorpay?: {...}, kofi?: {...}, stripe?: {...} } }`).

### Media
| Method | Path | Body → Response |
|---|---|---|
| POST | `/api/v1/media` | multipart `file` → `{ id, kind: "image" \| "sound" \| "video", name, bytes, url }`. Limits: images 10 MB (png, jpeg, gif, webp, svg not allowed), sounds 5 MB (mp3, ogg, wav), video 50 MB (webm, mp4). 413 / 415 with the limit in the message. |
| GET | `/api/v1/media` | → media list |
| DELETE | `/api/v1/media/{id}` | → 204 (409 if an alert still uses it, with which one) |

### OBS
GET `/api/v1/obs` → `{ state, version, scene }`; PUT `/api/v1/obs` `{ host, port, password }` (password goes to the keychain).

### Settings and data
GET/PUT `/api/v1/settings`; POST `/api/v1/export` → a `.ferropack` download; POST `/api/v1/import` (multipart, `source: "ferropack" | "streamelements" | "streamlabs"`) → a report of what was imported; GET `/api/v1/backups`, POST `/api/v1/backups/{id}/restore`.

## 5. Live socket for the dashboard

`GET /api/v1/live?session=<token>` (WebSocket). Server → client frames `{ v: 1, type, data }`:
- `hello { seq }`
- `event Event` (everything, including chat)
- `status { connectors }`
- `queue Queue`
- `tts_pending [...]`
- `notice { level: "info" | "warn" | "error", message, action?: { label, href } }`
- `goals Goal[]` (after any goal change)
- `held { event_id, reason }` (the chat filter held a message for a mod)
- `panic { on, plan }`
- `resync { missed }`: the socket fell behind; refetch status, queue and events

The client sends nothing but pings.

## 6. Overlay protocol

Page: `GET /o/<token>/` (the overlay bundle). Media: `GET /o/<token>/media/<id>`. Socket: `GET /o/<token>/ws`.

Server → overlay frames `{ v: 1, seq, type, data }`:
| type | data |
|---|---|
| `snapshot` | `{ overlay: Overlay, state: { goals: Goal[], labels: { [widget_id]: string }, viewers: { [platform]: number }, events: Event[] (last 25 alertable), chat: Event[] (last 50) } }`, always first |
| `event` | `Event`: chat messages and alertable events the overlay's widgets need (filtered server-side) |
| `alert` | `RenderedAlert` |
| `alert_hide` | `{ alert_id }` |
| `state` | partial `{ goals?, labels?, viewers? }` |
| `config` | `Overlay` (re-render without reloading) |
| `delete_message` | `{ platform, message_id }` |
| `delete_user` | `{ platform, user_id }` |
| `clear_chat` | `{ platform: Platform \| null }` |
| `tts` | `{ id, text }`: a mod approved a held TTS message; speak it |
| `revoked` | `{}`: this URL was regenerated or the overlay deleted; the server closes the socket next |

Overlay → server frames: `hello { since_seq }`, `ack { alert_id, phase: "started" | "finished" | "failed" }`, `visibility { visible }`. Anything else is ignored and logged. **An overlay can never create events** (R-36).

Test mode: `/o/<token>/?test` asks the server for sample events on a loop, for previews in the editor.

Build notes for `ferrocast-web`: the overlay bundle is served under `/o/<token>/`, so build it with Vite `base: "./"` (assets resolve to `/o/<token>/assets/…`). The dashboard is served under `/app/` (Vite `base: "/app/"`); unknown paths without a file extension fall back to `index.html`. Dashboard `<img>`/`<audio>` previews may pass the session as `?session=` on `GET /api/v1/media/{id}`.

## 7. Not in this build yet

`POST /accounts/{platform}/connect` and `POST /overlays/{id}/add-to-obs` answer 422 with a clear message until the platform and OBS adapters land.
