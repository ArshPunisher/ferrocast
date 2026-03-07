# Steps

Each step is one focused commit (or a few) and names its test. Tick only when the test exists and passes. `R-xx` = the rule in [LESSONS](../LESSONS.md) the step follows. Frontend steps live in `ferrocast-web/docs/plan/STEPS.md`.

## P0 Foundations
- [x] 0.1 Repo, licence, readme, commit-msg hook
- [x] 0.2 Vision, architecture, plan
- [ ] 0.3 Cargo workspace with empty crates, pinned toolchain — test: `cargo build`
- [ ] 0.4 `tools/check.sh` (fmt, clippy, tests, deny) + CI on macOS, Windows, Linux

## P1 Core (pure, test-first)
- [ ] 1.1 Event envelope, kinds, actors, money, fragments, samples for every kind — test: serde round-trip + sample coverage
- [ ] 1.2 Dedupe (key from source id or hash, bounded window) — R-29 — test: repeats dropped, TTL expiry
- [ ] 1.3 Templates (`{name}`, `{amount}`, filters, escaping) — test: rendering, unknown variables, escaping
- [ ] 1.4 Conditions and alert rules with variations — test: selection by amount/tier/platform, priority
- [ ] 1.5 Alert queue state machine (enqueue, start, ack, timeout, skip, pause, staleness) — R-31 — test: ordering, timeout, condense after outage
- [ ] 1.6 Goals and totals with reversals — R-32 — test: per-currency sums, refund reverses
- [ ] 1.7 Word filter with lookalike folding, caps/links/symbols/repeat rules — R-48 — test: evasion cases
- [ ] 1.8 Commands: parser, aliases, roles, cooldowns, variables — test: matching, cooldown windows
- [ ] 1.9 Panic plan (what to switch on per platform) and ban plan (cross-platform) — R-50 — test: plans per platform capability
- [ ] 1.10 TTS policy (min amount, max length, strip numbers/phones/links, hold) — R-48 — test

## P2 Store, server, overlays end to end
- [ ] 2.1 Store: migrations runner, settings, overlays, alert rules, events, alert_queue — R-34 — test: migrate empty + fixture, atomic snapshot
- [ ] 2.2 Server: loopback bind, session token, Origin check, overlay tokens, static embedding — R-43 — test: 401/403 cases, wrong host, LAN off
- [ ] 2.3 Overlay WebSocket protocol (snapshot, events, alerts, ack) — R-36, R-38 — test: overlay can't send events, resync from seq
- [ ] 2.4 Engine bus: normalise → dedupe → store → rules → fan-out; staged startup gate — R-30 — test: early events buffered
- [ ] 2.5 Dashboard API v1 (overlays, alerts, test events, queue controls, settings) with `{error:{code,message,hint}}` errors — test: every route's negative cases
- [ ] 2.6 Media library with Range, size/type limits (413/415) — test

## P3 Desktop shell
- [ ] 3.1 Tauri app: single window → local dashboard, tray menu, global hotkey, single instance — R-56
- [ ] 3.2 Headless mode `ferrocast --headless`
- [ ] 3.3 OBS adapter: connect, supervisor, one-click "Add to OBS" (browser source in current scene) — test against fake OBS
- [ ] 3.4 Autostart, window state, notifications

## P4 Twitch
- [ ] 4.1 Device-code sign-in, keychain storage, proactive refresh, one re-auth banner — R-45, R-22
- [ ] 4.2 EventSub WebSocket session: welcome, keepalive watchdog, reconnect handoff, revocation — R-18, R-21 — test against fake EventSub
- [ ] 4.3 Payload mapping for chat, follow, subs, gifts, cheers, raids, redemptions, moderation, stream — fixtures
- [ ] 4.4 Helix: send chat (rate budget, expiry), ban/timeout, delete, chat settings — R-19, R-20
- [ ] 4.5 Backfill after a gap (followers, subs) marked `Backfill`

## P5 YouTube
- [ ] 5.1 Sign-in (loopback PKCE), find the active live chat
- [ ] 5.2 `streamList` gRPC client with resume token — R-24
- [ ] 5.3 Mapping (chat, Super Chat, Super Sticker, members, gifts, deletions, bans)
- [ ] 5.4 Send, delete, ban; quota meter — R-19

## P6 Relay + Kick
- [ ] 6.1 Relay binary: health, config, desktop link over WSS with per-install keys
- [ ] 6.2 Kick webhook ingress: RSA verify, timestamp window, replay table, queue when desktop offline — R-25
- [ ] 6.3 Kick OAuth broker
- [ ] 6.4 Kick adapter mapping + chat send + bans

## P7 Bot, moderation, TTS
- [ ] 7.1 Command runtime + built-ins (!commands, !uptime, !so, counters, quotes)
- [ ] 7.2 Timers (online-only, chat-activity gate, Shared Chat aware) — R-11
- [ ] 7.3 Moderation pipeline: filters → actions on the source platform; cross-platform ban list
- [ ] 7.4 Panic mode across platforms — R-50
- [ ] 7.5 TTS: system voice to WAV, approval queue, drop on delete — R-48

## P8 Tips
- [ ] 8.1 Tip page config (display name only, never the legal name) — R-13
- [ ] 8.2 Ko-fi webhook → Tip
- [ ] 8.3 Razorpay (payment.captured, refunds, disputes) → Tip / Reversal — R-49
- [ ] 8.4 Stripe (checkout.session.completed, charge.dispute.created) → Tip / Reversal

## P9 Own your setup
- [ ] 9.1 Export everything (`.ferropack`: JSON + media) — R-02
- [ ] 9.2 Import StreamElements (overlays, commands, timers, loyalty) — R-03
- [ ] 9.3 Import Streamlabs (Cloudbot commands, timers, quotes)
- [ ] 9.4 Restore snapshots from the UI

## P10 Release
- [ ] 10.1 Packaging (macOS universal DMG, Windows NSIS per-user, Linux deb/rpm/Flatpak) — R-62
- [ ] 10.2 Updater with "never while live", rollback — R-60
- [ ] 10.3 Signing (Apple notarisation, SignPath) — R-61
- [ ] 10.4 Soak test 24 h in CI — R-40
