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

### 1.2 What runs in the cloud in 1.4 and what stays in the app

| In the cloud (free) | Stays in the app |
|---|---|
| Alerts for follows, subs, resubs, gift subs, Bits, Kick KICKs, raids, tips; alert rules and their variations | OBS control (scenes, sources, Add to OBS, auto-detect) |
| Alert designs (built-in gallery + your own), the active design, test alerts | Local TTS voices, Ferrocast voices (Pro), held-TTS approval queue |
| Overlays and their widgets: alerts, chat (multichat Twitch + Kick), event list, labels, viewers, layers | Loyalty points, polls, giveaways, quotes, duels (need the PC's viewer database) |
| The bot: commands, counters, timers, auto-replies, through the existing cloud bot (§6) | Chat moderation filters, panic mode, ban-on-sight lists |
| Tips through the account's tip page (§7) | Studio (markers, clips, the "Don't miss" lane), the recap, importers |
| Chat in the web dashboard (read) | Sending chat as the streamer, viewer cards, Discord go-live webhook |
| YouTube: Pro, separate (§11) | Export/import (`.ferropack`), local backups |

Widgets that need the PC (`poll`, `giveaway`) render nothing in a cloud overlay (the snapshot
says `poll: null`, `giveaway: null`); the overlay editor marks them "Needs the app".

## 2. Storage

New relay tables (one migration, `cloud::SCHEMA`):

| Table | What |
|---|---|
| `cloud_accounts` | `account_id` PK, `enabled_at`, `last_active_at` (sign-in, dashboard or overlay use), `sleeping_at`, `next_version`, `bytes_used` |
| `cloud_docs` | `(account_id, kind, id)` PK, `version`, `updated_at`, `deleted_at` (tombstone), `json` (≤ 256 KB). `kind`: `overlay`, `alert_rule`, `design`, `settings` (one document `main`: active alert design, test alerts on/off, queue tuning) |
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

## 7. Tips

A cloud account can have a tip page without a PC: the relay makes it a **cloud install** (an
`installs` row with a random secret nobody holds, linked to the account), so the existing tip
pages and provider webhooks (`/v1/tips/<provider>/<install>`) work unchanged. A verified tip
for the cloud install, or for any PC signed in to an account with cloud mode on, is published
on the account's bus (alert + labels). The tip page and the provider set-up are edited in the
web dashboard's Tips page (the same fields as the app).

## 8. Resource budget (free Oracle ARM server, 4 OCPU, 24 GB; the relay container 1 GB)

Per **active** streamer (bus in memory): rings and queue ≈ 100 KB, a bus channel ≈ 64 frames
(shared `Arc<str>`), ≈ 30 KB per open overlay socket, about 5 events a second at a busy peak
(chat included), each a JSON map and a few small allocations. Measured target: **≤ 300 KB and
≤ 0.2 % of one core per active streamer** with 3 overlays open.

| | Default | Setting |
|---|---|---|
| Active streamers (bus in memory) | 400 | `CLOUD_MAX_ACTIVE` |
| Overlay sockets relay-wide | 4,000 | `CLOUD_MAX_SOCKETS` |
| Ingest queue | 4,096 waiting, 32 workers | fixed |
| Memory for cloud mode | ≈ 120 MB at 400 active | |

How it degrades:

1. Over `CLOUD_MAX_SOCKETS`: new overlay sockets close with 4503 and retry with backoff; open
   ones keep working.
2. Over `CLOUD_MAX_ACTIVE`: the least recently used idle bus (no socket open) is dropped; if
   none is idle, a new account's events wait in the ingest queue and its sockets get 4503.
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
| `GET /c/v1/cloud` | `{ enabled, sleeping, overlay_origin, usage: { bytes, bytes_limit, overlays, rules, designs }, limits, channels: [...], features }`. 404 when cloud mode isn't on this relay (the dashboard hides it) |
| `POST /c/v1/cloud/enable` | turns cloud mode on for the account (free, idempotent) → the same |
| `GET /c/api/v1/status` | an engine-like `Status` with `cloud: true` and the connectors from the channels |
| `GET /c/api/v1/accounts` · `POST …/{twitch\|kick}/connect` · `DELETE …/{platform}` | §4 |
| `GET/POST /c/api/v1/overlays`, `GET/PUT/DELETE …/{id}`, `POST …/{id}/regenerate-url` | as API.md §4 Overlays; `url` is the cloud URL |
| `GET/POST /c/api/v1/alert-rules`, `PUT/DELETE …/{id}` | as API.md |
| `GET /c/api/v1/designs[?kind=]`, `GET/PUT/DELETE …/{id}`, `POST /c/api/v1/designs`, `POST …/{id}/duplicate`, `POST …/{id}/use`, `GET/PUT /c/api/v1/designs/active-alert` | built-ins from the overlay build's catalogue, read-only |
| `POST /c/api/v1/test` | a test alert through the cloud pipeline |
| `GET /c/api/v1/queue`, `POST …/queue/{pause\|resume\|skip\|clear}` | the cloud queue |
| `GET /c/api/v1/events`, `GET /c/api/v1/chat` | from the bus (memory) |
| `GET/POST /c/api/v1/media`, `GET/DELETE …/{id}` | uploads (§5) |
| `GET /c/api/v1/bot/config`, `PUT …` · `GET /c/api/v1/pro/bot`, `PUT /c/api/v1/pro/bot/enabled` | the cloud bot (§6) |
| `GET /c/api/v1/live` (WebSocket) | live frames for the dashboard: `hello`, `event`, `queue`, `status`; Origin must be the web host |
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

`GET /admin/v1/cloud/stats` → active cloud streamers now, accounts with cloud on, sleeping,
connected channels per platform, overlay sockets open, events in the last hour, dropped
events, storage used (bytes, files), memory estimate. Same guard as every admin route.

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
- `cloud_channels.platform = "youtube"` rows and `GET /c/v1/cloud` `channels` list them; the
  connect route answers 402 `pro_required` for YouTube without Pro, so the dashboard can show
  the Pro card.
- `cloud::bot_allowed` is the one gate for the bot keyed by an account; YouTube's own quota
  rules (API.md §9.7) apply on top.

## 12. Milestones

- **M1** account-scoped documents and the sync API (§2, §9.3), with the core validators.
- **M2** web Twitch/Kick connect (§4).
- **M3** ingest, bus, alert pipeline, overlay pages and sockets from the relay (§1, §3).
- **M4** web dashboard cloud mode (alerts, designs, overlays and their URLs, chat, bot).
- **M5** limits, sleeping accounts, admin numbers (§5, §8, §9.4).
- **M6** desktop: when signed in, "Use cloud overlays" and sync (§1.1).
