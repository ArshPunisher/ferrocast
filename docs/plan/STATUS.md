# Status

Newest entry first. Update at the end of every working session.

## Now

- **v1 feature-complete.** Engine, adapters (Twitch, YouTube, Kick, OBS), tips through the relay, export/import, plus community features: loyalty points, chat polls, giveaways, first-time chatter marks, stream recap, Discord go-live posts, OBS auto-detect and starter commands.
- **Relay is live** at ferrocast-relay.clearcraftlabs.com (Docker on the Oracle server behind its own Cloudflare tunnel).
- **Pro billing built, not on sale** (18 Oct): Razorpay Subscriptions in the relay (`crates/relay/src/billing`), Pro status in the engine (`/api/v1/pro`), `deploy/razorpay-keys.sh` for keys and plans. `PRO_SALES_OPEN=false` until Pro's features exist (ARCHITECTURE.md §10).
- **Beta channel ready** (18 Oct): version `0.1.0-beta.1`, Release workflow checks the tag against `Cargo.toml` and drafts betas as prereleases; "Beta feed" publishes `releases/download/beta/latest.json` (ARCHITECTURE.md §11). Not tagged yet.
- **Platform apps are registered** (Twitch, Google/YouTube, Kick); ids are in the release workflow's variables and secrets.
- **Desktop:** Spark icons, the overlay-collage install window, LAN access, and signed auto-updates (key in GitHub secrets, feed is `latest.json` on GitHub releases).
- **Importers:** StreamElements (JWT, GET only, never stored) and Streamlabs (exported CSV/XLSX files), with a preview, keep/replace for clashes, a backup first and a report. Points for people not seen yet wait and are paid when they show up.
- **Security:** imported packs can't choose media types or change device settings; media is served sandboxed; relay sign-ups are capped and unused installs pruned.

## Waiting on the owner

1. Making releases public (Homebrew tap, updates and downloads need it).

## Next

- Try the StreamElements importer on an account with real commands and points (the test account is empty), and a real Streamlabs Chatbot Currency export.
- Live tests against the real platforms (Twitch `!so` game lookup, Discord delivery).
- Panic-mode "hold new accounts" needs account ages from each platform.

## Log

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
