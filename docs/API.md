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
// tts_audio is a media id of a WAV made with the computer's own voices; it may be null even when tts_text is set
// (synthesis still running or unavailable): then speak tts_text with speechSynthesis.
```

## 4. Routes

### Status and session
| Method | Path | Body → Response |
|---|---|---|
| GET | `/api/v1/status` | → `{ version, port, live: { twitch, youtube, kick }, connectors: { twitch: ConnectorState, youtube, kick, obs, relay }, setup_done: boolean }` |
| POST | `/api/v1/session/pair` | `{ code }` → `{ token }` (no auth needed) |
| POST | `/api/v1/session/pair-code` | → `{ code, url, expires_at }` (makes an "open in browser" link) |
| POST | `/api/v1/setup/complete` | → 204: first run is done, `setup_done` stays true (it's also true once an overlay exists) |
| GET | `/api/v1/stats/session` | → `{ started_at, follows, subs, tips: Money[] (one per currency, refunds taken off), bits, messages, peak_viewers, viewers: { twitch?, youtube?, kick? } }` |
| GET | `/api/v1/recap?since=<ms>` | → `Recap` (below): this stream so far, or the last one after it ended. `since` (optional, not in the future, else 400 `since: …`) picks the start instead |
| GET | `/api/v1/themes` | → `{ id, name, description, pro }[]`: `molten` (default), `daylight`, `arcade`, `minimal`, `sticker`; unknown ids fall back to `molten` |
| GET | `/api/v1/updates` | → `{ current, latest: string \| null, channel: "stable" \| "beta", blocked_while_live, notes_url }`; never offer an update while `blocked_while_live` (R-60) |

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
type Overlay = { id: string; name: string; url: string; width: number; height: number; widgets: Widget[]; updated_at: number };
type Widget = { id: string; kind: "alerts" | "chat" | "events" | "goal" | "label" | "viewers" | "poll" | "giveaway";
                x: number; y: number; w: number; h: number; config: object };
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
| POST | `/api/v1/mod/panic` | `{ on: boolean }` → `{ on, applied: { platform, modes: string[] }[] }`; also sends `panic` and one `notice` frame |
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

### Media
| Method | Path | Body → Response |
|---|---|---|
| POST | `/api/v1/media` | multipart `file` → `{ id, kind: "image" \| "sound" \| "video", name, bytes, url }`. Limits: images 10 MB (png, jpeg, gif, webp, svg not allowed), sounds 5 MB (mp3, ogg, wav), video 50 MB (webm, mp4). 413 / 415 with the limit in the message. |
| GET | `/api/v1/media` | → media list |
| DELETE | `/api/v1/media/{id}` | → 204 (409 if an alert still uses it, with which one) |

### Discord go-live posts
The streamer pastes a Discord webhook URL; Ferrocast posts when a platform goes live. The URL is a secret: it is kept in the OS keychain, never in the database, never in a `.ferropack`, never sent back.

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
GET `/api/v1/obs` → `{ state, version: "31.1.2" | null, scene: string | null, host, port, has_password, detail }`; PUT `/api/v1/obs` `{ host, port, password }` (password goes to the keychain; absent keeps it, `null` clears it).

Auto-detect reads OBS's own WebSocket settings (`plugin_config/obs-websocket/config.json` in OBS's config folder: `~/Library/Application Support/obs-studio` on macOS, `%APPDATA%\obs-studio` on Windows, `$XDG_CONFIG_HOME` or `~/.config/obs-studio` and the Flatpak folder on Linux), so nobody types the port or password:
- GET `/api/v1/obs/detect` → `{ found, port: number | null, has_password, server_enabled, reason: string | null }`. Never the password. A missing or damaged file is `found: false` with a plain `reason`; `server_enabled: false` comes with how to switch it on.
- POST `/api/v1/obs/detect` → the OBS view (as GET `/obs`) after saving `127.0.0.1`, the detected port and password (to the keychain). 422 with the reason when nothing usable was found.

### Settings and data
GET/PUT `/api/v1/settings`: `{ theme: "system" | "dark" | "light", reduced_motion: "system" | "on" | "off", low_motion_overlays, lan_access (next start), update_channel: "stable" | "beta", never_update_while_live, launch_at_login, close_to_tray, language, currency (ISO 4217, the main currency), keep_chat_days, test_alerts, bot_enabled, auto_ban_on_sight }`. PUT saves only the fields sent (unknown ones are ignored); 400 names a bad field (`lan_access: must be true or false.`).
POST `/api/v1/export` → a streamed `.ferropack` download. POST `/api/v1/import` (multipart `file`, up to 2 GB) → `{ imported, media_files, backup }`; a backup is taken first. 415 `bad_pack` / `damaged_pack`, 422 `pack_too_new`, 413 `too_large`. GET `/api/v1/backups`, POST `/api/v1/backups/{id}/restore`. StreamElements and Streamlabs imports aren't in this build yet.

## 5. Live socket for the dashboard

`GET /api/v1/live?session=<token>&since=<seq>` (WebSocket; `since` optional). With `since`, the `event` frames after that seq (the last 500 are kept) are replayed right after `hello`, `status` and `queue`, with their `held` marks; `hello.seq` is then `since` itself, and `resync { missed: 0 }` (0 = count unknown) comes first if some were already forgotten or `since` is from before a restart. Seqs only grow, across restarts too. Drop duplicate event ids. Server → client frames `{ v: 1, type, data }`:
- `hello { seq }`
- `event Event` (everything, including chat)
- `status { connectors }`
- `queue Queue`
- `tts_pending [...]`
- `notice { level: "info" | "warn" | "error", message, action?: { label, href } }` (one with `action: { label: "Reconnect", href: "#/settings/accounts" }` when a platform goes to `sign_in_needed` or `scopes_missing`)
- `goals Goal[]` (after any goal change)
- `held { event_id, reason }` (the chat filter held a message for a mod; its `event` frame comes first and it never reaches overlays)
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

`POST /accounts/{platform}/connect` answers 422 with a message when this build has no app registered for that platform (Twitch and YouTube need the build's client ids; Kick and tips need the relay). `POST /overlays/{id}/add-to-obs` answers 422 with a hint when OBS can't be reached. StreamElements and Streamlabs imports, and the update feed (`/updates` always has `latest: null`), aren't in this build yet.
