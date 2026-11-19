# Ferrocast on the Microsoft Store (written 8 Nov 2026)

## Why

Smart App Control (on by default on new Windows 11 PCs) blocked the 8 Nov installer: it stops
unsigned programs Microsoft's cloud hasn't seen yet, and every new build is a new file. The
owner's rules: nothing paid, and the publisher must read as us, never as someone else (so no
SignPath Foundation).

| Option | Free | Publisher shows | Stops the block |
|---|---|---|---|
| **Microsoft Store (MSIX)** | Yes: registration has been free for individuals since Sept 2025 (and for companies since May 2026), and Microsoft signs Store packages for free | **Our own publisher display name** | **Yes**: Microsoft's certificate, trusted from the first install |
| SignPath Foundation | Yes | "SignPath Foundation" (ruled out) | Builds up reputation over time |
| Azure Artifact Signing (Trusted Signing) | No, from $9.99 a month | Our name | Builds up reputation over time |
| Certum open-source certificate | No (about €25–70 a year plus a card reader) | Our name | Builds up reputation over time |
| Self-signed certificate | Yes | "Unknown publisher" | No: Windows treats it as unsigned |

So: **Microsoft Store**. The direct-download installer stays on the website for people who want it.
Microsoft's own page says Store apps are "re-signed by Microsoft and carry full reputation", and
that Smart App Control blocks unsigned files with no reputation
(learn.microsoft.com/windows/apps/package-and-deploy/smartscreen-reputation).

## What's built (code, done)

- `packaging/msix/AppxManifest.xml`, the package's manifest:
  - full trust;
  - camera and microphone;
  - screen and window capture with no yellow border;
  - local network for overlays and the remote dashboard;
  - a startup task for "Launch at login".
- `packaging/msix/Assets/`: the icons in every size Windows asks for, made from the app icon.
- `packaging/windows-msix.ps1` builds the package from the program, with three things inside:
  - FFmpeg: the same pinned build the app downloads today, checked by SHA-256, with its
    licence. A Store app may not download programs to run.
  - The icons.
  - The manifest.

  With `-Test` it also does this on the CI machine:
  - installs a signed test copy;
  - starts it the way the Start menu does;
  - checks it runs from its package with FFmpeg beside it;
  - checks the startup task works;
  - removes it.
- `apps/desktop/src/store.rs`: the app knows when it's the Store copy.
  - **Updates:** its own updater stays off, because the Store updates it. Settings › Updates
    says so and points to the Store.
  - **Launch at login:** uses the package's startup task. The usual registry entry is private
    to a package, so Windows would never read it.
  - **Started at sign-in:** detected, so it starts in the tray, as `--minimized` does now.
- **CI:**
  - `.github/workflows/windows-store.yml` builds and tests the package whenever the packaging
    changes.
  - `release.yml` builds it for every release as the `store-msix` artifact, kept out of the
    GitHub release because it's for the Store.
- **Website:** `MS_STORE_URL` in `ferrocast-web/apps/site/src/data/company.ts`. Once it's set,
  the Windows card on the download page offers "Get it from Microsoft Store" first, with the
  installer as the second choice.

## What you do (free, about 30 minutes plus Microsoft's wait)

1. **Make the developer account**:
   - go to https://storedeveloper.microsoft.com and choose **Individual**;
   - sign in with your personal Microsoft account;
   - verify your identity (an ID and a selfie). No fee and no card.
2. **Publisher display name: `ClearCraft Labs`.** This is the name Windows and the Store show
   as the publisher. If it's taken, pick another; it's yours either way.
3. **Reserve the name.** In Partner Center, go to Apps and games › New product › **MSIX or
   PWA app** and reserve **Ferrocast**.
4. **Give me the identity.** It's on the Product management › **Product identity** page:
   - `Package/Identity/Name`, for example `12345ClearCraftLabs.Ferrocast`;
   - `Package/Identity/Publisher`, for example `CN=1A2B3C4D-…`;
   - `Package/Properties/PublisherDisplayName`.

   Paste the three lines to me. I'll set them as repository variables (`MSIX_IDENTITY_NAME`,
   `MSIX_PUBLISHER`, `MSIX_PUBLISHER_DISPLAY_NAME`). The package must carry exactly these or
   the Store refuses it.
5. **The package.** I run the "Windows Store package" workflow, or the next release builds
   it. You download the `.msix` from the run's artifacts.
6. **The submission.** In Partner Center, start a submission and fill in each section:
   - **Pricing:** Free, all markets.
   - **Properties:** category Photo & video, privacy policy
     `https://ferrocast.clearcraftlabs.com/privacy/`.
   - **Age ratings:** the questionnaire. Ferrocast has user chat from Twitch, YouTube and Kick,
     so answer yes to "users can interact".
   - **Packages:** upload the `.msix`.
   - **Store listing:** the text below, plus screenshots. I can make the screenshots
     (1920×1080) from the app.
   - **Submission options › restricted capabilities:** paste the runFullTrust reason below.
7. **Submit.** Certification usually takes 1–3 working days.
8. **When it's live:** send me the Store link. I'll set `MS_STORE_URL`, so the download page
   offers it.

Store releases are stable only. A beta packed as 3.0.0.0 would block the real 3.0.0. The
Store copy updates through the Store, and the installer copy keeps updating itself as now.

## Text to paste

**runFullTrust reason:**
> Ferrocast is a desktop streaming studio (Rust + WebView2). It captures screens, windows,
> cameras and microphones, encodes video with FFmpeg (bundled in the package, started as a
> child process), serves stream overlays to OBS and browsers on a local port, and connects to
> Twitch, YouTube and Kick. These need a full-trust desktop process; it doesn't run in an
> app container.

**Short description:**
> A free stream studio for Twitch, YouTube and Kick: alerts, overlays, chat and recording, on
> your own PC.

**Description:**
> Ferrocast runs your whole stream on your own computer. Build scenes from your camera, screen,
> windows and media, then go live to Twitch, YouTube and Kick at once, or record. Alerts and
> overlays work in OBS or in Ferrocast's own studio, and chat from every platform comes into
> one list with a chatbot and moderation that covers them all.
>
> It's free and open source (GPL-3.0). It needs no account, and your data stays on your PC.
> Ferrocast uses your graphics card's encoder (NVIDIA, AMD or Intel) when it has one, and
> never updates while you're live.

**Features (one per line in Partner Center):**
- Studio with scenes, camera, screen, window and media sources
- Stream to Twitch, YouTube and Kick at the same time, or record
- Alerts and overlays for OBS or the built-in studio
- One chat for every platform, with a chatbot and moderation
- Uses your graphics card's encoder when it has one
- Free and open source, no account needed

**Search terms (7 at most):** streaming, stream overlay, alerts, twitch, youtube live, kick,
recording.

## How to check a build without the Store

On CI, `windows-store.yml` installs and starts the package on GitHub's Windows machine. On a
PC that isn't yours, run `packaging/windows-msix.ps1 -Exe <ferrocast.exe> -Test` from an
administrator PowerShell with the Windows SDK installed. Never run it on your own PC: it
installs, then removes, a test copy.
