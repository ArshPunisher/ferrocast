# Pro pre-sales gates: load test and lapse end to end

Two of the gates in [PRO.md](PRO.md#gates) before `PRO_SALES_OPEN=true`: a load test with 200
fake subscriptions, and lapse tested end to end in Razorpay test mode. Both ran locally on
19 Oct 2026, against fakes only: never the live relay, no real keys, no money.

## 1. Load test (200 fake subscriptions)

### How to run it

```sh
export PATH="$HOME/.cargo/bin:$PATH"
LOAD_SECS=600 LOAD_OUT=/tmp/load.txt \
  cargo test -p ferrocast-relay --release --lib -- --ignored load_ --nocapture --test-threads=1
```

`crates/relay/src/load_tests.rs`, two `#[ignore]` tests, so CI never runs them:

- `load_200_subscriptions` (the driver) starts the fakes that live outside the relay: the
  testkit's `FakeRazorpay` (test-mode key, plans) and a fake Kick (OAuth token, users,
  public key, with a real 2048-bit RSA key so webhook checks cost what they do in
  production). It then runs **the same test binary again as a child process** with only
  `load_child_relay`. The child serves the real `router()` on 127.0.0.1 with a temporary
  SQLite database, an `FsBlobStore`, the vault, and fake providers: `FakeTts`, Twitch, Kick
  and Discord (identities spelled out in the OAuth codes, so the driver needs no back
  channel), `FakeMailer`, `FakeHostnames` and `FakeDns`. RSS and CPU are sampled with `ps`
  every 2 s, so they are the relay's own, not the load generator's.
- The child also runs `bot::run` (cloud bot timers) and every sweep from `main.rs`
  (housekeeping, tips + custom domains, backups, remote, Discord, mail, bot) every
  `LOAD_SWEEP_S` seconds (default 60, so 60 times more often than production).
- A few test-only routes on the child (`/__load/*`): the confirm links the fake mailer
  kept, marking a custom address live at the fake Cloudflare and DNS, the weekly email run
  at a chosen moment, and counters from the fakes.
- Everything else goes through the public HTTP API only. Relay logs go to a file at `warn`,
  which is searched afterwards for `database is locked`, `relay database error` and
  `dropped webhook work`.

### Setup (all through the API)

| What | How many |
|---|---|
| Subscriptions | 200: `POST /v1/installs` → `POST /v1/pro/checkout` → `FakeRazorpay.activate` → signed `subscription.activated` webhook |
| Installs | 399 (1 to 3 per subscription; the others joined with `POST /v1/pro/restore`) |
| Cloud bot | 100 subscriptions: Twitch connected in two steps (bot account, then the channel, tied to the PC's own token), config pushed, switched on (300 EventSub subscriptions); 50 also Kick through the relay's broker |
| Dashboard from anywhere | 50 PCs with a tunnel, 75 phones paired by code (owners and mods) |
| Tip pages | 30 with brand colours, an avatar (PNG re-encoded to WebP) and a goal; 10 of them on their own address (live at the fake Cloudflare with the TXT proof) |
| Discord | 40 servers connected, one sub rule each |
| Weekly email | 200 addresses saved and confirmed through the link in the email |
| History | one stream per subscription |

Setup took 0.4 s (16 at a time).

### The busy hour (10 minutes at real rates)

- Every install holds its `/v1/link` socket and acks every item (15,072 items acked:
  Kick events and the cloud bot's counter changes). Half the cloud-bot PCs say `bot_seen`
  every 30 s (the PC answers chat, the cloud stands by); the other half are silent, so the
  cloud takes over after 90 s and answers.
- 50 tunnels; each phone holds a live socket and makes 2 requests a second through the
  tunnel (`/status`, `/chat`): 150 requests a second.
- 100 Twitch channels get an HMAC-signed EventSub chat notification every second (one in
  five a command); 50 Kick channels a signed webhook every 2 s.
- Tip pages: 5 visits a second (the page, the goal page, and the custom address by `Host`).
- Every subscription every 5 minutes: a history sample and a stream update, `/v1/pro/status`
  on every install, the bot panel and a config push (cloud bot subscriptions), Discord perk
  events (Discord subscriptions), the device list (tunnel PCs), the goal (tip pages).
- Ferrocast voices: 40 a minute across all subscriptions (the relay-wide cap is 60).
- 10 cloud backups of 20 MB (8 MB parts) at random moments.
- The weekly email run halfway, for the subscriptions whose day it is (33 sent of 200,
  about one seventh).
- Every sweep every minute (10 runs).

### Numbers (10-minute run, release build, MacBook (Apple silicon, 10 cores), relay child on 4 tokio workers)

| Route group | Requests | Per second | p50 ms | p95 ms | p99 ms | Max ms | Statuses |
|---|---|---|---|---|---|---|---|
| `phone_proxy` (`/r/api/v1/*` through a tunnel) | 90,082 | 150.0 | 0.4 | 1.4 | 8.4 | 27.6 | 200 |
| `eventsub` (`/v1/bot/twitch/eventsub`) | 60,100 | 100.1 | 7.9 | 15.5 | 20.5 | 28.0 | 204 |
| `kick_webhook` (`/v1/kick/webhook`) | 15,013 | 25.0 | 0.8 | 1.7 | 3.0 | 15.1 | 200 |
| `tip_page` (`/t/<handle>`, goal, own address) | 3,003 | 5.0 | 0.4 | 11.8 | 14.6 | 18.0 | 200 |
| `api_history` | 802 | 1.3 | 0.9 | 1.7 | 5.7 | 13.5 | 200 |
| `api_status` (`/v1/pro/status`) | 799 | 1.3 | 0.3 | 0.6 | 1.8 | 12.0 | 200 |
| `api_tts` | 401 | 0.7 | 4.1 | 14.7 | 16.2 | 17.3 | 200 |
| `api_bot` (view, config push) | 400 | 0.7 | 0.4 | 0.7 | 1.6 | 2.8 | 200 |
| `api_discord` (perk events, view) | 160 | 0.3 | 0.4 | 0.6 | 0.7 | 0.9 | 200 |
| `api_remote` (device list) | 100 | 0.2 | 0.3 | 0.5 | 0.9 | 1.1 | 200 |
| `api_tips` (goal) | 60 | 0.1 | 0.3 | 0.7 | 1.0 | 3.6 | 200 |
| `api_backups` (start, list) | 20 | – | 0.3 | 0.4 | 0.4 | 0.4 | 200, 201 |
| `phone_ticket` | 75 | – | 2.8 | 4.1 | 4.2 | 4.5 | 200 |
| `backup_part` (8 MB PUT, bulk) | 30 | – | 9.6 | 11.2 | 12.0 | 12.0 | 200 |
| `backup_complete` (join + SHA-256 of 20 MB, bulk) | 10 | – | 50.4 | 52.5 | 52.5 | 52.5 | 201 |
| weekly email run (33 reports) | 1 | – | 28.8 | | | | |

Total ≈ 282 requests a second plus 399 link sockets, 50 tunnels and 75 phone sockets.
What the fakes saw: 5,113 Twitch replies and 1,267 Kick replies from the cloud bot (timers
and commands), 401 TTS calls, 233 emails (200 confirms + 33 reports), 306 Discord calls.

| Resource | Value |
|---|---|
| RSS at start / after setup / peak | 14 MB / 97 MB / 150 MB (still rising slowly at the end, see the soak) |
| CPU | 0.09 cores on average, 0.13 at the busiest 2 s |
| Slowest sweep | housekeeping 14 ms, tips 7 ms, bot 6 ms, mail 2 ms, the rest < 1 ms |
| Relay warnings / errors in the log | 0 / 0 |

**30-minute soak** (same load, `LOAD_SECS=1800`): passed every criterion. 0 5xx in
≈ 512,000 answers; p95 phone proxy 1.3 ms, EventSub 16.9 ms, Kick 2.0 ms, tip pages 12.5 ms,
TTS 16.1 ms, history 1.8 ms; 0 locked, 0 relay warnings. RSS 97 MB after setup, 140 MB at 5
min, 150 MB at 13 min, 166 MB at 30 min: still creeping (about 0.5 MB a minute at the end)
while the `seen` table grows (225,000 webhook ids by the end; housekeeping keeps them a day,
its slowest run 36 ms). One stall of about 180 ms hit every group at the same moment (825 s
in) without a log line or a slow sweep; most likely the laptop, but worth watching on the
server. CPU stayed at 0.09 cores.

### Pass criteria

| Criterion | Result |
|---|---|
| No 5xx | **Pass**: 0 of ≈ 170,000 answers in the busy hour, 0 in setup |
| p95 < 200 ms for API routes | **Pass**: worst API p95 is 15.5 ms (EventSub); bulk backup work is 11 ms per 8 MB part and 52 ms to join and hash 20 MB |
| Memory < 700 MB (container limit 1 GB) | **Pass**: peak 150 MB in 10 minutes, 166 MB after the 30-minute soak |
| No SQLite "database is locked" | **Pass**: 0 (also 0 `relay database error`, 0 dropped webhook work) |

### What the first runs found

- **Socket storms on connect (harness, not the relay).** The first 10-minute run had 63,653
  `503 pc_offline` answers to phones: 34 of the 50 tunnels (and 175 of the 399 link sockets)
  never opened. All 450 sockets were opened in the same instant and macOS's listen backlog
  (`kern.ipc.somaxconn` = 128) reset the rest. The engine reconnects with a backoff
  (1 s doubling), so the harness now does the same (0.2 s doubling, 8 tries; retries are
  counted, a socket that never opens fails the test). In the run above 34 link sockets and 2
  tunnels needed one retry and all opened. Production sits behind cloudflared and Linux's
  backlog is larger, but see the follow-up about a reconnect storm after a relay restart.
- **Data of a `halted` subscription was never deleted** (found by the lapse test, below).

### Caveats

- A MacBook is much faster than the production VM (an Oracle Ampere VM shared with
  TransformX and Hookline): 0.09 cores here leaves a lot of room, but latency there will be
  higher. macOS fsync is also cheaper than a cloud block volume's; the relay commits a write
  on most requests (`touch_install` on every authenticated call, `seen` on every webhook).
- RSS is macOS's allocator, not glibc's in the container.
- YouTube's cloud bot (beta) was not in the load: its chat is a gRPC stream per channel,
  which would need a fake gRPC server.
- Rates are a "busy hour" as specified, compressed to 10 minutes (30 for the soak); the
  sweeps run every minute instead of every hour.

### Follow-ups

1. **Run it on the server** (or an ARM VM like it) before sales open, against a scratch
   container (not the live relay): the same test binary works there with `LOAD_SECS`.
2. **Reconnect storm after a deploy**: all PCs reconnect their links and tunnels at once;
   the engine's backoff has jitter, but check the container's `net.core.somaxconn` and the
   tunnel's connection limits.
3. **Writes on every request**: `authed()` writes `last_seen` on every call and every
   webhook inserts into `seen`, all through one `Mutex<Connection>`. Fine at this load;
   `touch_install` at most once a minute per install and `PRAGMA synchronous=NORMAL` (safe
   with WAL) are the cheap wins if the server's disk makes p95 climb (PRO.md M0 already lists
   the writer + read pool split).
4. EventSub's p50 (7.9 ms) is ten times Kick's (0.8 ms), although Kick's webhook does more
   writes (`seen` and the PC's queue); EventSub's `seen` write goes through `spawn_blocking`. Not a problem at 100/s; worth a look before
   Twitch traffic grows.

## 2. Lapse end to end

`crates/relay/src/lapse_tests.rs`, three normal tests (they run in CI, about 0.5 s
together). Everything goes through the real `router()` with `FakeRazorpay` (signed webhooks,
the real checkout), the fake Twitch, Kick and YouTube, the fake Discord, `FakeMailer`,
`FakeTts`, `FakeHostnames` + `FakeDns`, an `FsBlobStore` and the vault. The 7- and 30-day
clocks are moved by backdating the stored times (`current_end`, `lapsed_at`), as the
feature tests do; the mail sweep takes `now`.

**Setup, shared by all three:** checkout → `subscription.activated`, then every Pro feature
through its own routes: a history stream and samples; a tip page with brand colours, avatar
and goal; a custom address with the TXT proof, live at Cloudflare and served on its `Host`; a
backup uploaded and completed; a TTS call; a phone paired by code; the cloud bot on Twitch
(two steps), Kick (broker) and YouTube (channel proven), config pushed and switched on, an
EventSub `!lurk` answered; Discord connected with a rule, a linked viewer and a perk event
that gives the role; the weekly email saved and confirmed.

| Test | What it checks |
|---|---|
| `lapse_halted_then_recovered_within_the_grace_gets_everything_back` | `subscription.halted`: 23 gated routes answer 402 `pro_required` with the exact message and a hint to Settings › Pro; the phone gets 402 (Pro paused) and stays paired; the history CSV, the backup list and a full download still work; the tip page loses its branding but the custom address keeps serving. The next hourly sweep stops the cloud bot (EventSub subscriptions deleted, chat not answered) and revokes or deletes nothing yet. With the lapse mark 7 days old the address is removed from Cloudflare and the host answers 404. Reactivated (`subscription.activated`): Pro is back and history, goal, backups, branding, email, Discord, TTS and the phone work again; the next sweep subscribes Twitch again and the cloud answers. (The custom address stays removed after 7 days, by design.) |
| `lapse_cancelled_deletes_everything_after_30_days_and_a_new_subscription_starts_clean` | Cancelled with the period over: the same 402s, phone and exports. 31 days on: all 14 Pro tables empty and no blobs left; Twitch (both sign-ins), Kick and YouTube revoked at the fakes; the Discord bot left the server; the address and the email address gone; export and backups now 402. A new checkout on the same PC starts clean (no history, backups, bot config, Discord, email, address or branding); the old phone gets 401 `device_moved`. |
| `lapse_a_halted_subscription_rechecked_daily_is_still_deleted_after_30_days` | Regression test for the bug below. |

### Bug found and fixed

**A `halted` (or `paused`) subscription's data was never deleted.** The 30-day rule in
backups, the cloud bot, Discord, mail and history counted from
`access_until(..).or(refunded_at).unwrap_or(synced_at)`. A halted subscription has no access
end, so it fell back to `synced_at`, and the daily `billing::reconcile` re-syncs every halted
and paused subscription (so does `/v1/pro/status?refresh=1`): `synced_at` moved every day and
"30 days after Pro ended" never came. History, backups, the cloud bot's sealed tokens, the
Discord server and email addresses of a streamer whose card failed would have stayed forever
(against R-04/R-51 and the privacy promise).

Fix: `SubRow::stopped_at()` in `crates/relay/src/billing/store.rs` (the access end, else the
refund, else `current_end` + the 3-day late-renewal grace; the last sync only for a
subscription that was never charged; `current_end` is kept across syncs by `COALESCE`), used
by the five callers (`backups/mod.rs`, `bot/mod.rs`, `discord/mod.rs`, `history/store.rs`,
`mail/weekly.rs`). Tests: `billing::tests::pro_stops_at_the_end_of_access_never_at_the_last_sync`
(unit) and the third lapse test above (fails without the fix).

### Gaps and follow-ups

- **A new checkout within the 30 days loses the subscription's data.** Recovery within the
  grace works when Razorpay reactivates the same subscription. A streamer who cancels and buys
  again gets a new `sub_id`: history, backups, the cloud bot, Discord and the email (keyed by
  subscription) stay on the old id and are deleted on schedule, and phones get 401
  `device_moved`; branding, goal and the address (keyed by install) carry over. Carrying them
  over means moving rows to the new id, and the sealed tokens and email addresses use `sub_id`
  in their AAD, so they would have to be sealed again. A design decision for the owner (or
  say so on the pricing/FAQ page).
- Discord roles already given stay on members after the bot leaves (a known M8 follow-up).
- Kick and YouTube have nothing on the relay to unsubscribe, so "the cloud bot stops" is
  asserted through Twitch's EventSub subscriptions and through no replies.
- Most sweeps read `now_ms()` themselves; a `now` parameter on each (as mail has) would let
  tests move the clock without editing stored times.
