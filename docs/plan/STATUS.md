# Status

Newest entry first. Update at the end of every working session.

## Now

- **Engine and adapters are feature-complete for v1 on fakes.** Twitch (device code, EventSub WebSocket with handover and watchdog, Helix), YouTube (PKCE sign-in, gRPC `streamList`), Kick (relay-delivered webhooks, public API), OBS (Add to OBS), TTS with the computer's voices, tips (Razorpay/UPI, Ko-fi, Stripe through the relay), export/import `.ferropack`.
- **Relay** (`crates/relay`) is ready to deploy on the Oracle ARM server (`deploy/`).
- **Desktop shell** (`apps/desktop`, Tauri 2) and `apps/headless`.
- Every step is tested against fakes; nothing has been run against the real Twitch, YouTube or Kick yet because the platform apps aren't registered.

## Waiting on the owner

1. Register the platform apps (free): Twitch (dev.twitch.tv, public client), Google Cloud project with the YouTube Data API (OAuth desktop client), Kick (dev.kick.com, redirect and webhook URLs on the relay).
2. SSH access to the Oracle server and a DNS record for the relay (suggested `ferrocast-relay.clearcraftlabs.com`).
3. Apple Developer account for signing (later).

## Next

- StreamElements and Streamlabs importers (P9.2, P9.3): StreamElements' REST shapes aren't publicly documented well enough to build blind; build and test against the owner's own StreamElements account (JWT from the SE dashboard).
- Packaging, updater, signing (P10) once the dashboard build exists.
- Panic-mode "hold new accounts" needs account ages from each platform.
- Held messages need a release action in the dashboard.

## Log

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
