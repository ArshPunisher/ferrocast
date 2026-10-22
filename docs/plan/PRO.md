# Ferrocast Pro: build plan

Approved by the owner on 19 Oct 2026: seven Pro features, all of them things that run on our
servers. Free features never move behind Pro (R-01, R-05). Google Cloud TTS is the chosen voice
provider; a Discord app "Ferrocast" exists (id 1561463863197835294); Brevo is the mail sender.

## 1.4: what Pro adds (owner decision, 21 Oct 2026)

Pro = **YouTube from the cloud + smart features + a year of insights**, on top of everything
Pro already has. Built 21 Oct 2026 (STATUS.md):

- **F8. Insights** (F7 grown up): stream history for **free accounts** too, the last 30 days;
  Pro keeps a year. The relay keys free rows by the account id, sweeps hourly, and says
  `limited` when a range goes past what the plan keeps, so the dashboard shows a calm Pro note
  (never to the audience). Insights page: charts per stream (viewers, follows, subs, tips,
  chat), best times to stream, top supporters (from the PC's own log, free), CSV.
- **F9. Smart features** (never called "AI" in the product, R-15): smart moderation
  suggestions that read Hinglish (log, one-click undo, automatic short timeouts only if the
  streamer turns it on, never a ban), smart replies from facts the streamer approved (rate
  limited), and the written stream recap. A free model tier only (Cloudflare Workers AI, or
  Gemini's free tier), off until `pro-keys.sh --ai`, hard caps per subscription and relay-wide
  (API.md §9.10).
- **F1 grown up: YouTube from the cloud** with no PC (the web dashboard connects it for a Pro
  account) and a fair share of Google's daily quota across streamers (API.md §9.7). Google
  verification steps: docs/GOOGLE-VERIFICATION.md.

## Ground rules

- Relay DB: one `Db { conn: Mutex<Connection> }`, each module its own `SCHEMA` (`CREATE IF NOT
  EXISTS`) called from `Db::open`. M0 moves to numbered migrations (`PRAGMA user_version`) and a
  writer + small read pool. Container memory limit is 256 MB today (raise to 1 GB); the VM is
  shared with TransformX and Hookline.
- Identity: `authed()` gives `install_id`; Pro links installs via `pro_installs` (up to 3 per
  subscription). **Every quota is per subscription (`sub_id`).**
- `/v1/link` is the outbound 64 KB queue (`item` → `ack`, dispatched in
  `engine/relay.rs::handle_relay_item`). New streams use their own sockets.
- Reuse pure logic in `ferrocast-core` (`command::run`, `Timer::due`, `chatbot::*`, `recap`).
- Auto-replies don't exist yet: build them free and local first (M5), the cloud only runs them.
- Discord go-live by webhook from the PC stays free; Pro adds cloud posts and role perks.
- `pricing.astro` advertises "Smart filters": remove; list exactly what ships.

Shared pieces (M0):
- `relay/src/billing/gate.rs`: `require_pro(&st, &headers) -> Result<ProCtx { install, sub_id }>`;
  401 `unauthorized`, **402 `pro_required`** ("This runs on Ferrocast's servers and is part of
  Pro."). A lapsed subscription stops cloud work at the next hourly sweep, keeps data 30 days,
  export stays open (R-04).
- Carry-over (`relay/src/billing/carry.rs`, decided 19 Oct 2026): when a new subscription starts
  granting Pro (webhook, refresh, daily re-check), a lapsed subscription still inside its 30 days
  hands it everything keyed by its id (history, finished backups with their files, cloud bot with
  its sign-ins re-sealed for the new id, Discord server with rules/links/grants, weekly email with
  the address re-sealed, paired phones and audit) in one transaction. Only the same owner's: an
  install linked to the new subscription was linked to the old one (`pro_install_subs`, migration
  13) and the checkout email hash matches. `comp_` test grants are never a source or target. A
  feature moves only when the new subscription has none of its own; a value that doesn't open, a
  backup file that can't be copied or an unfinished upload stays behind and expires with the old
  id. Running again moves nothing. Over 30 days, nothing moves.
- `relay/src/vault.rs`: AES-256-GCM with `RELAY_VAULT_KEY` (32 bytes, `deploy/vault-key.sh` like
  `razorpay-keys.sh`), AAD `install|purpose`, rows `(key_v, nonce, ct)`, redacted `Debug`, never
  logged. Platform tokens are opt-in; removing one deletes the row and revokes it.
- `relay/src/quota.rs`, `relay/src/blob.rs` (`BlobStore`: `FsBlobStore`, later R2).
- Per-module `routes()` merged in `lib.rs` with stubs for every feature, so later milestones
  don't touch `lib.rs`/`db.rs`. `/v1/pro/status` gains `features` and `quotas`.
- Dashboard: `components/pro/ProChip.tsx` (neutral chip) and `ProPanel.tsx` ("Runs on
  Ferrocast's servers, part of Pro", link to Settings › Pro, no nagging). Pro panels show only
  when `active || sales_open`.

M0 as built (19 Oct 2026):
- Migrations live in `relay/src/migrate.rs` (append one line per feature: version, name, that
  module's `SCHEMA`); 1 = every table from before, 2 = `quota_usage`. A copy is taken first
  (`relay.db.before-vN.from-vM.<ms>`, never overwritten); migrations only add, so a rollback still runs on a newer DB.
- Each feature module (`history`, `backups`, `remote`, `bot`, `tts`, `discord`, `tips`) has
  `routes()`, `available()` (→ `/v1/pro/status` `features`) and `QUOTAS` (→ `quotas`); `mail`
  has no routes. `quota::Db::try_take` returns `TakeErr::{Over(QuotaErr { reset_at }), Db}` →
  `into_api(code, message)` (429 + `X-Quota-Reset` ms + `Retry-After`).
- `AppState::vault()` / `blobs()` answer 503 `vault_unavailable` / `storage_unavailable` when
  the key or the folder is missing.
- Follow-ups: split `Db` into one writer + a small read pool (still one `Mutex<Connection>`);
  vault key rotation (`RELAY_VAULT_KEY_OLD` with `key_v` 2) if it's ever needed; the demo-only
  `#/preview/pro` route in the dashboard goes once a real Pro panel exists (M1 History);
  raise the relay container's memory limit before M3/M6.

## Features

### F1. 24/7 cloud bot
- Relay `bot/{mod,twitch,kick,youtube,lease,store}.rs` (deps on twitch/kick/youtube crates for
  REST and mapping); engine `cloud_bot.rs` pushes config (2 s debounce, backoff); Bot page panel
  "Keep the bot on when this PC is off".
- Twitch: second confidential app "Ferrocast Cloud"; streamer's bot account authorizes
  `user:bot user:read:chat user:write:chat` via a relay broker (like the Kick broker), the
  broadcaster grants `channel:bot`; chat via EventSub Conduit webhook shards at
  `/v1/bot/twitch/eventsub` (HMAC + replay check); replies with the app token. Moderation scopes
  only with a separate opt-in, refresh token in the vault.
- Kick: webhooks already reach the relay; replies need an opt-in `chat:write` refresh token.
- YouTube: last (M12, built as a beta: "M12 as built" below); refresh token in vault, gRPC `streamList` only while
  live; shared 10k/day quota: 1,500 units per subscription per day, bring-your-own-key a follow-up, apply for quota audit.
- Lease (R-11): a desktop with the cloud bot on and a live link refreshes `desktop_seen`; the
  cloud stands by, takes over after 90 s of silence, hands back on reconnect. The free desktop
  bot never waits for the relay. Replies deduped per platform message id.
- Tables: `bot_channels`, `bot_tokens(sub_id, platform, purpose, key_v, nonce, ct, updated_at)`,
  `bot_config(sub_id PK, version, json, updated_at)`, `bot_counter_changes` (sent back as link
  kind `bot_counter`). Chat never stored; in-memory ring of 200 lines per channel (R-51) for F2.
- API (Bearer + Pro): `GET /v1/pro/bot`; `PUT /v1/pro/bot/config {version, config}` (409
  `stale_version`, 413 over 256 KB / 500 commands / 50 timers, 415); `POST
  /v1/pro/bot/{platform}/connect` → 201 `{auth_url}` (404, 409, 503 `platform_unavailable`);
  `DELETE /v1/pro/bot/{platform}` (revokes); `PUT /v1/pro/bot/enabled` (422 with no platform).
- Quotas: one channel per platform, 20 messages/min per channel, 50 timers.
- M7 as built (19 Oct 2026, API.md §9.7), Twitch and Kick: relay `bot/{mod,twitch,kick,lease,store,tests}.rs`,
  migration 10; core `cloudbot.rs` (the cloud answers with the desktop's own `command::run`, counters via the
  now-shared `chatbot::counter_op_for` + `KeyedCooldowns`, `autoreply::run` with the same `SendBudget`, `Timer::due`
  with the shared `cloudbot::timer_text`; built-in modules, points, quotes and OBS commands stay on the PC); engine
  `cloud_bot.rs`; server `routes/cloud_bot.rs`. **Twitch:** "Ferrocast Cloud" (`TWITCH_CLOUD_CLIENT_ID`,
  `TWITCH_CLOUD_CLIENT_SECRET`, `TWITCH_CLOUD_EVENTSUB_SECRET`; all three or off; optional
  `TWITCH_CLOUD_DESKTOP_CLIENT_ID`), two sign-in steps (the bot account, then the streamer's `channel:bot`; the same
  account twice is fine), **both tied to the PC's own Twitch account**: `connect` carries the desktop app's token
  (`owner_token`, added by the engine), the relay checks it with `oauth2/validate` (and that it's the desktop app's),
  signs that account into the state, and the channel step's callback refuses and revokes any other account. State:
  HMAC with an HKDF key derived from the cloud secret (never the secret itself), 10 minutes, single use (nonce in
  `pending_oauth`). A proven owner takes a channel another subscription still has (theirs unbound, subscriptions
  deleted, sign-in revoked, `twitch_channel_taken`). EventSub `authorization_revoked` / `user_removed` unbind the
  channel (subscriptions deleted, channel sign-in dropped). Pages show the PC code (HMAC of the install id, like M8's)
  and the bot page names whose chat it will speak in, with a warning when it isn't its own. One Conduit per relay with
  webhook shard 0 at `/v1/bot/twitch/eventsub` (checked hourly: GET, PATCH only when disabled or pointed elsewhere;
  its id kept in `bot_kv`, a new id subscribes every channel again), subscriptions `channel.chat.message` (bot as
  reader) + `stream.online/offline` (each id saved as made; a 409 is looked up), made when switched on and deleted
  when switched off or Pro lapses, replies with the app token. EventSub ids go in the relay's `seen` table (`tw:`
  prefix, a day, one `first_time`); webhook work runs in a bounded queue (1,024 waiting, 16 at once, drops counted),
  DB lookups off the async threads. Moderator scopes only with `moderation: true` (kept for M10). **Kick:** the
  existing broker with a `bot.` (or `bot.m.` with `moderation:ban`) state, so tokens stay sealed on the relay and the
  channel's owner mapping is untouched; needs Kick connected in Accounts (`kick_not_linked`) and the channel is the
  one the install owns in `kick_owners` (checked again at the callback; a stale binding of another subscription is
  removed and revoked); webhooks are handed over from `kick_webhook` into the same kind of bounded queue; refreshes
  one at a time per subscription (re-read and one retry when another refresh replaced the sealed token). **Errors:**
  only `*_signed_out` / `twitch_revoked` block a channel; refusals and Twitch being unreachable show with "Try again"
  (`connect {retry: true}`; the sweep retries `twitch_unavailable`) and a sent message clears them; dropped messages
  (`is_sent: false`, Kick 400) are kept as `last_drop`. **Own messages:** recognised by the sent message ids (256 per
  channel) or the exact text from the bot account within 5 s; the streamer's own lines are answered when the bot is
  their account. **Live:** `live_since` on the channel (Get Streams at switch-on and hourly), so timers and `{uptime}`
  survive a restart. **Lease:** in memory per subscription and platform, counted from relay start (a restart never
  jumps in front of a PC), switching on starts the 90 s again; `bot_seen {platforms}` names the PC's connected chat
  platforms (a plain `bot_seen` means all), so the cloud covers the others; every PC of the subscription follows the
  relay's switch every 5 minutes so a PC that never opened the panel still says `bot_seen`. A takeover seeds each
  timer's last post (no double post at handover); timers run as a task per channel, 16 at once. **Counters:** cloud
  changes become `bot_counter_changes` rows (`op` add/set with `delta`) + link items to each PC, which applies the op
  (a change on the PC meanwhile isn't overwritten); pushes carry `counters_seen` and newer rows are applied over a
  push in order. **Pushes:** definitions hashed (values ride along once a minute), 2 s debounce, 2 s to 5 min
  backoff, one re-push on `stale_version`. Rates: 120 requests/min per subscription (`bot_rate`), 20 messages/min per
  channel (`bot_messages_<platform>` minute counters).
  **Stored (privacy, R-51):** per platform the bot and channel account ids and logins, granted scopes, EventSub
  subscription ids, connected/last-reply times, the last error, the last dropped reply's reason and time, when the
  channel went live (the relay's Twitch conduit id in `bot_kv`); the refresh tokens sealed (AES-256-GCM, AAD
  `<sub>|bot_<platform>_<bot|channel>`), revoked at the platform on disconnect; the pushed config (commands, timers,
  auto-replies, counter names and values) and version; counter changes until a PC applied them (30 days at most).
  **Never stored:** chat. The last 200 lines per channel live in the relay's memory only (for M10) and the message ids
  for dedupe (2,048 per channel). Everything goes on disconnect or 30 days after Pro ends.
  **Threat model:** a forged or replayed EventSub call is refused (HMAC in constant time, 10-minute window, ids kept a
  day); the callback's account comes from Twitch, never the URL; a channel can belong to one subscription only (unique
  index) and only its proven owner binds it (the PC's own Twitch token, checked with Twitch; Kick's `kick_owners`); a
  state is used once; someone tricked into finishing another person's "bot" link sees whose chat it will speak in and
  the PC code, and is told to remove Ferrocast Cloud in Twitch's connections. Follow-ups:
  register the Twitch app (deploy/README.md); the cloud's own go-live and perk
  events to Discord (M8 follow-up); offline dashboard reads of the ring and moderation with the opt-in scopes (M10);
  YouTube (M12); the lease and ring are per relay process (fine for one relay); privacy/terms pages must list these
  items before sales open.

- M12 as built (19 Oct 2026, API.md §9.7 YouTube, a beta): relay `bot/youtube.rs` (+ `youtube_tests.rs`), no migration
  (units in `quota_usage`, period `p<Pacific date>`, owner `*relay` for the relay-wide count); `ferrocast_youtube::feed` (one
  `streamList` as batches of mapped events, shared gRPC + mapping with the desktop). Env `YOUTUBE_CLOUD_CLIENT_ID` +
  `YOUTUBE_CLOUD_CLIENT_SECRET` (both or off; a Google OAuth client "Ferrocast Cloud", type Web, redirect
  `/v1/bot/youtube/callback`), `YOUTUBE_CLOUD_DESKTOP_CLIENT_ID` (default the official desktop id),
  `YOUTUBE_CLOUD_DAILY_UNITS_PER_SUB` (1,500), `YOUTUBE_CLOUD_DAILY_UNITS` (9,000); `pro-keys.sh --youtube`, carried by
  `deploy-relay.sh`. **Decisions:** one sign-in step, the streamer's own channel (the cloud speaks as the channel, like the
  desktop; a separate bot account is a follow-up); scope `youtube.force-ssl` only (readonly adds nothing; posting needs it);
  the moderation opt-in is a flag on the same scope; the channel is proven by the PC's desktop token (`tokeninfo` client id
  = desktop app, `channels?mine=true` id = the callback's channel), state HKDF-signed, single use. Google's revoke ends the
  whole grant of that Google account for the client (a newer sign-in included), so the relay revokes only on Disconnect,
  the lapse sweep, or a sign-in for another channel: the moderation opt-in / connecting again for the same channel
  overwrites, a reclaim drops the other subscription's row without revoking, and a refusal revokes only a grant whose
  channel YouTube named and no subscription has bound (else it's left to expire).
  Chat read with the desktop's gRPC `streamList` (5 units per opened stream; `liveChatMessages.list` polling at 5 units per
  call would spend 1,500 in well under an hour), so a stream YouTube closes (after about 10 s, even while live) is opened again at once from
  the last page token (replies within about a second, about 1,800 units a live hour); only instant empty closes back off, 1 s doubling to 30 s; HTTP/2 keepalive (30 s ping, 10 s timeout, while idle
  too) and a 90 s idle timeout on the stream (reopened from the page token; a second silence in a row is a failure);
  failed opens back off 30 s doubling to 1 h, the streak kept in `YtMem` across reader restarts, 6 in a row →
  `youtube_unavailable` (retryable) and no reading until "Try again" or a live check 1 h later. Live detection
  `liveBroadcasts.list?broadcastStatus=active` (1 unit) only while the cloud holds YouTube's lease, in its own loop
  (JoinSet, 8 at once, never blocking Twitch/Kick timers): at once on takeover (after 2 min when the PC's `bot_seen` said
  `youtube_live: false`); offline it slows down: 2 min for 30 min after the takeover or the last live (or while the PC
  last saw it live), 5 until 2 h, 15 until 6 h, then hourly (about 65 units for an idle day); free wakes (at most every
  30 s): the PC's `youtube_live` turning true, Twitch/Kick going live for the same subscription, "Check now". Caps: live
  checks 150 units a sub a day and 30% relay-wide. Units counted once the sign-in is at hand, before each call, in one
  transaction (sub and relay-wide, totals and per call); a 401 keeps 1 unit and the retry counts again; over budget: no
  call. Reserves: replies, timers and moderation stop at `limit − reserve` (300 a sub, or a fifth of a smaller budget;
  10% relay-wide), reading and checks may use it; `youtube_quota` (non-blocking, `reset_at`, `relay_wide`) clears on the
  first pass after the reset; Google's own `quotaExceeded` stops every call until midnight Pacific (US DST rules, computed
  by hand), kept in `bot_kv` `yt_google_out` across restarts. Timers on YouTube: at most one post per 15 min
  (`YOUTUBE_CLOUD_TIMER_GAP_MIN`, 0 = off). Lease: the engine sends `lease: 2`; a frame with `platforms` but no `lease`
  (a PC before M12) covers YouTube too. WebSub (PubSubHubbub push on the channel's uploads feed) is not built: it fires
  on a new or scheduled video, not the go-live moment, and needs a public callback route, hub verification, an HMAC
  secret and 5-day lease renewal per channel; a follow-up as one more free wake signal. Replies, timers, dedupe, echo, 20/min, counters
  and the ring are the Twitch/Kick path (`say` → `youtube::send`, 200 characters). Offline moderation: delete any time, ban /
  timeout only while the relay has the live chat (409 `youtube_not_live`), 50 units each (429 `youtube_quota`);
  `cloud_moderation.youtube`; the engine records YouTube's `cloud_moderation` items. Hourly sweep refreshes the sign-in (no
  quota; refusal → `youtube_signed_out`, blocking); lapse revokes and forgets; refresh locks of subs without YouTube are
  pruned each pass. Engine: `DesktopYoutube` (the Accounts sign-in
  and live chat), `owner_token` for YouTube, `bot_seen {platforms incl. youtube, youtube_live}`. **Stored (privacy, R-51):**
  the channel's id and title, the scope, the sealed refresh token (AAD `<sub>|bot_youtube_bot`), units per Pacific day per
  call type (62 days); access token, live chat id and page token in memory only; chat never stored (the same 200-line ring).
  **Follow-ups (owner):** create the Google project/OAuth client and consent screen (deploy/README.md); `youtube.force-ssl`
  is sensitive: until Google verifies the app only up to 100 listed test users can sign in and their sign-ins expire after
  7 days; submit for verification (privacy policy listing these items, demo video, domain in Search Console) and later the
  YouTube API quota audit/extension. **Follow-ups (code):** bring-your-own Google project (not built: it needs per-sub
  client credentials sealed and a second callback flow); a separate YouTube bot account; WebSub push as a wake signal
  (above); the lease, ring and live chat id are per relay process; privacy/terms pages must list the new items before
  sales open.

### F2. Dashboard from anywhere
- Proxy to the PC when online; limited cloud mode (F1-backed) when offline.
- Tunnel socket `/v1/remote/tunnel` (not `/v1/link`), multiplexed `{t:"req"|"res"|"live"}`
  frames; engine `server/src/remote.rs` runs requests through the same axum router in-process
  with a `RemotePrincipal { device_id, role }`; allowlist in `server/src/routes/remote_scope.rs`,
  checked again on the relay.
- Phone: the website serves the dashboard at `https://ferrocast.clearcraftlabs.com/app/`
  (Cloudflare Pages); its API is `https://api.clearcraftlabs.com/r/…` (`RELAY_REMOTE_URL`,
  tunnel hostname), set by `/app/ferrocast-remote.js`. (Until 22 Oct the relay served the page
  itself at `remote.clearcraftlabs.com/app/`.)
- Pairing without accounts: Settings › Dashboard anywhere › Add a device / Invite a mod → QR +
  link with a one-time code (10 min, single use, hashed); the phone exchanges it for a device
  token (30-day sliding, hashed, revocable).
- Roles: `owner` (everything except accounts, secrets, tip secrets, Pro, export/import, media
  upload); `mod` (chat read, ban/timeout/delete/unban, held approve/reject, panic, viewer card).
- Offline: relay answers `GET chat/recent` (F1 ring), `POST moderation/ban|timeout|delete` (F1
  tokens), `GET history` (F7), cloud bot pause; everything else 503 `pc_offline` ("Your streaming
  PC is off. Chat, bans and history still work."). Config read-only offline.
- Tables: `remote_devices`, `remote_codes`, `remote_audit` (30 days).
- API: `POST /v1/pro/remote/codes` (409 at 5 devices + 10 mods), `GET|DELETE
  /v1/pro/remote/devices[/{id}]`, `POST /r/v1/pair` (uniform 404, 429 at 5/IP/10 min), `ANY
  /r/api/v1/*` (401, 403 `not_allowed_for_role`, 413 over 2 MB, 429 over 20 req/s, 503
  `pc_offline`, 504 `pc_timeout` after 15 s), `GET (WS) /r/api/v1/live`. Bodies never stored or
  logged.
- M6 as built (19 Oct 2026, API.md §9.5), online proxy only: relay `remote/{mod,limits,scope,store,
  tunnel,proxy,app,tests}.rs`, migration 7; engine `remote.rs` (the tunnel: opens only with Pro +
  `features.remote` and a paired phone or a waiting link, settings `remote_tunnel`; backoff 1 s to
  60 s; 32 requests at once; live frames per phone socket, marked owner-only when a mod may not see
  them); server `remote.rs` (`RemotePrincipal` extension, `oneshot` through the same router,
  `api_guard` checks the role; `PUT /settings` can't flip `lan_access`) and
  `routes/remote_scope.rs`. Devices belong to the PC (install), limits 5 own + 10 mods per PC with
  unused links counted. The phone page is served from `RELAY_DASHBOARD_DIR` (deploy-relay.sh copies
  `ferrocast-web/dist/dashboard` to `/opt/ferrocast-relay/dashboard/releases/<date>` and switches
  its `current` link, the folder mounted read-only): the relay
  image can't contain it because CI builds the image from this repo alone. Frame field `to` (a
  phone socket id) is used instead of `device_id` so one device can have two tabs. Reads aren't
  audited, only changes and refused changes.
  **Review fixes (19 Oct 2026):** the 30-day device token never goes in a URL: `POST /r/v1/ticket`
  gives a single-use 60 s live ticket (in memory, hashed) and a 5-minute signed media ticket that
  only opens `GET /media/{id}`; `?session=` is refused. At most 3 live sockets per device (a 4th
  closes the oldest with 4409) and opening one counts in the device's 20/s. Close codes 4401
  removed (the page stops, forgets the token, "Pair this phone"), 4402 Pro paused (stops, keeps the
  token; `/r/v1/me` 402 too), 4408 fell behind (taken out of the map at once, reconnects and
  refetches); a close travels on its own channel, never behind a full queue. The relay lets 32
  requests wait per PC (the engine's own limit; a test keeps relay ≤ engine) and never waits for
  queue room (503 `pc_busy` "Your PC is busy, try again in a moment."). The engine never awaits in
  its tunnel loop: requests (32 at once, else an instant 503 `pc_busy`, 14 s each else 504
  `pc_timeout`) and new phones' first frames (4 at once) run in the session's `JoinSet`, aborted
  when the session ends; the backoff starts over after a connection that held 30 s. Rate limits are
  token buckets (pairing: 5, then one every 2 minutes; IPv6 per /64) in a map capped at 100,000
  keys (oldest out, O(1)) and pruned once a minute. Refused `GET`s aren't audited, refused changes
  at most once per device and route a minute, and routes the PC doesn't know (404/405) or refused
  ones outside the known list are `{unknown}`. Code limits are counted and inserted in one
  statement. The config script goes right after the `<head …>` tag, never before the doctype.
  Media `Range` is passed through (cut to 4 MB on the PC; `Content-Range`/`Accept-Ranges` come
  back) so video previews stream under the 8 MB per answer. The phone's client refuses calls its
  role can't make before they're sent (`remoteAllowed`), so boot reads don't fill the audit; phones
  skip first-time setup and the update install button. The "PC is off" screen polls `/r/v1/me`
  quietly and changes only when the answer does. The link dialog counts down from `expires_in`.
  The phone dashboard is deployed as `dashboard/releases/<date>` with a `current` link swapped by
  one rename (`deploy/dashboard-swap.sh`, `RELAY_DASHBOARD_DIR=/dashboard/current`): the mounted
  folder itself is never renamed, which a Docker bind mount wouldn't follow.
  **Stored (privacy, R-51):** per device id, PC, subscription, role, name, token hash
  (HMAC-SHA256 with a server pepper: `RELAY_REMOTE_PEPPER`, else derived from `RELAY_VAULT_KEY`)
  and created/last-used/expiry times; per unused link its hash, role, name, 10 minutes; audit lines
  (method + route template, never ids or bodies; status; device id and name; time) for 30 days.
  Bodies, queries and live frames are in the relay's memory only while passing through (decision 4).
  **Threat model:** a stolen phone token reaches only that role's routes and is revocable at once
  (sockets close 4401); it never appears in a URL (tickets instead), so it can't leak through
  logs, history or `Referer`; a mod token can never reach settings, secrets or other screens (checked on
  the relay and again on the PC, which an attacker controlling the relay process can't skip without
  also faking the tunnel's install auth); a phone token can never pair more devices, see accounts,
  secrets, Pro or export data; codes are 10 symbols of 31 (~49 bits), single use, 10 minutes, 5
  tries per address (IPv6 /64) then one every 2 minutes, every miss the same 404; the token in the page lives in
  `localStorage` on the website's origin (`ferrocast.clearcraftlabs.com`), which serves only our
  own static build (the site and `/app/`, no viewer or user content) and gives `/app/` a strict CSP
  (no inline script, no third-party script, no framing, `connect-src` only the API host); requests
  carry the Bearer token plus the account cookie (`RELAY_REMOTE_LOGIN=required`), across sites
  with CORS allowed for that one origin only (with credentials), and any other `Origin` is refused; the relay only ever forwards
  to the paired PC's own tunnel (no URL from the phone is fetched, no SSRF) and only `/api/v1/*`
  with a plain path; 2 MB requests, 8 MB answers, 20 requests/s per device, 3 live sockets per device, 32 waiting
  per PC, 15 s timeout. The relay sees traffic in memory (E2E later, decision 4). Follow-ups: the Cloudflare
  tunnel needs the public hostname `api.clearcraftlabs.com` → `http://ferrocast-relay:8787`;
  the website deploys the phone's dashboard on every push to ferrocast-web (a version mismatch between the PC's engine and the phone's page is possible until the
  page is versioned); memory limit of the relay container (8 MB answers in flight); privacy/terms
  pages must list these items before sales open; offline mode (M10).
- M10 as built (19 Oct 2026, API.md §9.5 Offline): relay `remote/offline.rs` (+ `offline_tests.rs`), `bot/moderate.rs`,
  migration 11 (`remote_audit.source`, `pc`/`cloud`). With no tunnel, `proxy::api` hands the request (after the same
  401/402/429/403/413 checks) to `offline::answer`: `GET /chat` from the cloud ring (`bot::recent_events`; each kept line
  is now the mapped `Event` without `raw`, id `cl-<platform>-<message id>`, `seq` 0), `POST /mod/delete|timeout|ban` (the
  PC's bodies; ban is this platform only) via `bot::moderate::run`, `GET /pro/history` (`history::phone_list`, owners;
  added to `OWNER_ALLOWED` on the relay, the PC and in `packages/api`), `PUT /pro/bot/enabled` (`bot::phone_set_enabled`,
  the same switch). Chat and moderation need `bot::cloud_state(sub).on` (switched on, Pro, a ready channel the relay has);
  else 503 with the M6 message. Anything else: 503 `pc_offline` "Your streaming PC is off. Chat, bans and history still
  work." when the cloud is on. **Moderation:** the opt-in is now real: `connect {account:"bot", moderation:true}` over a
  channel without it re-runs the bot account's consent with the moderator scopes and keeps the channel (Twitch callback's
  COALESCE; Kick's broker with `bot.m.`); Kick's opt-in asks `moderation:ban moderation:chat_message:manage` (an older
  grant without the second can't delete: 403). Each action needs the scope for it (403 `needs_moderation_scope` with the
  hint to turn the opt-in on), counts in the channel's 20 a minute (`bot_messages_<p>`, 429 `cloud_rate`) and in the
  device's 20/s, and goes out behind the traits (`TwitchCloud::ban/delete_message` with the bot account's user token,
  refreshed from the sealed refresh token, a rotated one sealed again, cached in memory an hour, one retry on 401;
  `KickBot::ban/delete_message` with `kick::access`). Platform errors map to 403 `bot_not_moderator`, 422
  `platform_refused`, 404 `message_not_found`/`viewer_not_found`, 409 `platform_busy`/`cloud_signed_out`, 429
  `platform_rate`, 502 `platform_unreachable`, each with a hint; never a 500. Answered changes are audited with source
  `cloud` (refusals once a minute, as online). **Live socket:** `remote {pc_online, role, cloud, cloud_paused}` on open,
  tunnel up/down and when the switch changes (`remote::cloud_changed` from `set_enabled`); with the cloud on, a `resync`
  after it; `bot::on_chat` hands each kept line to `remote::cloud_chat`, which sends it as the PC's `event` frame to the
  subscription's phones whose PC has no tunnel (role-checked; a full queue closes 4408); a successful offline delete
  sends `message_deleted`. Phone sockets now carry their subscription. Handover: the PC's return sends `remote
  {pc_online:true}` and `live_open since 0` (the PC answers `hello` + `resync`), cloud lines stop; its loss sends `remote
  {pc_online:false, cloud:true}` + `resync`. `/r/v1/me` adds `cloud`, `cloud_paused`, `cloud_moderation {twitch, kick}`
  (on, opted in, every mod scope; `bot::can_moderate`) and `history`; so does the `remote` frame. Review fixes (19 Oct):
  offline bans/timeouts are queued for the device's PC as link items `cloud_moderation` and recorded by the engine
  (`Engine::apply_cloud_moderation`: ban list with `BanEntry.offline`, timeout count; no platform call; acked); 503
  words by role and capability (`offline::offline_message`); `device_ok` refuses a device whose PC's Pro moved to
  another subscription (401 `device_moved`, the socket closes 4401); one Twitch user-token refresh at a time per
  subscription (`Runtime::twitch_lock`, cache re-checked under it), a fresh token refused too → `twitch_signed_out`;
  "already banned/timed out" → success; field-only validation messages (≤ 200); `cl-` ids → 422 `invalid_message`;
  phone sockets indexed by subscription (`remote::Runtime::phone_subs`, cleaned lazily); `history::phone_list` checks
  `features.history`. Web: chat lines offer only what works (`lineActions`: Delete needs `source_id`, bans only where
  `cloud_moderation`), `remoteConnect` keeps `cloudPaused`/`cloudModeration`/`history`, the gate reloads on any change,
  limited → full runs `loadFull` (boot's load), History nav needs `history`, the strip's live region is the words only,
  Chat's height from the measured header (`--fc-top`), e2e 375 px vertical-fit check. **Dashboard:** `boot.ts` opens a
  limited connection when `me.cloud` (no gate), a module-level `remoteLink` (set at connect and by `remote` frames) drives
  the client's new `guard` (`remoteOfflineGuard`: the relay's 503 words without a request), modes switch in place, the
  gate gets "Switch cloud bot on" for owners when paused. **Stored (privacy, R-51):** nothing new about chat: the ring is
  memory only (200 lines per channel, the mapped events without the platform payload, gone on restart, on switch-off and
  on disconnect); the Twitch bot's access token for moderation lives in memory only (an hour); moderation bodies (ids,
  reasons) are never stored or logged; audit lines gain only `source`. Follow-ups: unban, the ban list, held messages and
  panic offline (need a cloud-side moderation log); `everywhere` bans offline would need linked identities on the relay;
  YouTube moderation with M12; the ring and the Twitch user token are per relay process.

### F3. Premium TTS (Google Cloud TTS)
- Relay `tts/{mod,fake,google}.rs`: `trait TtsProvider { fn voices(); async fn synthesize(text,
  voice) -> Result<Audio, TtsError> }`, `TtsError { Quota, Unavailable, BadVoice, TooLong }`,
  `FakeTts` for tests. Google key `GOOGLE_TTS_KEY` restricted to the TTS API.
- `POST /v1/pro/tts {text ≤ 300, voice}` → `audio/ogg`; `GET /v1/pro/tts/voices`. Errors 402, 404
  `voice_not_found`, 413 `too_long`, 429 `tts_quota` (+ `X-Quota-Reset`), 503 `tts_unavailable`.
- Quota 20,000 characters/month per subscription; `tts_usage(sub_id, month, chars)` only.
- Engine `speech.rs` gains cloud voices (`pro: true`); local approval and filters first (R-48);
  audio saved like the system WAV; fallback to the system voice with one notice.
- UI: "Ferrocast voices" group with a quiet Pro chip and usage.
- M4 as built (19 Oct 2026, API.md §9.4): relay `tts/{mod,provider,google,fake,store,tests}.rs`,
  migration 6 (`tts_usage`); `GoogleTts` (REST, Ogg Opus, key in the `X-Goog-Api-Key` header,
  6 s per call, 7 s per relay request) with 10 curated voices (hi-IN and en-IN Neural2 + WaveNet,
  female and male, then en-US Neural2 F/D). **Money safety:** 20 requests/min per subscription and
  60/min relay-wide (429 `tts_rate`), 20,000 characters/month per subscription (`tts_chars`, 429
  `tts_quota`), then `TTS_GLOBAL_MONTHLY_CHARS` (default 800,000, hard ceiling 1,000,000 = Google's
  free Neural2 tier) checked and counted in one statement before Google is called; once reached,
  503 "Ferrocast voices are resting until next month" and Google is never called until the 1st.
  Failed calls give the subscription back its characters; the relay-wide count only when Google
  refused before working (its 429, bad voice, too long), never after a timeout/5xx (maybe billed).
  `deploy-relay.sh` carries `GOOGLE_TTS_KEY` and `TTS_GLOBAL_MONTHLY_CHARS` over like the Razorpay
  keys (added on the server by hand, deploy/README.md). **Stored (privacy, R-51):** the per-
  subscription counter and one `tts_usage` row a month (characters and requests, everyone
  together); never the text, the voice or the audio. Engine `speech/cloud.rs`: only lines the
  local rules passed (R-48) are sent; `speech.ogg` saved and attached like the WAV; settings
  `tts_cloud {voice}` with the policy's system voice as the backup; 8 s overall, then the system
  voice with one `tts-cloud` notice; previews cached per voice per run. Follow-ups: set a ₹1 budget
  alert and lower the API's per-minute quota in Google Cloud too; Chirp 3 HD voices if the free
  tier allows later; privacy/terms pages must list the counters before sales open; the per-minute
  relay-wide limit is in memory (fine for one relay process).

### F4. Discord perks
- Relay `discord/{mod,api,perks,interactions}.rs`, REST only; env `DISCORD_BOT_TOKEN`,
  `DISCORD_APP_ID`, `DISCORD_PUBLIC_KEY`.
- `POST /v1/pro/discord/connect` → invite URL (bot + applications.commands; Manage Roles, Send
  Messages, Embed Links) with state; callback records `guild_id`.
- Viewer linking at `/d/{handle}/link` (viewer-started), Discord OAuth `identify connections`,
  token discarded. Kick via `/ferrocast link` slash command later.
- Rules `{kind: sub|sub_tier|member|tip_min, platform?, tier?, min_minor?, currency?, role_id,
  keep_days}`; perk events from the desktop (`POST /v1/pro/discord/perk-events`, queued
  offline) and from the cloud bot.
- Cloud go-live embed with thumbnail, edited at the end; desktop skips its free post when Pro
  go-live is on (30-minute rule).
- Tables `discord_guilds`, `discord_rules`, `discord_links`, `discord_grants` (hourly sweep).
- Errors 402, 403 `missing_permission` (50013: "Move Ferrocast's role above the perk roles."),
  404 `guild_not_found`, 409, 429 passthrough. Quotas 1 guild, 10 rules, 5,000 links.
- M8 as built (19 Oct 2026, API.md §9.6): relay `discord/{mod,api,perks,live,pages,interactions,store,
  tests}.rs`, migration 8 (`discord_guilds`, `discord_rules`, `discord_links`, `discord_grants`,
  `discord_posts`), REST only behind `DiscordApi` (fake in tests); env `DISCORD_APP_ID`,
  `DISCORD_BOT_TOKEN`, `DISCORD_PUBLIC_KEY`, `DISCORD_CLIENT_SECRET` (all four or `features.discord`
  is false; `deploy-relay.sh` carries them over). One OAuth redirect `/discord/callback` for the bot
  install (server taken from Discord's token answer, never the URL; state HMAC-signed with the client
  secret, 10 minutes) and viewer linking. Interactions endpoint verifies Ed25519 (`ed25519-dalek` 2,
  verify only) and answers PING. Engine `discord/pro.rs`: go-live reported to the relay, the free
  webhook post skipped only when the bot actually posted (the webhook is the fallback otherwise);
  perk events from subs, YouTube memberships and paid messages queued in settings `discord_pro`.
  **Stored (privacy, R-51):** the server id/name/icon hash, channel id, go-live switch and line, a
  random link code, the rules; per viewer who linked *themselves* (viewer-started, never posted by us)
  their Discord user id and the ids of their verified Twitch/YouTube connections, never names, email
  or the token (revoked right after two reads); per role given the user, role, rule and end; the last
  go-live post per platform for 30 days. Everything goes on disconnect or 30 days after Pro ends.
  Follow-ups: Kick linking via a `/ferrocast link` slash command (register commands); go-live
  `@everyone`/role ping option (the bot never pings today); YouTube/Kick preview images (needs the
  video id / Kick's thumbnail); removing given roles on disconnect (they stay today); the cloud bot
  (M7) sending perk events and go-live too; privacy/terms pages must list these items before sales open.
- M8 review fixes (19 Oct 2026): stream ends are queued with `ended_at` and retried; the relay
  treats a report starting 30+ minutes after the open post as a new stream (old post closed), the
  engine keeps reported streams across restarts and counts `recent` only for its own stream. Perk
  grants stand alone (`blocked`/`skipped`/`invalid` counts, 50001 = server gone, only 429/5xx stop
  a batch) and the view shows `perk_issue`; bad or clock-skewed events are skipped/clamped one by
  one. Grants carry `attempts`/`retry_at` (migration 8): backoff 1 h to a day, forgotten after 5
  failures, renewals put the role back. Live reports are serialised per subscription+platform and a
  post Discord accepted counts even if saving fails. The bot leaves a server it can't keep (no Pro,
  one server already). When Pro ends (19 Oct 2026 decision) the first hourly sweep takes off every
  perk role the bot gave (`discord_grants.paused_at`, migration 13; 500 a sweep, a rate limit stops
  the round, per-member failures back off and after 5 the role is left on); the grants that haven't
  ended go back on when Pro returns (activation webhook or the next sweep); the bot itself still
  leaves 30 days after Pro ended. Install phishing: the "connected" page and the view show a PC code (HMAC of
  the install id). The perk queue removes by sequence number. Interactions hex is strict. Web:
  currency exponents mirror `money.rs`; the Pro section is hidden in remote mode.

### F5. Cloud backup and sync
- Engine `cloud_backup.rs` + `store/src/pack.rs`: reuse `.ferropack` (no secrets), encrypt with
  `age` (X25519 identity in `ferrocast-secrets` as `cloud-backup-key`, optional passphrase).
  "Save your recovery key" confirmed before the first upload.
- Upload in 8 MB parts: `POST /v1/pro/backups` → `{id, part_size}`, `PUT
  /v1/pro/backups/{id}/parts/{n}` (octet-stream), `POST /v1/pro/backups/{id}/complete` (409
  `hash_mismatch`). `GET /v1/pro/backups`, `GET|DELETE /v1/pro/backups/{id}`. Errors 402, 404, 409,
  413 `backup_quota` (250 MB / 7 backups, oldest automatic pruned), 415, 429 (6 uploads/day).
- Blobs in `BlobStore`; table `cloud_backups`. "Sync" in v1 = automatic upload after the daily
  local backup when changed + "Use your newest backup from <other PC>" on a new install.
- M3 as built (19 Oct 2026, API.md §9.3): relay `backups/{mod,store,tests}.rs`, migration 5
  (`cloud_backups`, `cloud_backup_parts`), parts as blobs `backups/<sub>/<id>.part<n>` joined into
  `backups/<sub>/<id>` on complete; `backups::sweep` (spawned in `main.rs`, hourly) deletes uploads
  a day after their last part and everything 30 days after Pro ends; list/download/delete stay
  open meanwhile (R-04). Engine `cloud_backup.rs`: age 0.12 (no default features), the file is
  `ferrocast-backup v1` + an optional `passphrase <age scrypt of the key>` line + the age X25519
  file, because age forbids mixing an scrypt recipient with others; the passphrase therefore
  wraps the key inside each backup. State lives in `<data>/cloud-backup/state.json` (a file, so
  it's never in a pack and a restore can't bring another PC's state). `store::pack::content_hash`
  skips unchanged days. **Stored (privacy, R-51):** per backup the id, the uploading install id,
  size, SHA-256 of the ciphertext, app version, reason (automatic/manual) and dates, and the
  encrypted file; the relay never has the key, the passphrase or any plaintext. On the PC: the key
  in the secret store (never in settings, packs or logs). Follow-ups: the "other PC" is unnamed
  (no device names are sent; a user-chosen label could come later); packs over 250 MB once
  encrypted (big videos) can't go up (the message says to use Export); move to R2 behind
  `BlobStore` before many subscriptions; raise the relay's memory limit (joining streams 64 KB
  chunks, so memory stays flat); privacy/terms pages must list these items before sales open.

### F6. Tip page upgrades
- `tips.rs` → `tips/{mod,brand,goal,domains}.rs`.
- Branding `PUT /v1/pro/tips/brand {accent, background, text}` (auto-fix contrast < 4.5:1),
  `PUT /v1/pro/tips/brand/{avatar|banner}` (png/jpeg/webp ≤ 1 MB / 3 MB, re-encoded to webp,
  EXIF dropped), served at `/t/a/<sha>.webp` immutable. Page stays script-free with the same CSP.
- Goal `PUT|DELETE /v1/pro/tips/goal {title, target, currency, ends_at?}`, page
  `<tip host>/<handle>/goal`; `tip_goals` + `tip_goal_payments` (refunds subtract, R-32; other
  currencies not counted; rows deleted 60 days after the goal ends).
- Custom domain via Cloudflare for SaaS (`CF_SAAS_TOKEN`, `CF_ZONE_ID`): subdomains only, 1 per
  subscription, free up to 100 hostnames; errors 409 `domain_taken`, 422 `apex_not_supported` /
  `invalid_hostname`, 503 `domains_full`, 502 `cloudflare_unavailable`.
- M2 as built (19 Oct 2026, API.md §9.2): branding and goal are per install (its tip page);
  migration 3. **Stored (privacy, R-51):** three colours and two image hashes per install; the
  images as lossless WebP re-encoded from pixels only (no EXIF/GPS/ICC/XMP), shared by hash;
  goal title, target, currency, dates; per counted tip only `(goal_id, SHA-256(install|provider|
  payment id), amount_minor)`, never a name, message or payment id. Payment rows go with the goal
  60 days after it ends; a lapsed install's branding and goal go 30 days after the hourly sweep
  first sees the lapse (`tips::sweep`, spawned in `main.rs`). Uploads: 30 a day per subscription
  (`tip_images`), one decode at a time, ≤ 4096 × 4096 pixels. Goal page `?overlay=1` for OBS.
  Follow-ups: lossless WebP keeps photos large (a 1500 × 500 banner can be ~1 MB; lossy WebP needs
  libwebp); privacy/terms pages must list the new stored items before sales open.
- M9 as built (19 Oct 2026, API.md §9.2 "Your own address"): relay `tips/domains/{mod,hostname,cloudflare,proof,tests}.rs`,
  migration 12 (`tip_domains`; M10 took 11, committed first; 12 has no dependency on M10's code). `trait CustomHostnames`
  (boxed futures, like `Mailer`): `CloudflareHostnames` (`CF_SAAS_TOKEN` + `CF_ZONE_ID`, both or off; 10 s; HTTP DCV, DV,
  TLS 1.2+, no `custom_metadata` (Enterprise only); 409/1406 → duplicate, hostname-specific 14xx 400/422 →
  `Refused` (logged, streamer gets our words), zone-level (401/403, 7000/7003/1001/1003/auth codes, "not entitled/enabled",
  404 on create) → `Zone` → 503 `domains_unavailable` logged once until a call works, 429/5xx/other → 502; token in `Debug`
  redacted) and `FakeHostnames`. **Per-claim proof:** TXT `_ferrocast.<hostname>` = `ferrocast-proof-` + HMAC(install,
  hostname) keyed by HKDF(`RELAY_VAULT_KEY`, own label; `bot::derive_key`), read over DNS-over-HTTPS (`trait ProofDns`:
  `DohResolver` on `cloudflare-dns.com` JSON, 5 s; `FakeDns`), answers cached 30 s; live = Cloudflare active + TXT matches;
  only the hourly sweep counts misses, 3 → `proof_missing` (not served) until found again. Without the vault key the
  feature is off. Per install (its tip page), one per subscription, 100 on the relay (`Runtime` cap; only live + unfinished
  younger than 7 days count), adds serialised by one async lock; a Cloudflare duplicate (found by exact name) is adopted
  only when `_ferrocast.<hostname>` holds this install's proof right now, read fresh (only the DNS owner can publish
  it); else 409 `domain_taken` "set up somewhere else on Cloudflare" with `proof {type,name,host,value}` in the body,
  passed whole through the engine (`Engine::pro_call_passing`) and server, kept on the web `ApiError.body`, and shown
  by the dashboard with copy buttons and Try again;
  a failed DB save deletes the new hostname again. **Squatting:** a row not live 7 days after added/last live is deleted
  (Cloudflare + DB) by the sweep; a PUT takes over a hostname whose row isn't live and is over 24 h old (the other's
  Cloudflare hostname and row deleted; they just see no address). `quotas.tip_domain_changes` 10/day (adds refused over
  it, removals counted but never refused). Status `pending | verifying | live | failed | proof_missing`; GET checks at most
  once a minute (a claim on `claimed_at`; `checked_at` moves only when Cloudflare answers). **Serving:**
  `route_custom_hosts`, a `from_fn_with_state` layer on the whole router, fails closed: Ferrocast's hosts (relay, tip,
  remote and their registrable domains, `*.clearcraftlabs.com`, IPs, single labels, `localhost`; computed once, `OnceLock`
  in `Runtime`, warmed in `main.rs`) pass untouched; a live address gets `/`, `/goal`, `/t/a/*` only; every other host gets
  "Not set up yet" (404, script-free) on every path; an unreadable `Host` (HTTP/2 `:authority` used) → plain 400; no `Host` at all (only the container's own HTTP/1.0 health check sends that) is answered as ours. Host
  cache: generation counter (a lookup stores only if no change came in between), addresses 30 s in one bounded map
  (pending evicted before live), unknown hosts 5 s in another. The sweep re-checks every row, live ones too.
  Validation: punycode via `url::Host`, apex by the full PSL (`psl` crate, ICANN + private: `github.io`, `pages.dev`,
  wildcard rules), each Unicode label single-script and not confusable (UTS #39 skeleton, `unicode-security`) with our
  domains' names (`clearcraftlabs`); `hostname_unicode` shown. **Lapse:** 7 days after the hourly `tips::sweep` first sees
  it, then deleted from Cloudflare (kept on a Cloudflare error) and the DB; shorter than the 30 days for other data because
  a hostname is a shared, capped slot and keeps serving a page. Engine/server `/api/v1/pro/tips/domain` pass the relay's
  200/201 through (`Engine::pro_call_status`; new codes added to the engine's known list); remote: owner phones may GET
  only (`OWNER_ALLOWED` on relay, PC and web). Web: the dashboard polls every 15 s while pending/verifying and the panel is
  visible, shows both records with copy buttons (the TXT as a required step), "Checked … ago" at every width, and only
  warns for an obvious bare domain (the relay judges). Deploy: `pro-keys.sh --domains`, carried by `deploy-relay.sh`;
  README has the owner's Cloudflare steps incl. the TXT proof. **Stored (privacy, R-51):** hostname, Cloudflare id and
  statuses, optional TXT records and messages, the proof's last-seen time and miss count, dates; nothing about visitors. Follow-ups: the tunnel's catch-all must point at the relay (deploy/README.md)
  and be checked with a real address; a stuck `failed` address can only be removed and re-added
  (a Cloudflare "retry validation" PATCH could come later); rotating `RELAY_VAULT_KEY` changes every TXT value;
  privacy/terms pages must list the hostname before sales open.

### F7. Stream history and reports
- Engine `history.rs` uploads recap totals at stream end and a 5-minute viewer sample while
  live; the relay also records platform online/offline when the PC is off (F1).
- Tables `streams(...)`, `stream_samples(stream_id, at, viewers)`.
- API `POST /v1/pro/history/streams` (idempotent upsert), `POST /v1/pro/history/samples` (≤ 100),
  `GET /v1/pro/history?from&to&platform`, `GET /v1/pro/history/export.csv`. Errors 402, 409
  `ended_stream_locked`, 413, 429.
- Weekly email (Brevo): `mail/{mod,fake,brevo}.rs`, `trait Mailer`, double opt-in, encrypted
  address, one-click unsubscribe, spread by `hash(sub) % 7`.
- UI `#/history`: range chips, plain-SVG charts, per-stream table, weekly email toggle.
- M11 as built (19 Oct 2026, API.md §9.8): relay `mail/{mod,mailer,brevo,fake,report,pages,weekly,
  store,tests}.rs`, migration 9 (`mail_addresses`, `mail_sent`, `mail_budget`). `BrevoMailer` (transactional API,
  `api-key` header, 10 s) behind `trait Mailer`; `features.weekly_email` needs `BREVO_API_KEY`, the
  vault and a link key (`MAIL_TOKEN_KEY`, else from `RELAY_VAULT_KEY`). Double opt-in: the confirm
  email is sent first and the address saved only once it went (a failure never replaces a working
  address); the link (`c1.…`, HMAC, 24 h, single use, a resend replaces it) opens a script-free page
  whose button POSTs the confirm, so mail scanners confirm nothing; 3 confirm emails per
  subscription per day. A new address waits in its own (pending) columns while the confirmed one
  keeps getting the report; confirming swaps them; the sweep drops only the pending part; Stop
  removes both; GET shows both (`address_hint`, `pending_hint`). Each report carries
  `List-Unsubscribe` + `List-Unsubscribe-Post` (RFC 8058); the unsubscribe link carries the
  address's tag (HMAC of the normalized address with the link key), so saving the same address
  again keeps old links working and another address never matches ("This link is out of date",
  still 200). Relay-wide daily budget `MAIL_DAILY_BUDGET` (default 270, Brevo free is 300) shared
  by confirms and reports: a confirm over it is 503 `mail_busy` "Try again tomorrow."; the run
  stops at it and gives claims back for the next day. **Schedule:** day `SHA-256(sub_id) % 7` (stable, unlike std's seeded
  hasher), 04:00 UTC (09:30 IST), up to 2 days late after an outage (last week's moment counts
  early in the next week), only to addresses confirmed before that moment and only with Pro;
  **weeks without a stream send nothing** (decided: skip, not a "no streams" email; re-checked
  until 2 days after the moment so late uploads count). `mail_sent` is claimed before each send
  and the row re-read after the claim (at most once, even across restarts). Brevo: no connection
  / 503 gives the claim back; a read timeout / other 5xx keeps it (counts as sent); a 400 about
  the recipient pauses the address, but two different refused in a row stop the run (second not
  paused); other 4xx (our account or request) stop the run, give back, log; 402 / 403-limit / 429
  stop for the day. A vault that opens nothing pauses nothing (give back, log, stop); only a row
  that alone fails is paused `unreadable`. The dashboard shows the day in the viewer's time zone.
  **Stored (privacy, R-51):** the addresses only sealed (AES-256-GCM, AAD `sub_id|mail_address`)
  plus each one's tag (keyed HMAC, for unsubscribe links), never plain, never logged (`Email`'s `Debug` and every error leave it out; Brevo's error text is
  dropped, only its code kept); status, saved/confirmed times, the confirm nonce's SHA-256 and
  expiry; per (subscription, week) when the report went (120 days); emails per UTC day (7 days).
  Deleted on Stop emails or
  unsubscribe, a week after an unconfirmed link expired, 30 days after Pro ended (hourly sweep).
  The report itself is built from the history already kept (§9.1) and never stored. Remote: an
  owner's phone may `GET /pro/history/email` (`OWNER_ALLOWED` on the relay and the PC). Deploy:
  `pro-keys.sh --mail`, carried by `deploy-relay.sh`. Follow-ups: Brevo hard-bounce/complaint
  webhook (pauses addresses Brevo blocks later); Brevo's free plan sends 300 a day (spread over 7
  days that's ~2,100 subscriptions; plan or second sender before then); a "no streams" email if
  streamers ask; per-streamer time zone for the send hour; privacy/terms pages must list the
  address before sales open; the History screen is hidden on phones, so the owner-phone read is
  only reachable from the API today.

## Milestones (A and B can run in parallel after M0)

| # | Milestone | Owner |
|---|---|---|
| M0 | Foundations (gate, vault, quota, blob, migrations, route stubs, status features, ProChip/ProPanel) | A alone |
| M1 | History (no email) | A |
| M2 | Tip branding + goal | B |
| M3 | Cloud backups | A |
| M4 | Premium TTS (Google + fake) | B |
| M5 | Local auto-replies (free) | B |
| M6 | Remote dashboard (online proxy) | A |
| M7 | Cloud bot Twitch + Kick + lease | A |
| M8 | Discord go-live + perks | B |
| M9 | Custom tip domain | B |
| M10 | Offline remote mode (built 19 Oct) | A |
| M11 | Weekly email | B |
| M12 | YouTube cloud bot (beta, built 19 Oct) | A |

Ownership after M0: A owns `relay/src/{history,backups,remote,bot}`, `server/src/remote*`,
`engine/{history,cloud_backup,cloud_bot,relay}.rs`; B owns `relay/src/{tips,tts,discord,mail}`,
`core/autoreply.rs`, `engine/{speech,discord}.rs`, `engine/studio/chatbot.rs`. Shared files are
append-only (API.md §8.x, `packages/api` exports, `App.tsx` routes, `relay.env.example`).

Every milestone: Rust unit + relay integration tests through `router()` with fakes, `packages/api`
types + demo feature + vitest, a Playwright spec covering 402, offline and 375 px, API.md and
STATUS.md updates, CodeRabbit (or an equal manual review).

## Gates

Before `PRO_SALES_OPEN=true`: M0–M4 and M6–M7 live; pricing/FAQ/features list exactly what
ships; privacy, terms and refunds updated for every new stored item; lapse tested end to end in
Razorpay test mode; nightly off-box relay backup + restore drill; load test (200 fake
subscriptions); second Twitch app registered, Discord app, TTS and mail providers set up;
security review of vault, tunnel and pairing; R-04 end-of-life pledge for cloud features.

Before v1.0.0 stable: free features complete, 24-hour soak (R-40), N-1/N-2 update tests (R-62),
a signing decision (R-61), version 1.0.0 tagged; site `latestStable()` and stable-first download
page; Pro chip copy and `LAUNCH_NOTE` updated.

## Open decisions for the owner

1. TTS: Google Cloud (chosen). Quota per subscription vs the shared free tier.
2. Twitch: register a second confidential app "Ferrocast Cloud".
3. YouTube cloud bot: ship last or as a beta; apply for the quota audit.
4. Remote proxy: relay sees proxied traffic in memory only; E2E later.
5. Custom domains: 100 free hostnames cap.
6. "Sync" means backup and restore in v1.
7. Discord tipper roles need an optional, viewer-started "Link Discord".
8. Consider a separate Always-Free VM for Ferrocast.
