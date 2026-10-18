# Status

Newest entry first. Update at the end of every working session.

## Now

- **v1 feature-complete.** Engine, adapters (Twitch, YouTube, Kick, OBS), tips through the relay, export/import, plus community features: loyalty points, chat polls, giveaways, first-time chatter marks, stream recap, Discord go-live posts, OBS auto-detect and starter commands.
- **Relay is live** at ferrocast-relay.clearcraftlabs.com (Docker on the Oracle server behind its own Cloudflare tunnel).
- **Pro billing built, not on sale** (18 Oct): Razorpay Subscriptions in the relay (`crates/relay/src/billing`), Pro status in the engine (`/api/v1/pro`), `deploy/razorpay-keys.sh` for keys and plans. `PRO_SALES_OPEN=false` until Pro's features exist (ARCHITECTURE.md §10).
- **Betas are public** (18 Oct): beta.2 and beta.3 published, beta.4 building. Razorpay is set up in test mode (website verified, keys, plans, webhook).
- **First real stream test** (18 Oct, private YouTube via OBS): found and fixed three real bugs. (1) Two rustls backends in the app and none chosen: every `wss://` task panicked, so Twitch EventSub never connected and the relay link (Kick events) stayed off; `install_tls()` now picks ring at start. (2) OBS auto-detect never ran in the real app (`start` didn't set the settings paths). (3) YouTube only looked for a live broadcast every 10 min; OBS starting a stream now wakes it, with quick re-checks for a minute. Also: Kick's connect dialog now closes by itself.
- **Platform apps are registered** (Twitch, Google/YouTube, Kick); ids are in the release workflow's variables and secrets.
- **Desktop:** Spark icons, the overlay-collage install window, LAN access, and signed auto-updates (key in GitHub secrets, feed is `latest.json` on GitHub releases).
- **Importers:** StreamElements (JWT, GET only, never stored) and Streamlabs (exported CSV/XLSX files), with a preview, keep/replace for clashes, a backup first and a report. Points for people not seen yet wait and are paid when they show up.
- **Security:** imported packs can't choose media types or change device settings; media is served sandboxed; relay sign-ups are capped and unused installs pruned.

## Waiting on the owner

1. See `../NEEDS-FROM-YOU.md`.

## Next

- Try the StreamElements importer on an account with real commands and points (the test account is empty), and a real Streamlabs Chatbot Currency export.
- Live tests against the real platforms (Twitch `!so` game lookup, Discord delivery).
- Panic-mode "hold new accounts" needs account ages from each platform.

## Log

### 2026-10-19 (Pro M6: dashboard from anywhere, online proxy)
- Relay: `remote/{mod,scope,store,tunnel,proxy,app,tests}.rs`, migration 7 (`remote_devices`, `remote_codes`, `remote_audit`). `/v1/remote/tunnel` (install auth + Pro, one per PC, pings 25 s, Pro rechecked every 5 min), `POST /v1/pro/remote/codes` (10-minute single-use code, peppered hash, 409 `too_many_devices` at 5 own / 10 mods), `GET|DELETE /v1/pro/remote/devices[/{id}]` (no Pro needed), and on `RELAY_REMOTE_URL`'s host only: `/r/v1/pair` (uniform 404, 5 tries/IP/10 min), `/r/v1/me`, `/r/v1/forget`, `ANY /r/api/v1/*` (401/402/429 `remote_rate`/403 `not_allowed_for_role`/413/503 `pc_offline`/504 `pc_timeout`), `/r/api/v1/live` (plus the relay's `remote {pc_online}` frame), `/app/` from `RELAY_DASHBOARD_DIR` with the injected config script and a strict CSP. Audit of changes and refusals, 30 days (`remote::sweep`, hourly). `remote` is now `pub mod` (the server test compares the rule tables).
- Engine `remote.rs` (tunnel with backoff, `RemoteHandler` trait, per-socket live hello/replay and role-marked frames), server `remote.rs` (`RemotePrincipal` + `oneshot` through the router; `api_guard` checks the role) and `routes/remote_scope.rs`; `/api/v1/pro/remote`, `/pro/remote/codes`, `/pro/remote/devices/{id}`. `server` gains `tower` (util) and `async-trait` as normal deps.
- Deploy: `deploy-relay.sh` writes `RELAY_REMOTE_URL`/`RELAY_DASHBOARD_DIR`, keeps `RELAY_REMOTE_PEPPER`, copies `../ferrocast-web/dist/dashboard`; compose mounts `./dashboard:/dashboard:ro`. API.md §9.5, PRO.md F2 "as built" with the threat model.
- Tests: relay 21 (`remote::scope` 5, `remote::tests` 16 through `router()` with a fake PC on a real tunnel socket: every status, pairing, limits, host/origin, expiry and sliding, forget, live frames by role, reconnect, revoke closes sockets, audit, sweep, the phone page), server 6 end to end (relay + engine + server in-process: requests through the tunnel, role checks on both sides, `lan_access`, live frames by role, the tunnel coming back after a cut connection, the PC's own refusals without the relay, the two rule tables equal).
- Follow-ups for the owner: Cloudflare tunnel public hostname `remote.clearcraftlabs.com` → `http://ferrocast-relay:8787`; `pnpm build:embed` in ferrocast-web before `deploy-relay.sh`.

### 2026-10-19 (Pro M8: the Ferrocast bot for Discord)
- Relay: `discord/{mod,api,perks,live,pages,interactions,store,tests}.rs`, migration 8. Connect (signed state, server from Discord's token answer), channel and go-live, rules (10, role-order check → 403 `missing_permission`), viewer linking at `/d/<code>/link` (identify + connections, ids only, token revoked), perk events (≤ 100) → roles with an end, hourly `discord::sweep` (ended roles, lapsed servers after 30 days), go-live embed posted and edited at the end, `/discord/interactions` (Ed25519, PING). Discord 50013 → 403, 429 → `Retry-After` passed through, 404 guild → `guild_not_found`. `ed25519-dalek` 2 added (cargo deny ok). API.md §9.6.
- Engine `discord/pro.rs`: go-live through the bot with the webhook as fallback (skipped only when the bot posted), stream end with peak, perk queue (`discord_pro`, backoff, restart-safe); `DiscordView.pro_go_live`; 403 now passes through `relay_error`. Server: `/api/v1/pro/discord…` routes.
- Deploy: `DISCORD_*` carried over by `deploy-relay.sh`, documented in `relay.env.example` and deploy/README.md; new `deploy/tests/deploy-relay-env.test.sh`.
- Tests: relay 35 (`discord::tests`), engine 6 (`discord::pro_tests`), server 1 end-to-end. Owner: set the redirect URI and interactions URL in the Discord portal, then add the four keys on the server (deploy/README.md).
### 2026-10-19 (Pro M4: Ferrocast voices, Google Cloud TTS)
- Relay `tts/` (replaces the stub): `TtsProvider` (boxed futures), `GoogleTts` (REST, Ogg Opus, key in a header, never logged, redacted `Debug`), `FakeTts` (records calls, can fail or be slow); `GET /v1/pro/tts/voices`, `POST /v1/pro/tts` (401/402, 400/415, 422 `empty_text`, 413 `too_long`, 404 `voice_not_found`, 429 `tts_rate` / `tts_quota`, 503 `tts_unavailable`). Migration 6 `tts_usage` (relay-wide monthly cap, default 800k, ceiling 1M). `AppState::with_tts`, `main.rs` reads `GOOGLE_TTS_KEY` / `TTS_GLOBAL_MONTHLY_CHARS`; `tts` is now `pub mod`.
- Engine `speech/cloud.rs`: cloud voices in `/api/v1/tts/voices` (`pro: true`), `GET|PUT /api/v1/tts/cloud`, `POST /api/v1/tts/preview`; `speak_to_media` goes through `speak_line` (Ferrocast voice, else system voice with one notice; 8 s). Server `routes/tts.rs` (the old `bot::tts_voices` moved there). `pro.rs` passes the TTS codes through.
- Deploy: env example, `deploy-relay.sh` carries the two variables over, README section.
- Tests: relay 17 (unit, a fake Google, every route status, per-sub quota, global cap never calls the provider, rates, give-back, nothing but counters stored), engine 9 against the real relay with `FakeTts` (approved text only, fallback notice once, lapsed Pro, month used up, 8 s timeout, preview cache), server 1.
- Follow-ups: in PRO.md F3 "as built".

### 2026-10-19 (Pro M5: local auto-replies, free)
- Core `autoreply.rs`: rules (`contains`/`word`/`starts_with`/`regex`, case, platforms, roles, live-only, cooldowns), NFKC + case folding, word boundaries, regex via the `regex` crate with size/nest caps and a pointed error (`at character N`), "matches every message" refused, first match wins, per-rule-per-platform and per-viewer cooldowns (5 s floor), a per-platform send budget (R-19) with pause on the platform's rate limit, starters. `Access` accepts `subscriber`/`vip`/`mod`. New deps: `regex`, `regex-syntax` (already in the lockfile).
- Store migration `0011_autoreplies.sql` + `repo::autoreplies` (ordered list, reorder).
- Engine `studio/autoreplies.rs`: CRUD, order, Try it (saved list or a draft), `{channel}`/`{game}`/`{title}`/`{uptime}` variables, the pipeline hook after moderation and after commands (`answer_command` now says whether the message was a command), loop guard (streamer, connected account, bot role, known bots, echo of its own replies for 2 min), quiet when the bot is off or panic is on. Server `routes/autoreplies.rs` (400/404/409/413/422). API.md §4 "Auto-replies".
- Tests: core 19, store 1, engine 8, server 5.
- `pack.rs` `TABLES` includes `"autoreplies"`, so Export/Import and cloud backups carry the rules; the engine reloads them after an import or restore.

### 2026-10-19 (Pro M3: cloud backups)
- Relay: `backups/{mod,store,tests}.rs`, migration 5; `POST /v1/pro/backups` (201 `{id, part_size, parts}`, 409 `upload_in_progress`, 413 `backup_quota`, 429 `backup_upload_quota` at 6/day via `quota.rs`), `PUT …/parts/{n}` (octet-stream only, 413 over 8 MiB, 422 `invalid_part`), `POST …/complete` (size + SHA-256, 409 `hash_mismatch` deletes, 409 `parts_missing` keeps, prunes the oldest automatic backups to stay within 7 / 250 MB), `GET` list, `GET {id}` (streamed ciphertext), `DELETE`. Read routes open after a lapse while kept (R-04); `backups::sweep` hourly. `features.backups` true with a file store. `backups` is now `pub mod` (for `main.rs`).
- Engine: `cloud_backup.rs` (age X25519 key in `ferrocast-secrets` as `cloud-backup-key`, optional passphrase wrapping the key with age scrypt inside each backup, 8 MiB part upload that resumes after a restart, automatic upload after the daily backup when `pack::content_hash` changed, restore = download → hash check → decrypt → existing import + reload, adopting the key on a fresh PC). Hook in `backups::run`. Server: `routes/cloud_backups.rs` (`/api/v1/pro/backups…`). `pro.rs` passes the new relay codes through. API.md §9.3, PRO.md F5 "as built".
- Review fixes: the key is made once under a lock and "saved" is bound to its public key (a changed key means not saved, nothing uploads); downloads use connect + 60 s idle timeouts instead of a total one; Back up now always packs fresh; restore tries keys before the passphrase; `pro_status` and `history_upload` are never exported and an import never replaces them (`pack::DEVICE_LOCAL_SETTINGS`). +5 tests.
- Tests: relay 11 through `router()` (every route's 401/402, parts in any order, hash mismatch, missing parts, 413/415/422, one upload at a time, quota 429, pruning, 503 without storage, lapse + sweep), engine 8 against the real relay (round trip, ciphertext only on the relay, wrong key/passphrase fail cleanly and import nothing, automatic skip, resume after a cut-off upload, 402, offline), store 1 (`content_hash`), server 1 end to end. Workspace: 582 passed; clippy `-D warnings`, fmt, `cargo deny` clean.
### 2026-10-19 (Pro M2: tip page branding and goal page)
- Relay: `tips/brand.rs` (`PUT|GET|DELETE /v1/pro/tips/brand`, colours with automatic contrast fixes reported in `changes`; `PUT|DELETE /v1/pro/tips/brand/{avatar|banner}` raw PNG/JPEG/WebP ≤ 1 MB / 3 MB, ≤ 4096² px, decoded with `image` (png, jpeg, webp only; pure Rust), turned upright, shrunk to 512² / 1500×500, lossless WebP from pixels only, stored by SHA-256 in `BlobStore`, served `/t/a/<sha>.webp` immutable on both hosts; 30 uploads/day `tip_images`), `tips/goal.rs` (`PUT|GET|DELETE /v1/pro/tips/goal`, public `/t/<handle>/goal` and `<tip host>/<handle>/goal`, `?overlay=1` for OBS; counts verified tips in the goal's currency from `queue_tip`, refunds/disputes subtract, refund-first never counted). Migration 3 (`tip_brand`, `tip_goals`, `tip_goal_payments`). Tip page uses the branding and goal card only while Pro; `tips::sweep` (hourly, `main.rs`) drops ended goals after 60 days and lapsed branding after 30. `features.tip_brand` = file store configured. API.md §9.2.
- Engine `tip_pro.rs` + `pro_raw` in `pro.rs`; server `routes/tip_pro.rs` (`/api/v1/tips/brand…`, `/api/v1/tips/goal`; images checked for type/size first).
- `deploy/razorpay-keys.sh`: same EXIT-trap restore as `vault-key.sh` (a failed write puts relay.env back, temp file and curl credentials removed, values reach awk via the environment); new failing-`cat` cases in `tests/razorpay-keys.test.sh`.
- Tests: relay 7 integration (every route through `router()`: 401/402/400/404/413/415/422/429/503, pages, counting, sweep) + 9 unit; server 2 end-to-end.
### 2026-10-19 (Pro M1: stream history, no email)
- Relay: `history/{mod,store,tests}.rs`, migration 4 (`streams` UNIQUE(sub_id, platform, key), `stream_samples`); `POST /v1/pro/history/streams` (idempotent upsert, 201/200, 409 `ended_stream_locked` a day after the end), `POST /v1/pro/history/samples` (1-100, 413 `too_many_samples`), `GET /v1/pro/history` (streams + weekly), `GET /v1/pro/history/export.csv` (formula-safe; open after a lapse while history is kept, R-04). All behind `require_pro`, 60 requests/min per subscription (`quota::Period::Minute`, 429 `history_rate`), 400/415/413/422 with `{error}`. Housekeeping deletes history 30 days after Pro ends and samples after 400 days. `features.history` is true.
- Engine: `history.rs` counts each live platform while Pro (remembered status) is active, uploads the stream at start and end plus a viewer sample every 5 minutes; the queue lives in settings `history_upload` with backoff (30 s to 15 min); restored streams end where last seen. Server: `GET /api/v1/pro/history` and `/export.csv` (`routes/history.rs`). API.md §9.1.
- Tests: relay 10 history tests through `router()`, engine 8 (fake relay: down, 402, 409, restart), server 2 end-to-end.
### 2026-10-19 (Pro M0: foundations)
- Relay: `billing/gate.rs` (`require_pro` → 401 / 402 `pro_required`, same access rule as status), `vault.rs` (AES-256-GCM, `RELAY_VAULT_KEY`, AAD `install|purpose`, 503 `vault_unavailable` without a key), `quota.rs` (per-subscription day/month counters, 429 + `X-Quota-Reset` + `Retry-After`), `blob.rs` (`BlobStore`, `FsBlobStore` at `RELAY_BLOB_DIR` or `<db dir>/blobs`), numbered migrations in `migrate.rs` (`user_version`; the live tables are migration 1, a copy is taken before migrating), per-module `routes()` with stubs for history, backups, remote, bot, tts, discord (mail has none), `tips.rs` → `tips/mod.rs`. `/v1/pro/status` gains `features` (all false) and `quotas`.
- Engine: `ProStatus` passes `features`/`quotas` through; relay 402/413 and its `Retry-After` reach the dashboard. API.md §8 status shape and new §9 (the gate and shared errors).
- Deploy: `deploy/vault-key.sh` (+ `tests/vault-key.test.sh`, in CI and `tools/check.sh`); `deploy-relay.sh` keeps `RELAY_VAULT_KEY`; `relay-ci-setup.sh` installs the script. Follow-ups in PRO.md.

### 2026-10-18 (designs)
- Overlay design system, phase 1 (API.md §4 Designs): `core::design` and `core::layer` (alert looks per event key with fallbacks and conditional variations, themes, scenes, widget styles; every check names the field; CSS can't fetch files), `layer` widgets and widget rotation/opacity/lock/hide, an overlay's `design` link with the branding check (a built-in keeps its mark). Store: migration 0009 (`designs`, `overlays.design`), designs in `.ferropack`, `.ferrodesign` share files (`store::designfile`, strict 413/415/422). Engine: built-in catalogue read from the overlay bundle's `designs/catalog.json`, the active alert design dresses alerts and `?test` previews, its TTS switch and paid-amount minimum on top of the policy, Use, Duplicate, designer test alerts with a design and an amount. Server: `/api/v1/designs` routes. Tests: `docs/TESTING.md`.
### 2026-10-18
- Owner-reported fixes. Secrets on macOS move from the Keychain to a private `secrets.json` in the data folder (`FileSecrets`; 0700/0600, atomic writes, damaged file = clear error, never wiped), because ad-hoc signed updates made macOS ask for the login password (R-45 rewritten). Old Keychain items are not read: macOS users reconnect once. OBS: a background connection follows OBS's own settings until the streamer types different details (`auto` on `/obs`, `in_use` on `/obs/detect`), retries every 3 s, re-reads OBS's file each attempt, and sends `status` frames on every change. Notices carry a `key` so the dashboard replaces instead of stacking (`panic`, `account:*`, …).
- StreamElements and Streamlabs importers (P9.2, P9.3): `/api/v1/import/streamelements/preview`, `/import/streamlabs/preview`, `/import/apply`. Migration 0008 (`pending_points`, `imported_tips`; both in `.ferropack`, imported tips in `tips.csv`). Preview tried against the owner's real StreamElements account (fresh, so empty).

### 2026-10-17
- Community features: loyalty points (watch time, support bonuses, `!points`/`!addpoints`/`!removepoints`, leaderboard, in `.ferropack`), chat polls (`!vote N`, live results, poll overlay), giveaways (keyword entry, point cost, sub bonus, OS-random draws, refunds), first-time and returning chatter marks with an optional welcome, the stream recap, Discord go-live posts (webhook in the keychain), OBS auto-detect, and the starter command pack (`!so` names the last game on Twitch). Migrations 0005 to 0007.

### 2026-05-27
- Tips through the relay with verified webhooks; export/import; TTS synthesis.
### 2026-05-24
- Kick adapter and relay with RSA-verified webhooks and the OAuth broker.
### 2026-05-19
- YouTube adapter on gRPC `streamList`.
### 2026-05-06
- Twitch adapter, EventSub handover, end-to-end engine test against a fake Twitch.
