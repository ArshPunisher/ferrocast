# Ferrocast cloud mode (1.4)

Ferrocast running from our server, with the desktop app closed or never installed. Free with
an account (like StreamElements). A streamer signs in on the web, connects Twitch and Kick
there, sets up alerts, designs, overlays and the bot in the web dashboard, pastes the overlay
URLs into OBS, and it all works with no PC of theirs running Ferrocast.

Code: `crates/relay/src/cloud/` (relay), `ferrocast-web/apps/dashboard` (cloud mode screens),
`crates/engine/src/cloud_sync.rs` (the desktop's sync, M6). This file is the contract; the
API shapes are in §9.

Owner decisions this follows:

- **Free** with an account. Cloud YouTube is **Pro** (built separately; the hooks are in §11).
- The **desktop app keeps working fully local**. Nothing in cloud mode is needed by the app,
  and an app that never signs in never talks to any of this (R-01).
- Built on what the relay already has: accounts (docs/ACCOUNTS.md), the cloud bot's Twitch
  EventSub conduit and Kick webhooks (API.md §9.7), tip webhooks, the vault, blobs, Brevo.
- Only the free Oracle server. No paid service, no new host required (§3).

## 1. Architecture

```
 Twitch EventSub (conduit webhook) ─┐
 Kick webhooks ─────────────────────┼─▶ ingest (cloud/ingest.rs) ─▶ per-account bus (cloud/bus.rs)
 Tip webhooks (cloud install) ──────┘        │ normalised ferrocast_core::event::Event          │
                                              │ dedupe (platform message id)                     │
                                              ▼                                                  ▼
                                   cloud bot (bot::on_chat,          alert pipeline: core::alert::select/render,
                                   sub = the account id)             core::design::pick/render_onto, core::queue
                                                                                                 │
                     overlay pages /o/<token>/ (the overlay build) ◀── overlay sockets /o/<token>/ws
                     web dashboard /app/ (cloud mode) ◀── /c/api/v1/* + /c/api/v1/live (cookie + CSRF)
                     desktop app (signed in) ◀── /v1/cloud/docs (bearer fca_, sync)
```

- **Ingest.** The relay already receives Twitch EventSub notifications (one conduit, webhook
  shard at `/v1/bot/twitch/eventsub`) and Kick webhooks (`/v1/kick/webhook`). Cloud mode adds
  subscriptions for a cloud channel (§4) and one hook in each handler: a notification for a
  channel bound to a cloud account goes to `cloud::ingest`. The events are mapped with the
  same mappers the desktop uses (`ferrocast_twitch::map`, `ferrocast_kick::map`), so an alert
  in the cloud is the same `Event` as on the PC.
- **Per-account event bus** (`cloud::bus`). In memory only, created when an account becomes
  active (an overlay or dashboard socket is open, or a live event arrives) and dropped 10
  minutes after the last socket closes and no event came. Holds: a `broadcast` channel of
  frames (capacity 64), the last 25 alertable events, the last 50 chat lines, viewer counts,
  session labels (latest follower, sub, tip…), and the alert queue (`core::queue::AlertQueue`).
  Nothing of chat is written to disk (R-51); events are kept in memory only, so a relay
  restart starts with empty lists (the overlays' snapshot then has no history, like a fresh
  app start).
- **Alerts** use the core functions the engine uses: `alert::select` → `alert::render` →
  the active alert design's `design::pick` / `render_onto` → `AlertQueue` (one per account,
  acks from the overlay pages drive it, as on the PC). TTS in the cloud: the overlay speaks
  `tts_text` with the browser's voice (no local voices, no Ferrocast voices in 1.4).
- **Overlay sockets** speak the same protocol as the engine (API.md §6): `snapshot` first,
  then `event`, `alert`, `alert_hide`, `state`, `config`, `delete_message`, `delete_user`,
  `revoked`; the page sends only `hello`, `ack`, `visibility` (R-36). The page itself is the
  existing overlay build (`ferrocast-web/dist/overlays`), served unchanged.
- **Settings and designs per account** live in the relay's database as JSON documents
  (`cloud_docs`, §2), checked with the same validators the engine uses
  (`Overlay::validate`, `AlertRule::validate`, `Design::validate`), so a document the cloud
  takes is one the app takes and the other way round.

### 1.1 Sync with the desktop app, and who wins

The web dashboard edits the cloud copy directly. A desktop app that is signed in and has
**Cloud sync** on (M6, off by default) pushes and pulls the same documents through
`/v1/cloud/docs` (§9.3) with per-document versions:

- Every document has a server `version` (one counter per account, raised on every write) and
  `updated_at`. The app remembers the version it last saw for each document and the account's
  highest version (`since`).
- **Pull**: `GET /v1/cloud/docs?since=N` returns every document changed after N, deletions as
  tombstones (kept 30 days).
- **Push**: each document carries `base_version` (what the app last saw). The relay applies it
  only when the cloud copy is still at that version; otherwise it is a **conflict**.
- **Conflict rule: the cloud copy wins.** Cloud mode is set up and used on the web, and the
  cloud copy is what OBS shows, so it is never overwritten by an older edit. The app's losing
  version is not thrown away: an overlay or a design is saved on the PC as a copy named
  "<name> (this PC)", an alert rule is kept in the app's notice ("Your change to the Follow
  rule on this PC was replaced by the one from the web"). Deleting on one side while the
  other edited: the edit wins (the document comes back).
- Media files sync by content hash: the app uploads a file the cloud doesn't have (counts
  against the 200 MB, §5), the cloud's files download on demand.
- With sync off the app and the cloud are separate. The app never needs the cloud.

How the app does it (M6, `crates/engine/src/cloud_sync.rs`; routes in API.md "Cloud sync"):
`GET/PUT /api/v1/pro/cloud` and `POST /api/v1/pro/cloud/sync`, under `/pro` like the account so a
phone never reaches them. Turning sync on calls `POST /v1/cloud/enable` (free) and syncs at once;
then every 5 minutes and 10 s after an overlay or alert rule changes. The app keeps, per
document, the version and a hash it last agreed on with the cloud (times and its own overlay
token left out of the hash, so a save that changed nothing never counts). A document that
exists on both sides with different content on the first sync is a conflict like any other
(the cloud copy wins). "Use cloud overlays" needs sync: the dashboard's `Overlay.url` becomes
the synced overlay's cloud URL (from `GET /v1/cloud/overlays`), and a pulled layout reaches
pages open on the PC's own URL too.

### 1.2 What runs in the cloud in 1.4 and what stays in the app

| In the cloud (free) | Stays in the app |
|---|---|
| Alerts for follows, subs, resubs, gift subs, Bits, Kick KICKs, raids, tips; alert rules and their variations | OBS control (scenes, sources, Add to OBS, auto-detect) |
| Goals, chat polls and giveaways (free to enter) (§7.1, §7.2) | Points-priced giveaway entries |
| Alert designs (built-in gallery + your own), the active design, test alerts | Local TTS voices, Ferrocast voices (Pro), held-TTS approval queue |
| Overlays and their widgets: alerts, chat (multichat Twitch + Kick), event list, labels, viewers, layers | Loyalty points, quotes, duels (need the PC's viewer database) |
| The bot: commands, counters, timers, auto-replies, through the existing cloud bot (§6) | Chat moderation filters, panic mode, ban-on-sight lists |
| Tips through the account's tip page (§7) | Studio (markers, clips, the "Don't miss" lane), the recap, importers |
| Chat in the web dashboard (read) | Sending chat as the streamer, viewer cards, Discord go-live webhook |
| YouTube: Pro, separate (§11) | Export/import (`.ferropack`), local backups |

Since the cloud has goals, polls and giveaways (§7.1, §7.2), every widget works in a cloud
overlay: the snapshot carries `goals`, and `poll` / `giveaway` for an overlay with that widget
(the running or last one, else `null`).

### 1.3 How the live parts behave (M3)

- **One ingest worker** takes the queue in order, so an account's alerts never overtake each
  other; everything it does is in memory (the rules, the alert design in use and the settings
  are read once per bus and again after any document change).
- **TTS** follows the alert design in use: its own `tts` switch and minimum decide, the message
  is read through `core::tts::decide` (never numbers, links or held text), and the overlay
  speaks it with the browser's voice. Without an alert design nothing is read. There's no
  approval queue in the cloud.
- **Session labels** (latest follower, sub, tip, top tip, this stream's follows and subs) live on
  the bus only and start again when a stream starts or the relay restarts.
- **Dashboard lists**: `GET /c/api/v1/events` is the bus's last 25 live alertable events (test
  ones never), `GET /c/api/v1/chat` its last 50 lines; both empty after a restart.
- **Close codes** of an overlay socket: 4404 unknown, rotated, deleted or the account disabled
  (the page clears and stops), 4410 the account sleeps, 4429 over a socket cap (20 an account,
  4 an overlay, 30 opens a minute per IP), 4503 the relay is full; the page retries the last
  three later. A frame over 4 KB closes the socket.
- **Uploads** are read into memory (20 MB at most), four at a time relay-wide; one more gets 503
  `cloud_busy`. Multipart `file` (the dashboard) or the raw file with `X-Media-Name` (the app).
- `RELAY_OVERLAY_URL`'s host counts as one of Ferrocast's own hosts for the custom tip address
  check, so overlay pages there are never mistaken for someone's tip address.

## 2. Storage

New relay tables (one migration, `cloud::SCHEMA`):

| Table | What |
|---|---|
| `cloud_accounts` | `account_id` PK, `enabled_at`, `last_active_at` (sign-in, dashboard or overlay use), `sleeping_at`, `next_version`, `bytes_used` |
| `cloud_docs` | `(account_id, kind, id)` PK, `version`, `updated_at`, `deleted_at` (tombstone), `json` (≤ 256 KB). `kind`: `overlay`, `alert_rule`, `design`, `settings` (one document `main`: active alert design, test alerts on/off, queue tuning), `goal` (§7.1, cloud only, never synced) |
| `cloud_overlay_tokens` | `token_hash` PK (SHA-256), `account_id`, `overlay_id`, `generation` |
| `cloud_channels` | `(platform, channel_id)` PK → `account_id`, `login`, `scopes`, `event_subs` (JSON ids), `connected_at`, `error` |
| `cloud_media` | `(account_id, id)` PK, `sha256`, `kind`, `mime`, `bytes`, `name`, `created_at` |
| `cloud_pending` | one-time connect finishes (§4): `code_hash` PK, `account_id`, `platform`, sealed tokens, `expires_at` |

Platform sign-ins (refresh tokens) are sealed in the vault exactly like the cloud bot's
(`bot_tokens`, AAD `<account id>|bot_<platform>_bot`), because the cloud bot reads them there
(§6). Media bytes are in the blob store under `cloud/<account id>/<sha256>`.

## 3. Overlay URLs

- `https://<overlay origin>/o/<token>/`, the overlay build; socket `…/o/<token>/ws`; media
  `…/o/<token>/media/<id>`. The overlay origin is `RELAY_OVERLAY_URL` if set, else the API
  host (`RELAY_PUBLIC_URL`, `https://ferrocast-relay.clearcraftlabs.com`), which is already
  live. **Never the web host**: the web host carries the session cookie, and a page that
  renders viewer text must not share an origin with the signed-in dashboard.
- The token is 40 lowercase hex characters: `HMAC-SHA256(cloud key, "overlay|<account>|<overlay
  id>|<generation>")`, cut to 160 bits, where the cloud key is derived from the vault key
  (HKDF label `ferrocast cloud overlay v1`). Only its SHA-256 is stored; the dashboard is
  shown the URL by deriving it again. Unguessable, never in logs (paths are logged without it).
- **Rotate** (`POST …/overlays/{id}/regenerate-url`): the generation goes up, the old hash is
  deleted, open pages on the old URL get `revoked` and close with 4404 (R-44). Deleting an
  overlay does the same. The dashboard shows URLs blurred until "Show".
- An unknown token: the page loads, the socket closes with 4404 (the page clears itself).
  `GET /o/<unknown>/` answers 404 with a plain page (no build), so a scanner learns nothing.
- `?test` on the socket: the layout plus samples every 6 s, as on the PC.

## 4. Connecting Twitch and Kick on the web

Twitch uses the existing confidential "Ferrocast Cloud" app (`TWITCH_CLOUD_*`), Kick the
relay's Kick app (`KICK_CLIENT_*`). Both reuse the redirect URIs already registered
(`/v1/bot/twitch/callback`, `/v1/kick/callback` on the API host), told apart by a state prefix
(`cl.`), so nothing new has to be registered.

1. Dashboard: `POST /c/api/v1/accounts/{twitch|kick}/connect` (cookie + CSRF) → `{ flow:
   "browser", url }`. The state is `cl.<random>` (24 random bytes), single use, kept server side
   in `pending_oauth` for 10 minutes with the account it names (Kick: with its PKCE verifier),
   so nothing in the URL needs signing.
2. The platform redirects to the API host. The relay trades the code, reads whose account it
   is, seals the tokens into `cloud_pending` under a one-time code (5 minutes), and redirects
   (303) to `https://<web host>/c/v1/connect/finish?code=…`.
3. **The finish needs the same signed-in account** (the web cookie): a link that reaches
   someone else's browser, or a browser that is signed out or signed in to another account,
   binds nothing and the fresh sign-in is revoked. This stops "connect your Twitch to my
   account" tricks, since the API host can't see the web cookie.
4. Bound: the channel row, the sealed sign-in, the EventSub / Kick subscriptions (below), and
   the cloud bot's channel (§6). The page goes back to `/app/#/cloud`.

Scopes asked (once, R-22): Twitch `user:read:chat user:write:chat user:bot channel:bot
moderator:read:followers channel:read:subscriptions bits:read channel:read:redemptions`
(the bot speaks as the streamer's own account, R-28); Kick the relay's `KICK_SCOPES`.

Twitch subscriptions on the conduit (12, each cost 0; ids in `cloud_channels.event_subs`, made by
cloud mode only, so the cloud bot's own subscribe and unsubscribe leave an account's channel
alone): `channel.chat.message`, `stream.online`, `stream.offline`, `channel.follow` (v2,
moderator = the streamer), `channel.subscribe`, `channel.subscription.gift`,
`channel.subscription.message`, `channel.cheer`, `channel.raid` (to the channel),
`channel.channel_points_custom_reward_redemption.add`, `channel.chat.message_delete`,
`channel.chat.clear_user_messages` (`channel.chat.clear` is left out: the mapper doesn't read
it). Kick: `chat.message.sent`, `channel.followed`, `channel.subscription.new|renewal|gifts`,
`kicks.gifted`, `livestream.status.updated`, `moderation.banned`, created with the streamer's
token (`POST /public/v1/events/subscriptions`, method webhook). A failure is kept on the
channel (`error`, shown on the Cloud page) and the hourly sweep tries again.

The sign-in is sealed where the cloud bot reads it (`bot_tokens`, sub = the account id, purpose
`bot`) and a `bot_channels` row is made with the streamer as both bot and channel (§6). One
channel per platform per account; a channel already bound to another account (or to a Pro
cloud bot) moves to the newer, proven owner: the old binding's subscriptions are deleted and
its rows forgotten **without a revoke** (it's the same person at the platform, and a revoke
could end the new sign-in too); the old account's Cloud page then shows it not connected. The
hand-over from the API host to the web host is a one-time code (`cloud_pending`, the sign-in
sealed with AAD `<account>|cloud_pending_<platform>`, 5 minutes); one never finished is revoked
by the hourly sweep.

Disconnect: subscriptions deleted, the sign-in revoked at the platform, rows deleted.

If the same channel is also connected in the desktop app, both receive its events: the app's
own (EventSub WebSocket, its Kick link) and the cloud's. They don't interfere; the streamer
picks which overlays OBS shows.

## 5. Free limits

| Limit | Value | What happens |
|---|---|---|
| Uploads (media for designs and alerts) | **200 MB per account**, 20 MB per file, images, audio, short video (png, jpeg, gif, webp, mp3, ogg, wav, webm, mp4) | 413 `cloud_storage_full` with the used/limit and "Delete files in the media library"; 415 `unsupported_media` naming the allowed types; 413 `too_large` per file |
| Event history | **30 days** for anything stored about events (counts, the session labels' last values); chat is never stored | swept hourly |
| Documents | 50 overlays, 200 alert rules, 100 designs per account, 256 KB each | 409 `cloud_limit` naming the limit |
| Overlay sockets | 20 open per account, 4 per overlay | the extra socket closes with 4429 (the page retries later) |
| Dashboard requests | 120 a minute per account; writes 60 a minute | 429 `cloud_rate` with `Retry-After` |
| Test alerts | 10 a minute per account | 429 |
| **Sleeping accounts** | an account not used for **30 days** (no sign-in, no dashboard, no overlay open) **sleeps**: ingest stops (subscriptions deleted, the bot off) | waking needs a sign-in on the web (or the app with sync on): subscriptions are made again within a minute; overlays of a sleeping account show nothing and the socket closes with 4410 |

Overlays open in OBS count as use, so a streamer who streams at least monthly never sleeps.

How sleeping works (M5): the hourly sweep puts an account to sleep, deletes its channels'
subscriptions (so nothing arrives and the bot, gated by `bot_allowed`, is quiet), drops its bus
and closes its open overlays with 4410 (they try again later). The rows, sign-ins and the
bot's switch stay. Any dashboard call (`GET /c/v1/cloud` included, which still answers
`sleeping: true` that once so the Cloud page can say so) or synced app wakes it and its
channels are subscribed again at once. Event history: in 1.4 nothing about events is stored
(the bus is memory only), so there's nothing to sweep after 30 days.

## 6. The bot in the cloud

The existing cloud bot (API.md §9.7) runs it, keyed by the **account id** instead of a Pro
subscription id (`bot_channels.sub_id = ac_…`; account ids and subscription ids never clash).
For a cloud account:

- The channel step and the speaking account are the streamer's own sign-in from §4 (same
  account, the bot badge through `channel:bot`).
- Commands, counters, timers and auto-replies are edited in the web dashboard (the Bot page in
  cloud mode) and stored as the bot's config (`bot_config`, the same `CloudBotConfig` shape,
  validated by the same code). The 20 messages a minute, 500 commands, 50 timers limits stay.
- "Pro" for the bot's gates means "cloud mode on and not sleeping" for an account key
  (`cloud::bot_allowed`), so the bot's code paths stay one.
- Lease: if the streamer's desktop app is signed in to the same account with its bot on, it
  sends `bot_seen` (as Pro PCs do) and the cloud stands by, so the two never answer twice.
- The config's `bot_on` (the desktop's own switch) is always on for an account: the Bot page's
  "Run the bot in the cloud" switch (`PUT …/pro/bot/enabled`) is the one switch.

## 7. Tips

A cloud account can have a tip page without a PC: the relay makes it a **cloud install** (an
`installs` row with a random secret nobody holds, linked to the account), so the existing tip
pages and provider webhooks (`/v1/tips/<provider>/<install>`) work unchanged. A verified tip
for the cloud install, or for any PC signed in to an account with cloud mode on, is published
on the account's bus (alert + labels). The tip page and the provider set-up are edited in the
web dashboard's Tips page (the same fields as the app).

How it's built (`cloud/tips.rs`):

- The cloud install is made on the **first save** of the Tips page (`PUT /c/api/v1/tips`),
  never on a look: `installs` row (its secret hashed and thrown away, `last_seen` moved on so
  the weekly sweep of never-used installs leaves it alone) and `cloud_accounts.install_id`, set
  only while it's empty, so two first saves at once keep one install (the loser's row is
  deleted). No new table, no migration.
- `GET/PUT/DELETE /c/api/v1/tips` speak the engine's `TipSettings` (API.md "Tips"): the same
  checks as a PC's save (`tips::save_for`: handle rules, provider hosts, secret shapes, a blank
  secret keeps the saved one, name changes per day, names held 30 days), secrets never sent
  back, `webhooks` naming the cloud install. Errors as the PC's: 400 `invalid` (the message
  names the field), 409 `taken`, 429 `rate_limited`, plus cloud mode's 409 `cloud_off`.
- A verified tip for a cloud install is **not queued** for delivery (no PC ever reads it); it
  goes only to the account's bus (`ingest::tip`), deduped per provider payment as before, so
  alerts, the latest/top tip labels and tips goals (§7.1) follow. Since nothing else holds it,
  a tip the ingest queue can't take (full) is given back: its dedupe key is released and the
  provider gets 503 `busy`, so its retry delivers it. A tip on a **sleeping** account's tip page
  wakes the account (money arriving is use) and still alerts.
- Pro's tip page extras (branding, goal page, own address) stay with a PC's Pro install in
  1.4: a cloud install isn't linked to a subscription, so its page is the free look. The
  dashboard's Tips page in cloud mode shows the page and the providers only.

### 7.1 Goals

Goals are account documents (`cloud_docs` kind `goal`, the core `Goal`: progress in minor
units for tips), up to **20** an account (409 `cloud_limit`), never synced with the desktop app
(the pull leaves them out, a push of one is 422): the app's goals count its own event log.
`cloud/goals.rs`, the engine's paths and shapes (API.md "Goals"):

- `GET/POST /c/api/v1/goals`, `PUT/DELETE /c/api/v1/goals/{id}`, `POST …/{id}/adjust { delta }`.
  The engine's checks (`GoalView::into_goal`: a tips goal needs a currency, counts take whole
  numbers, a title of 1 to 80 characters; 422 `invalid` naming it), `current` never taken
  from a body, and a goal with progress keeps its kind and currency (409 `conflict` with a
  hint). `starts_at` 0 means now. Each edit reads and writes the document in one transaction
  (`Db::cloud_doc_edit`), so a tip counted during an edit is never lost.
- Counting (worked out from the stored goal inside its write, so an edit in between never gets
  another unit's amount; at most 20 goals, counted in the same transaction as a new one): every
  **live** event through the pipeline (`goal::contribution`: follows, new subs
  and gift batches, Bits, tips and paid messages in the goal's currency, captured only),
  tests never. Progress is saved at once, so it outlives the bus and a relay restart. The
  pipeline's dedupe (the platform's id) keeps a retry from counting twice while the bus is in
  memory; a refund doesn't take a cloud tip back in 1.4 (tips arrive captured only, and a
  later refund notice isn't an event).
- Frames: `state { goals }` to every overlay of the account (as the engine sends it) and
  `goals` to the dashboard's live socket; the overlay snapshot carries them. Goals are read
  into the bus once and again after any change.

### 7.2 Polls and giveaways

`cloud/community.rs`: the engine's rules from core (`PollSpec::cleaned`, `Poll::vote`,
`GiveawaySpec::cleaned`, `Giveaway::enter`, `pick_weighted` with the OS's randomness), held on
the account's bus **in memory**, as the app holds them (a relay restart forgets a running one,
like an app restart). A bus with an open poll, or a giveaway taking entries that started less
than 24 hours ago, is never dropped as idle or pushed out for another account; a giveaway left
open longer may go with its bus, so a forgotten one can't hold memory for good.

- Routes, the engine's paths and answers (API.md "Polls", "Giveaways"):
  `GET /c/api/v1/polls/current`, `POST /c/api/v1/polls`, `POST …/current/end`,
  `DELETE …/current`; `GET /c/api/v1/giveaways/current`, `POST /c/api/v1/giveaways`,
  `POST …/current/{close|draw|redraw|end}`, `DELETE …/current` (`{ refunded: 0 }`). Errors:
  422 `invalid` naming the field, 404 `poll_not_found` / `giveaway_not_found`, 409 `conflict`
  (another running, already ended, already drawn), 422 `nobody_entered` / `everyone_won`, 503
  `cloud_busy`.
- Viewers vote with `!vote N` and enter with the keyword in **live** chat on any connected
  platform (Shared Chat from a partner channel and tests never count), once per person:
  `platform:user id` (the cloud has no viewer database to link accounts, so one person on two
  platforms is two). The streamer, bots and anonymous accounts can't enter (core's
  `is_audience`). **Entry costs no points** in the cloud (the points live in the PC's
  database): a `cost` above 0 is 422 with a hint.
- Results go out at most every 250 ms while votes or entries come in: `state { poll }` /
  `state { giveaway }` (the `GiveawayCard`) to overlays with that widget, `poll { poll }` /
  `giveaway { giveaway }` to the dashboard. A poll closes itself at its end time.
- Chat: the start, a poll's result and each winner are posted as the streamer by the cloud bot
  (`bot::announce`), only where the bot is switched on and the cloud has that chat (no PC on
  duty), counted against its 20 messages a minute, split to each platform's length (500, YouTube
  200). With the bot off nothing is posted, and the Community page says so.

## 8. Resource budget (free Oracle ARM server, 4 OCPU, 24 GB; the relay container 1 GB)

Per **active** streamer (bus in memory): rings and queue ≈ 100 KB, a bus channel ≈ 64 frames
(shared `Arc<str>`), ≈ 30 KB per open overlay socket, about 5 events a second at a busy peak
(chat included), each a JSON map and a few small allocations. Measured target: **≤ 300 KB and
≤ 0.2 % of one core per active streamer** with 3 overlays open.

| | Default | Setting |
|---|---|---|
| Active streamers (bus in memory) | 400 | `CLOUD_MAX_ACTIVE` |
| Overlay sockets relay-wide | 4,000 | `CLOUD_MAX_SOCKETS` |
| Ingest queue | 4,096 waiting, one worker (in order, in memory only) | fixed |
| Memory for cloud mode | ≈ 120 MB at 400 active | |

**Measured (load test, `cloud/load_tests.rs`).** N active accounts, each with its Twitch
channel bound and **3 overlay pages open** (the Starter overlay: alerts, chat, events,
labels), at a **busy peak of 5 events a second each** (9 in 10 chat lines, 1 in 10 a follow
with its alert, acked by the pages), through the real signed EventSub webhook, the ingest
queue, the bus and the sockets. The relay runs as its own process (release build) and is
sampled with `ps`; 30 s runs on an Apple M4 (the Oracle Ampere cores are slower, so read the
CPU with headroom):

| Accounts | Sockets | Events in 30 s | Dropped | RSS idle → pages open → peak | Per account (pages open / peak) | CPU (one core) | Per account |
|---|---|---|---|---|---|---|---|
| 20 (10 s) | 60 | 1,020 | 0 | 19 → 28 → 32 MB | 488 / 673 KB | 4.8 % | 0.24 % |
| 100 | 300 | 15,100 | 0 | 20 → 63 → 85 MB | 435 / 651 KB | 18.8 % | 0.19 % |
| 300 | 900 | 45,300 | 0 | 21 → 150 → 210 MB | 431 / 632 KB | 42.7 % | 0.14 % |

Every webhook answered 2xx, every page stayed open, nothing was dropped. CPU per account
meets the ≤ 0.2 % target at a busy peak (and falls with scale); **memory is about twice the
300 KB estimate**: ≈ 430 KB an account with 3 pages open (≈ 140 KB a socket: its buffers,
the per-socket task and the broadcast receiver) and ≈ 630 KB at peak, with the allocator
keeping freed pages. At the 400-account default that is ≈ 250 MB, inside the relay
container's 1 GB but not the "≈ 120 MB" above; `memory_estimate_bytes` in the admin stats
still uses 300 KB. Run it again with:

```text
CLOUDLOAD_ACCOUNTS=100 CLOUDLOAD_SECS=30 cargo test -p ferrocast-relay --release --lib \
  -- --ignored cloudload_ --nocapture --test-threads=1
```

How it degrades:

1. Over `CLOUD_MAX_SOCKETS`: new overlay sockets close with 4503 and retry with backoff; open
   ones keep working.
2. Over `CLOUD_MAX_ACTIVE`: the least recently used idle bus (no socket open) is dropped; if
   none is idle, a new account's events are dropped and counted (`cloud.dropped`) and its
   sockets get 4503 (they retry).
   The dashboard says "Ferrocast's servers are busy; your overlays come back on their own".
3. The ingest queue full: events are dropped and counted (`cloud.dropped` in the admin
   stats), webhooks still answer 2xx at once (Twitch and Kick never unsubscribe us).
4. Chat is the bulk of the load: a bus with no chat widget open and the bot off drops chat
   lines at ingest.
5. Twitch's EventSub limits: the conduit's cost is 0 for authorized subscriptions; at most
   ~11 subscriptions per channel. Kick: one webhook per event per channel.

## 9. API

Errors everywhere: `{"error": {"code", "message", "hint"}}` with the right status, never a bare
500. Shared codes: 401 `signed_out`, 403 `csrf` / `account_disabled`, 404 `not_found` (also: cloud
mode not on this relay), 409 `cloud_limit` / `stale_version`, 413 `too_large` /
`cloud_storage_full`, 415 `unsupported_type` / `unsupported_media`, 422 `invalid` (the message
names the field, from the core validators), 429 `cloud_rate` (`Retry-After`), 503 `storage`,
`vault_unavailable`, `cloud_busy`, `platform_unavailable`.

### 9.1 Web dashboard (web host, cookie + CSRF, like `/r/…`)

The dashboard's own client talks to `https://<web host>/c` with the same paths as the engine's
`/api/v1/*` (API.md §4), so its screens work unchanged. Only these exist; anything else is
404 `not_in_cloud` ("That needs the Ferrocast app on your PC.").

| Route | Notes |
|---|---|
| `GET /c/v1/cloud` | `{ enabled, sleeping, overlay_origin, usage: { bytes, bytes_limit, overlays, rules, designs, goals }, limits, channels: [...], features, youtube }`. 404 when cloud mode isn't on this relay (the dashboard hides it) |
| `POST /c/v1/cloud/enable` | turns cloud mode on for the account (free, idempotent) → the same |
| `GET /c/api/v1/status` | an engine-like `Status` with `cloud: true` and the connectors from the channels |
| `GET /c/api/v1/accounts` · `POST …/{twitch\|kick\|youtube}/connect` · `DELETE …/{platform}` | §4; YouTube is Pro (§11) |
| `GET/POST /c/api/v1/overlays`, `GET/PUT/DELETE …/{id}`, `POST …/{id}/regenerate-url` | as API.md §4 Overlays; `url` is the cloud URL |
| `GET/POST /c/api/v1/alert-rules`, `PUT/DELETE …/{id}` | as API.md |
| `GET /c/api/v1/designs[?kind=]`, `GET/PUT/DELETE …/{id}`, `POST /c/api/v1/designs`, `POST …/{id}/duplicate`, `POST …/{id}/use`, `GET/PUT /c/api/v1/designs/active-alert` | built-ins from the overlay build's catalogue, read-only |
| `POST /c/api/v1/test` | a test alert through the cloud pipeline |
| `GET /c/api/v1/queue`, `POST …/queue/{pause\|resume\|skip\|clear}` | the cloud queue |
| `GET /c/api/v1/events`, `GET /c/api/v1/chat` | from the bus (memory) |
| `GET/POST /c/api/v1/media`, `GET/DELETE …/{id}` | uploads (§5) |
| `GET/PUT/DELETE /c/api/v1/tips` | the tip page on the account's cloud install (§7), the engine's `TipSettings` |
| `GET/POST /c/api/v1/goals`, `PUT/DELETE …/{id}`, `POST …/{id}/adjust` | goals (§7.1) |
| `GET/DELETE /c/api/v1/polls/current`, `POST /c/api/v1/polls`, `POST …/current/end` · `GET/DELETE /c/api/v1/giveaways/current`, `POST /c/api/v1/giveaways`, `POST …/current/{close\|draw\|redraw\|end}` | polls and giveaways (§7.2) |
| `GET /c/api/v1/bot/config`, `PUT …` (`{ version, config }`) · `GET /c/api/v1/pro/bot`, `PUT /c/api/v1/pro/bot/enabled` | the cloud bot (§6) |
| `GET/POST /c/api/v1/commands`, `PUT/DELETE …/{id}`, `POST …/starter` · `GET/POST /c/api/v1/timers`, `PUT/DELETE …/{id}` | the engine's command and timer paths, kept in the bot's config, so the Bot screen works unchanged |
| `GET /c/api/v1/live` (WebSocket) | live frames for the dashboard: `hello`, `event`, `queue`, `status`, `goals`, `poll`, `giveaway`; Origin must be the web host |
| `GET /c/v1/connect/finish?code=` | §4 step 3 (a page, then a redirect to `/app/`) |

### 9.2 Overlay pages (overlay origin)

`GET /o/<token>/`, `/o/<token>/<file>`, `/o/<token>/ws`, `/o/<token>/media/<id>` (§3).

### 9.3 Desktop app sync (API host, `Authorization: Bearer fca_…`)

| Route | Body → Response |
|---|---|
| `GET /v1/cloud` | the same as `GET /c/v1/cloud` |
| `POST /v1/cloud/enable` | turns cloud mode on from the app (free, idempotent) → the same |
| `GET /v1/cloud/overlays` | the cloud overlays with their cloud URLs ("Use cloud overlays", M6) |
| `GET /v1/cloud/docs?since=0` | `{ version, docs: [ { kind, id, version, updated_at, deleted, body \| null } ] }` (at most 500 a page, `more: true`) |
| `PUT /v1/cloud/docs` | `{ docs: [ { kind, id, base_version, body \| null } ] }` (≤ 100) → `{ version, applied: [{kind,id,version}], conflicts: [ { kind, id, server: {version, updated_at, deleted, body} } ] }`. A conflict is not an error (200); a bad document is 422 naming `docs[i]`, more than 100 is 413 `too_many`, a push that would go over a §5 document cap is 409 `cloud_limit`; all of these are checked before anything is written |
| `POST /v1/cloud/media` | upload (octet-stream, `X-Media-Name`, `X-Media-Sha256`) → `{ id }` (dedupe by hash) |
| `GET /v1/cloud/media/{id}` | the file |

### 9.4 Admin (Control Center, ADMIN-API.md § Cloud mode)

`GET /admin/v1/cloud/stats` → `{ available, active_streamers, max_active, accounts_on, sleeping,
channels: { twitch, kick, youtube }, overlay_sockets, max_sockets, events_last_hour (the hour
before the last hourly sweep), events_since_start, dropped_events, storage: { bytes, files },
memory_estimate_bytes (300 KB per active streamer) }`. Same guard as every admin route.

## 10. Security

- **Tokens**: overlay tokens (§3) are capability URLs: 160 bits, hashed at rest, rotatable,
  never logged. Platform refresh tokens sealed in the vault (AES-256-GCM, AAD per account and
  purpose), never logged or shown. Web calls use the account's session cookie + CSRF token
  (ACCOUNTS.md §1); the app uses its `fca_` session.
- **Origins**: overlays never on the web host (§3); `/c/…` only on the web host (else 404);
  the dashboard socket checks `Origin`; overlay pages have a CSP that allows scripts and
  connections only to their own origin, images from https.
- **Overlays never create events** (R-36): the socket takes only `hello`, `ack` (only for an
  alert this account's queue is playing), `visibility`; anything else is ignored; frames over
  4 KB close the socket.
- **Rate limits**: §5, plus per IP on the overlay paths (60 page loads and 30 socket opens a
  minute) and the existing sign-in limits.
- **Abuse**: one channel per platform per account, ownership proven by the platform sign-in;
  the bot's 20 messages a minute; the owner can disable an account (ACCOUNTS.md: disabled
  accounts' ingest stops at once and their overlays close); uploads checked by content (magic
  bytes), served with `nosniff`, `Content-Disposition: inline` only for images/audio/video,
  never HTML or SVG.
- **Data kept** (R-51): §2's tables; no chat, no viewer list, no IP addresses. Deleting an
  account deletes its cloud documents, media, channels (revoked) and overlay tokens.

## 11. Hooks for cloud YouTube (Pro, built separately)

- `cloud::ingest::publish(st, account, Event)`: any producer (the YouTube reader) hands
  normalised events to an account's bus; alerts, chat overlays and labels follow.
- `GET /c/v1/cloud` `channels` lists YouTube (bound to the account's Pro subscription, not a
  `cloud_channels` row); the connect route answers 402 `pro_required` for YouTube without Pro,
  so the dashboard can show the Pro card.
- `cloud::bot_allowed` is the one gate for the bot keyed by an account; YouTube's own quota
  rules (API.md §9.7) apply on top.

**What's built (Pro, 1.4).** YouTube from the cloud with no PC is in
`crates/relay/src/bot/youtube/web.rs` (API.md §9.7 "YouTube from the web"): a Pro account
connects on the web host (`/r/v1/pro/youtube/connect`, the same Google client and callback,
finish only for the same signed-in account) and the channel is bound to the **account's Pro
subscription**, so the cloud bot, its config and its YouTube budget are the same ones the
account's PCs use. The budget is per subscription (one per account) with a fair share of
Google's 10,000 a day across the streamers that used YouTube that day (never below 600).

**What cloud mode adds (built after 1.4.0).**

- **Connect from the Cloud page.** `POST /c/api/v1/accounts/youtube/connect` answers
  `{ flow: "browser", url }` for an account with Pro (`bot::youtube::web::connect_url`, the same
  checks as the Account page's connect: 20 an hour), and the finish returns to
  `/app/#/cloud?connected=youtube` (the nonce remembers which page started it). Without Pro:
  402 `pro_required` with a hint. **Without the relay's "Ferrocast Cloud (YouTube)" client**
  (`YOUTUBE_CLOUD_CLIENT_ID`/`_SECRET`, set with `pro-keys.sh --youtube`) or the vault: 503
  `platform_unavailable` ("YouTube from the cloud isn't set up on Ferrocast's servers yet.",
  hint: it keeps working in the app), and `GET /c/v1/cloud` says `features.youtube: false`, so
  the Cloud page explains instead of offering a button that can't work. `DELETE
  /c/api/v1/accounts/youtube` disconnects (revoked at Google), also after Pro ended (R-04).
- **The Cloud page's view.** `GET /c/v1/cloud` gains `youtube: { available, pro, bot_on,
  channel, quota }` (the Account page's view) and lists a bound channel in `channels`
  (`platform: "youtube"`, `login` = the channel title; `error` says why it stopped, Pro ended
  included). `GET /c/api/v1/status` reports its connector.
- **YouTube on the bus.** The cloud bot's YouTube reader (`bot::youtube::read`) hands every
  event it reads (chat, Super Chats, Super Stickers, memberships) to
  `cloud::ingest::youtube(st, account, event)` for the account the subscription is attached
  to, while that account has cloud mode on and awake; alerts, chat overlays, labels, goals and
  polls follow like Twitch's and Kick's. The reader runs for a Pro subscription with a ready
  YouTube channel whose bot is on **or** whose account uses cloud mode (`Db::cloud_youtube_subs`),
  so overlays get YouTube with the bot switched off; the bot still answers only when on. The
  **quota is unchanged**: the reader spends the subscription's own units (its fair share of the
  day, `fair_limit`) exactly as before, publishing costs nothing, and a spent day stops the
  reader as before (the channel's error says until when). While a PC holds YouTube's lease
  (its bot on), the PC reads YouTube and the cloud reads nothing; cloud overlays then miss
  YouTube until the PC goes quiet for 90 s (the same rule as the bot).
- **Reading YouTube chat, and saying why it stops.** Every reason the reader pauses or
  stops (stream refused, dropped, silent for 90 s, "chat over", no sign-in) is logged at info
  the first time per subscription and hour (`the cloud bot's YouTube chat reader`, with
  YouTube's own words), and the first batch of lines each hour too (how many, how many new),
  so a quiet YouTube bot shows why in the relay log. YouTube ending the stream as "chat over"
  within 20 s of opening while `liveBroadcasts` says live, **3 times in a row**, is a stream
  YouTube won't let the cloud read: the channel gets `youtube_chat_unavailable` ("check that
  live chat is on for the stream, then press Try again") and reading rests an hour, instead
  of opening a stream every 2 minutes (6 units each round) and reading nothing. Those strikes
  count only for one broadcast: a live check that finds it ended, a new chat id, a line read,
  or Try again starts them over. An answer marked offline (YouTube's own end of the chat) is
  never a strike. In a stream's
  first answer only lines **published before the stream opened** (with 5 s of slack) are
  history (`Origin::Backfill`, never answered); a line published after it is new, even in
  that first answer (the same in the desktop's reader).
- **One bot config for an account without a PC.** For a Pro account whose subscription has
  **no PC linked** (`pro_installs` empty) and a YouTube channel, the cloud Bot page's saves are
  copied to the subscription's config (`bot::youtube_follows_account`, `mirror_to_youtube`), and
  its switch switches YouTube's bot too (with only YouTube connected it is YouTube's switch).
  With a PC linked that **never pushed** a config (version 0), the YouTube bot reads the
  Bot page's config without writing it under the subscription (`load_config` falls back to
  the account's config, `bot_on`), so the first push from that PC takes over cleanly. With a
  PC that pushed, its config stays YouTube's and the Bot page says so
  (`youtube.follows_this_page: false` on `GET /c/api/v1/pro/bot`).

## 12. Milestones

- **M1** account-scoped documents and the sync API (§2, §9.3), with the core validators.
- **M2** web Twitch/Kick connect (§4).
- **M3** ingest, bus, alert pipeline, overlay pages and sockets from the relay (§1, §3).
- **M4** web dashboard cloud mode (alerts, designs, overlays and their URLs, chat, bot).
- **M5** limits, sleeping accounts, admin numbers (§5, §8, §9.4).
- **M6** desktop: when signed in, "Use cloud overlays" and sync (§1.1).
