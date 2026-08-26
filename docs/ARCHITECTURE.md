# Architecture

## 1. Shape

```
┌──────────── Ferrocast desktop (one process) ─────────────────────────────┐
│  Tauri 2 shell: tray, one window, single instance, autostart, updater    │
│     the window just loads http://127.0.0.1:<port>/app (dashboard SPA)    │
│                                                                          │
│  engine (tokio)                                                          │
│   ├─ adapters: twitch (EventSub WS + Helix) · youtube (gRPC streamList)  │
│   │            kick (events via relay, Helix-like REST) · obs (ws v5)    │
│   ├─ event bus: normalise → dedupe → store → rules → fan-out             │
│   ├─ alert dispatcher (persisted queue, ack from overlays)               │
│   ├─ bot (commands, timers, counters) · moderation (filters, panic)      │
│   └─ tts (system voices to WAV, approval queue)                          │
│                                                                          │
│  server (axum on 127.0.0.1 only)                                         │
│   ├─ /app/…           dashboard SPA (embedded build of ferrocast-web)    │
│   ├─ /api/v1/…        dashboard API (bearer session token + Origin)      │
│   ├─ /o/<token>/…     overlay pages + /o/<token>/ws (render-only)        │
│   └─ /media/<id>      sounds and images, with Range                      │
│                                                                          │
│  store: SQLite (WAL), numbered migrations, snapshots before updates      │
│  secrets: private 0600 file (macOS), OS keychain (Windows, Linux)        │
└───────────────┬──────────────────────────────────────────────────────────┘
                │ outbound WSS only (no inbound ports), optional
┌───────────────▼──────────────── Ferrocast relay (Oracle ARM, Rust) ──────┐
│  Kick webhooks (RSA verify, timestamp window, replay table) → desktop    │
│  Kick OAuth broker (holds the client secret)                             │
│  Tip pages + payment webhooks (Ko-fi, Razorpay, Stripe) → desktop        │
│  Pro: licence checks, cloud bot, sync (later)                            │
└──────────────────────────────────────────────────────────────────────────┘
```

**Why one local HTTP server for everything:** OBS needs URLs for overlays anyway. Serving the dashboard from the same server means the desktop window and the web version are the same app talking HTTP, with no Tauri IPC (which leaks memory at chat volume) and nothing to keep in sync between two transports. The Tauri window is a thin view (R-53, R-54).

**Why a relay at all:** Kick only delivers events to a public URL, Kick's token exchange needs a client secret, and payment providers need a public webhook. Everything else stays local (R-01). If the relay is down, Twitch and YouTube keep working and Kick events wait in the relay queue.

## 2. Crates

| Crate | Depends on | Does |
|---|---|---|
| `ferrocast-core` | serde, time | Event model, dedupe, templates, filters, alert rules and queue state machine, commands, goals, word filter, panic plan. **No I/O, no async.** Everything here is a pure function of its inputs so it is cheap to test. |
| `ferrocast-store` | core, rusqlite | Migrations, repositories, snapshots, export/import packs. |
| `ferrocast-twitch` | core | Device-code sign-in, EventSub WebSocket session, Helix (chat, moderation, users), payload → `Event`. |
| `ferrocast-youtube` | core | Sign-in (loopback + PKCE), `liveChatMessages.streamList` over gRPC, REST for sending and banning. |
| `ferrocast-kick` | core | Payload → `Event` for relay-delivered webhooks, REST for chat and bans. |
| `ferrocast-obs` | core, obws | Connection supervisor, "add browser source to the current scene", scene state. |
| `ferrocast-engine` | all of the above | The event bus, supervisors, bot runtime, moderation actions, alert dispatcher, TTS, relay link. |
| `ferrocast-server` | engine | axum routes, the overlay WebSocket protocol, auth, embedded web assets. |
| `ferrocast-relay` | core | The hosted relay binary. |
| `ferrocast-testkit` | core | Fake EventSub server, fake OBS, signed Kick webhooks, fixtures. Dev-only. |
| `apps/desktop` | engine, server | `ferrocast` binary: Tauri shell, or `--headless` for servers. |

Layering is enforced by `cargo deny` bans: nothing below `apps/desktop` may depend on Tauri.

## 3. The event model

Every platform becomes one envelope (`core::event`):

- `Event { id, seq, platform, channel, source_id, occurred_at, received_at, origin, kind, raw }`
- `origin`: `Live | Backfill | Replay | Test`. **Only `Live` fires alerts by default**; `Test` fires only on the overlay that asked; `Backfill` goes to the feed only (R-31).
- `kind`: chat (message, deleted, cleared), audience (follow, raid, viewer count), support (subscription, gift batch, platform gift, paid message, tip, reversal, member milestone), community (reward redemption, poll, prediction, hype train), moderation (ban, unban, held message, chat mode), stream (online, offline, info, ad break), connector status, and `Unknown` with the raw payload.
- Money is exact: `Money { minor, currency }`. Platform currencies (Bits, KICKs) are `PlatformValue` and never mixed with money (R-32).
- Viewer text arrives as `fragments` (text, emote, mention, link), never HTML (R-37).
- Every event has a dedupe key `(platform, source_id)`, or a hash when the platform gives no id. The bus drops repeats before anything else sees them (R-29).

## 4. Overlay protocol

- URL: `http://127.0.0.1:<port>/o/<overlay-token>/`. The token is random (128 bits), per overlay, revocable (R-43, R-44).
- WebSocket `/o/<token>/ws`, JSON frames `{ v: 1, seq, type, data }`.
  - On connect the server sends `snapshot` (overlay config + current state: goals, labels, last events) so nothing is blank (R-38).
  - Then `event`, `alert`, `config`, `clear`.
  - The overlay sends only `ack { alert_id, phase: started|finished|failed }` and `hello { since_seq }`. **It can't create events** (R-36).
- One socket per overlay page, paused when OBS hides the source.

## 5. Alert pipeline

1. Bus delivers a deduped `Live` event.
2. `core::alert::select` picks the alert rule and variation (conditions on amount, tier, months, platform, text).
3. The rendered alert (text from the template, media ids, duration, TTS text if allowed) is written to the `alert_queue` table, **then** dispatched (R-31: persisted).
4. The dispatcher sends it to every overlay subscribed to alerts and waits for `finished` or the max duration.
5. Queue controls: pause, skip, replay, priority, staleness policy after an outage.
6. A `Reversal` (refund, chargeback) undoes goal and leaderboard changes linked to the original event.

## 6. Security

- Server binds `127.0.0.1` only; LAN is an opt-in toggle (R-43).
- Dashboard API: bearer session token generated per app run, delivered to the Tauri window by an init script; `Origin`/`Host` checked (anti DNS-rebinding).
- Tokens and secrets in `ferrocast-secrets` (R-45): on macOS a private `secrets.json` in the data folder (folder `0700`, file `0600`, atomic writes), on Windows and Linux the OS keychain via `keyring`. Never in SQLite, settings, exports, backups or logs. macOS skips the Keychain because unsigned (ad-hoc) builds get a new code identity with every update, so the Keychain would ask for the login password after each one.
- No client secrets in the binary for Kick; YouTube and Twitch use public/installed-app clients injected at build time, never committed (R-46).
- Scripts and imported widgets run sandboxed with declared capabilities (R-47).

## 7. Storage

SQLite in WAL mode, a small pool, numbered migrations in `crates/store/migrations`, each with a test. Before any migration or update the store copies itself to `backups/<date>.db` (R-34). Tables: `settings`, `accounts` (no secrets), `overlays`, `alert_rules`, `media`, `events`, `alert_queue`, `viewers`, `viewer_identities`, `ledger`, `commands`, `timers`, `counters`, `quotes`, `filters`, `bans`, `tips`.

## 8. Web version

The same `server` + `engine` run multi-tenant behind the relay later (Pro). The dashboard SPA is identical; only the base URL and sign-in differ.

## 9. Repos

- `ferrocast` (this repo): Rust engine, server, relay, desktop shell.
- `ferrocast-web`: dashboard SPA, overlay bundle, website, Remotion videos, shared UI kit. Its build output (`dashboard/dist`, `overlays/dist`) is embedded into the desktop binary at build time (`FERROCAST_WEB_DIST`, defaults to `../ferrocast-web/dist`).
