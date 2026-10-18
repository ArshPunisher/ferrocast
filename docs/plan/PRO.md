# Ferrocast Pro: build plan

Approved by the owner on 19 Oct 2026: seven Pro features, all of them things that run on our
servers. Free features never move behind Pro (R-01, R-05). Google Cloud TTS is the chosen voice
provider; a Discord app "Ferrocast" exists (id 1561463863197835294); Brevo is the mail sender.

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
- YouTube: last (M12); refresh token in vault, gRPC `streamList` only while live; shared 10k/day
  quota: 1,500 units per subscription per day, bring-your-own-key option, apply for quota audit.
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

### F2. Dashboard from anywhere
- Proxy to the PC when online; limited cloud mode (F1-backed) when offline.
- Tunnel socket `/v1/remote/tunnel` (not `/v1/link`), multiplexed `{t:"req"|"res"|"live"}`
  frames; engine `server/src/remote.rs` runs requests through the same axum router in-process
  with a `RemotePrincipal { device_id, role }`; allowlist in `server/src/routes/remote_scope.rs`,
  checked again on the relay.
- Phone: relay serves the dashboard at `https://remote.clearcraftlabs.com/app/`
  (`RELAY_REMOTE_URL`, tunnel hostname), same origin, API base injected as `window.__FERROCAST__`.
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
| M10 | Offline remote mode | A |
| M11 | Weekly email | B |
| M12 | YouTube cloud bot | A |

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
