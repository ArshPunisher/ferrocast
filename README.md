# Ferrocast

**Alerts, overlays, chat and moderation for Twitch, YouTube and Kick. Runs on your computer, works in OBS, free and open source.**

Ferrocast replaces the companion tools streamers usually rent from a cloud: the alert box, chat box, event list, goals, chatbot, merged chat, cross-platform moderation and a tip page. It runs on macOS, Windows and Linux, keeps working if any server goes away, and lets you export everything you make.

> Status: in development. Not ready for streams yet.

## Why

- **You own your setup.** Overlays, sounds, commands, points and tip history live on your machine. One click exports all of it.
- **Light while you game.** The engine is Rust. The window closes to the tray; overlays are plain HTML that OBS draws.
- **One place for three platforms.** Twitch, YouTube and Kick events and chat in one feed, one alert box and one ban button.
- **Few clicks.** Sign in, pick a look, press "Add to OBS". That's the setup.

## What's in this repo

This repo holds the engine, the local server, the desktop app shell and the small relay server. The dashboard, overlays and website live in [`ferrocast-web`](https://github.com/ArshPunisher/ferrocast-web).

| Path | What it is |
|---|---|
| `crates/core` | Events, dedupe, alert queue, templates, filters, commands. Pure Rust, no I/O. |
| `crates/store` | SQLite storage with numbered migrations. |
| `crates/twitch`, `crates/youtube`, `crates/kick` | One adapter per platform. |
| `crates/obs` | OBS WebSocket v5 control. |
| `crates/engine` | Wires adapters, the bot, moderation and alerts together. |
| `crates/server` | The local HTTP + WebSocket server for overlays and the dashboard. |
| `crates/relay` | The hosted relay: Kick webhooks, tip payments, sign-in broker. |
| `apps/desktop` | The Tauri 2 desktop app (tray + window). |

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE). "Ferrocast" is a trademark of ClearCraft Labs; forks must use a different name.

Contributions are signed off with the [Developer Certificate of Origin](https://developercertificate.org/) (`git commit -s`). There is no CLA, so the licence can never be taken away from contributors.
