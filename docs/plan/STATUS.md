# Status

Newest entry first. Update at the end of every working session.

## Now

- **v1 feature-complete.** Engine, adapters (Twitch, YouTube, Kick, OBS), tips through the relay, export/import, plus community features: loyalty points, chat polls, giveaways, first-time chatter marks, stream recap, Discord go-live posts, OBS auto-detect and starter commands.
- **Relay is live** at ferrocast-relay.clearcraftlabs.com (Docker on the Oracle server behind its own Cloudflare tunnel).
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

### 2026-10-18
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
