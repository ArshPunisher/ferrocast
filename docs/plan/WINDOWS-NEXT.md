# Windows: plan for the next session (written 4 Nov 2026, status to 8 Nov)

## Status (7 Nov 2026, worked on the owner's Windows 11 PC)

Steps 1-4 are done and pushed on `own-engine-only` (both repos). Each was reproduced on the
PC first, then fixed and checked there with a release build.

| Step | Cause found | Fix | Checked |
|---|---|---|---|
| 1 Chrome window crash | Heap corruption in `win/app_sound.rs`: the `PROPVARIANT` VT_BLOB pointed at a stack struct and its `Drop` (PropVariantClear) freed it with CoTaskMemFree. Found from a ProcDump crash dump (`!heap`). | `ManuallyDrop` (d7056ff) | Chrome window + sound plays, app stays up, sound peaks at -26 dB |
| 2 Lag | Not the CPU path: wgpu took the laptop's Intel iGPU (Windows' first adapter); drawing + readback took 22-34 ms a 1080p60 frame | Ask for `HighPerformance` (4a33a72) | 1080p60 recording 60.2 fps, 0 late frames, 4.6 ms a frame, ~10.5% CPU (was 24-38 fps) |
| 3 Preview empty | The page only placed the picture when the whole stage fitted; the narrow layout (short window) never does | Place it whenever any of it shows, cut out under the top bar and tab bar (ferrocast-web 9986ddc) | 1000×700 at 125%: picture shows, follows scroll |
| 3 Resize error | "Failed to wait for GPU to come idle before reconfiguring the Surface": the frame loop submitted while the main thread configured the preview surface | One lock for submits and surface configure (3e4f211) | 100 resizes (60 while recording): none (before: every couple) |
| 4 Installer | Icon was already in the exe (checked on a renamed copy): the 4 Nov disc icon was Explorer's cache | Sidebar + header images from the brand script (ferrocast-web 236904e, bd148c3) | Built locally with `cargo tauri build --bundles nsis`, pages screenshotted |

Not done, on purpose: custom NSIS wording and fewer pages (needs a copy of Tauri's 1000-line
template to maintain; `apps/setup` replaces NSIS once builds are signed). The zero-copy
capture idea in Step 2 wasn't needed for 60 fps; keep it for later.

Step 0 A is in: `tools/windows-smoke.ps1` (display, an Edge window playing a tone with its
sound, desktop sound, 20 s recording; checks the app lived, picture and sound in the file,
fps, CPU; ProcDump catches a crash) and `.github/workflows/windows-smoke.yml` runs it on
GitHub's Windows runner. Passed 3 runs on the owner's PC (29.1-29.9 of 30 fps, 5-8% CPU,
7-48 frames dropped, the tone in the file). Its first run on the runner is still to be seen
(WARP, no real GPU: the frame-rate check may need a lower bar there).

## Status (8 Nov 2026, same PC)

| What | Cause found | Fix | Checked |
|---|---|---|---|
| Crash reports | The app wrote nothing when it crashed | `apps/desktop/src/crash.rs`: an unhandled-exception filter wakes a thread made at start that writes a minidump to `%APPDATA%\Ferrocast\crashes`; the next start logs it, keeps 5 (9a65e14). `FERROCAST_TEST_CRASH=1` crashes a debug-assertions build after 5 s; `windows-smoke.ps1 -Crash` tests it (e8dfba8) | Dump written (0.3 MB), cdb shows the access violation |
| Frames dropped at the start | The pipeline was built while holding the engine lock, and FFmpeg's input blocked the frame loop | Pipeline built outside the lock; FFmpeg input on its own thread with a 96 MiB queue (6e69da9); stats count from the output's start (2d347c7) | 30 fps: 0 dropped (was 7-48). 60 fps varies: the PC is on the "Silent" power plan |
| First-run setup | Opened on picking an alert look | Asks streaming or recording, then picture size (4K/1440p/1080p/720p, "Your screen" badge) and fps, and sets size, fps, hardware encoder and bitrate (ferrocast-web c922966) | E2E tests for both paths |
| Recording opened two empty terminals; file 0 bytes | An installed (GUI) app gives every console child a window; closing one killed FFmpeg. Debug-assertions test builds are console apps, so it never showed | `ferrocore::ffenc::hidden` (CREATE_NO_WINDOW) on every FFmpeg and PowerShell start: recording, encoder probe, remux, network media, vertical clips, speech, the vertical cutter | Owner installed the 8 Nov build: "it works now" |
| "Mac hardware" on Windows | Wording written for macOS | `lib/os.ts`: "Graphics card", Ctrl shortcuts, Videos folder, Ctrl/Alt/Win in keys (ferrocast-web 8fca28e) | Unit tests, owner |
| Path-only fields | Recordings folder and song file needed a typed path | Browse buttons with the computer's picker (8fca28e, 435dc9f) | Owner |

**Smart App Control.** It blocked the 8 Nov installer the first time (CodeIntegrity log,
event 3077, "did not meet the Enterprise signing level"). An hour later the very same file
opened. For an unsigned file it asks Microsoft's cloud, and a file it has never seen can be
blocked at first; nothing in the code caused it. A copy of the file is checked again (the old
verdict is cached on the file). Every new build is a new file, so users can hit this on any
release until Windows builds are signed. The owner said no to signing (SignPath) for now.
Check a build: start the setup, close it, then look for 3077 events in
`Microsoft-Windows-CodeIntegrity/Operational`.

**CI is red on this branch** (since 2 Nov, before this work): CI fails at rustfmt and the
Tests job on all three systems, and the Windows smoke workflow failed both runs. The logs
need a signed-in `gh`, which this PC doesn't have yet. Fix that first next time.

Still to do: CI above; a medium recording quality for "Recording only" (files were ~68 Mbps
at 'high', the owner hasn't decided); the uninstaller is unsigned like the setup. libobs
stays out (not needed).

**Test the installer without touching the owner's Ferrocast:** never install it here. Build
with `cargo tauri build --bundles nsis --features deepfilter` in `apps/desktop` (plus
`FERROCAST_WEB_DIST`); local builds have no Twitch/YouTube ids, so sign-in doesn't work.

**What this PC needed beyond the list below:** CMake (opusic-sys) and LLVM with
`LIBCLANG_PATH="C:\Program Files\LLVM\bin"` (whisper-rs bindgen); pnpm 11.22.0 to match the
lockfile (`npx pnpm@11.22.0`). A release build that still writes the token file:
`cargo build --release -p ferrocast-desktop --config 'profile.release.package.ferrocast-desktop.debug-assertions=true'`.
Recordings go to the real `Videos\Ferrocast` even with `FERROCAST_HOME` set: delete test ones.
Crash dumps: Sysinternals ProcDump (`procdump64 -ma -e -x <dir> <exe>`), read with WinDbg's cdb.

## Start here (Claude running on the owner's Windows PC)

You are on the owner's own Windows 11 PC, which is the machine the bugs below happen on. That
solves this plan's biggest problem: until now nobody could **run** the Windows engine, it was
only compiled on GitHub. Run everything here and look at it before calling anything fixed.

**Repo and branch.** `ferrocast` repo, branch `own-engine-only` (Ferrocast's own engine,
Ferrocore, in `crates/ferrocore`; no OBS/libobs: the owner chose that on 1 Nov 2026). The web
dashboard is a separate repo, `ferrocast-web`, same branch name `own-engine-only`: build it
with `pnpm install --frozen-lockfile && pnpm build` and point `FERROCAST_WEB_DIST` at its
`dist` folder when building the app (see `.github/workflows/release.yml`, "Build the app").

**Tools this PC needs:** Rust (rustup, MSVC toolchain + Visual Studio Build Tools "Desktop
development with C++"), Node 22 + pnpm, `cargo install tauri-cli --version "^2" --locked`,
WebView2 (already in Windows 11), Git.

**Running a dev build and driving it.**
- `cargo build -p ferrocast-desktop`, then start `target\debug\ferrocast.exe` with
  `FERROCAST_HOME` set to an empty test folder (keeps the owner's real data out of tests) and
  `FERROCAST_DEBUG_TOKEN_FILE=<file>`: a dev build writes "<base url> <token>" there.
- Drive the Studio through the local API with that token:
  `<base>/api/v1/studio/engine/...` with `Authorization: Bearer <token>`.
- The app is single-instance: close the installed Ferrocast first.
- Logs: `%APPDATA%\Ferrocast\logs` (installed app) or under `FERROCAST_HOME`.
- Engine CPU test: `cargo run --release -p ferrocore --example cpu_soak -- <secs> <fps>`.

**Owner's rules (from their own setup, follow them):**
- Commit only when the owner says "commit"/"commit and push". Small focused commits,
  conventional messages with a scope (`fix(ferrocore): ...`), body says why. The commit-msg
  hook (`tools/check-commit-msg.sh`) rejects subjects over 72 characters after the colon.
- Author is the owner only: never add `Co-Authored-By`, "Generated with Claude Code" or any
  AI attribution. Never commit secrets or `.env`, never force-push, push only this branch.
- Before committing, run CodeRabbit (`coderabbit review --agent --uncommitted`, free plan,
  3 an hour, never `--use-credits`); if it can't run, do your own review at the same bar.
- Don't spawn subagents or run several agents without asking the owner first.
- Write to the owner in plain, short English; they say "do all" often, but ask before big
  choices that are theirs (like bringing libobs back).

**What's already in this branch for Windows (all untested on a real PC):** WGC capture
(`win/wgc.rs`, now BGRA surfaces), per-app sound through WASAPI process loopback
(`win/app_sound.rs`, the first crash suspect), Direct3D 12 first in `compose.rs`, preview as a
child HWND (`crates/studio/src/ffi/win.rs`), our own setup program `apps/setup` (switched off
in the release workflow until Windows code signing exists; builds ship NSIS).

---

Owner's test of the 4 Nov build (run 41691853753, NSIS, commits up to c690c60) on a real
Windows 11 PC:

1. The exe still shows the default NSIS "disc" icon, and the installer is the plain default
   NSIS UI (expected: no branding was done yet; the icon fix `installerIcon` didn't show).
2. Adding a Chrome **window** source closes the app with no message (silent crash).
3. The app lags badly on Windows.

Owner: "too buggy on Windows, use a different, better approach. Plan only, do it next session."

## Root problem

None of the Windows engine code has ever been **run** by us. Everything was only compiled on
CI. Every fix so far was a guess from reading code. So step 0 is getting a Windows machine we
can drive and watch, and only then fixing.

## Step 0: a Windows machine we can run and see (do first)

Now: this PC (see "Start here"). Reproduce each bug here first, with the log and a crash
dump. Later, so it never breaks unseen again, also add:

- **A. GitHub's Windows runner as a test bench.** It has a desktop session, a GPU-less WARP
  device and can run WGC/WASAPI (verify). Add a CI job `windows-smoke` that:
  starts the built app with `FERROCAST_DEBUG_TOKEN_FILE`, drives the local API
  (`/api/v1/studio/engine/...`) to add a display source, a window source (start Notepad or
  Edge with a page first), a microphone and Desktop Audio; records 20 s; checks the app is
  still alive, CPU (Get-Process), the MP4 (ffprobe: video + sound present), and uploads
  screenshots, the log and any crash dump as artifacts. Free; repeatable on every push.
- **B. A Windows 11 VM on this Mac** (UTM or Parallels, Windows 11 ARM running the x64 build
  under emulation, or an aarch64-pc-windows-msvc build). Lets me watch the preview by eye.
- **C. Remote access to the owner's PC** for a final check only.

Also ship **crash reports** in Windows builds: a minidump on any native crash (crate
`minidumper` + `crash-handler`, written to `%APPDATA%\Ferrocast\crashes`) and a panic hook that
logs the backtrace. A silent close must leave a file we can read.

## Step 1: the Chrome window crash

Suspects, in order (check with the crash dump / smoke test):

1. New per-app sound (`crates/ferrocore/src/win/app_sound.rs`, commit c5c7ce9): COM
   `ActivateAudioInterfaceAsync` + our `#[implement]` handler, `PROPVARIANT` blob, the
   capture thread. A native fault there kills the process (catch_unwind can't catch it).
   Quick test: start the window source with sound off (`FERROCAST_NO_APP_SOUND=1` switch).
2. WGC window capture of Chrome (`win/wgc.rs`): Chrome's window is GPU-composited; the pool's
   `Recreate` on resize, `ContentSize` vs texture size, the new BGRA `Surface` path
   (commit 31e2640): a row read past the mapped texture would crash.
3. Window id truncation: `hwnd.0 as usize as u32` (screen.rs) — a 64-bit HWND cut to 32 bits
   is usually fine but not guaranteed; keep the full value (u64) end to end.

Fix the cause, keep a test in the smoke job that adds a Chrome/Edge window and plays sound.

## Step 2: the lag — move the Windows path to the GPU end to end (OBS's way)

Today each Windows frame goes GPU → CPU (staging map) → CPU copy → GPU (wgpu upload), on two
separate GPU devices (WGC on D3D11, wgpu on D3D12). OBS never brings frames to the CPU.

Better approach:

- **Zero-copy capture:** make WGC's D3D11 device and wgpu's D3D12 device the same adapter,
  copy each frame into a **shared texture** (D3D11 `CreateTexture2D` with
  `D3D11_RESOURCE_MISC_SHARED_NTHANDLE`, opened in D3D12 with `OpenSharedHandle`) and wrap it
  in wgpu with `wgpu::hal::dx12` `texture_from_raw` / `Device::create_texture_from_hal`.
  The compositor samples it directly: no map, no CPU copy, no upload. Keep the current CPU
  path as the fallback (WARP, odd adapters).
- **Encoders:** check which encoder runs on Windows; use the GPU one (NVENC / AMF / QSV via
  FFmpeg, or Media Foundation) by default, x264 only as fallback. Feed it NV12 straight from
  the GPU where FFmpeg allows (d3d11va frames), otherwise one readback per output frame.
- **Measure:** the smoke job prints CPU % and dropped frames for display/window at 1080p30 and
  1080p60; target close to 2.3.0 (libobs) on the same machine.

## Step 3: the preview window (canvas empty / lag on resize)

The preview is a child HWND over WebView2 with a wgpu swap chain (`studio/src/ffi/win.rs`,
`ferrocore/src/preview.rs`). Options:

- **Keep the child window, do it right:** follow the parent's size in the same step (subclass
  the main window, resize the child on `WM_SIZE`/`WM_WINDOWPOSCHANGED` like the Mac's
  `follow_window`), present with a flip-model swap chain sized to the child, `WS_CLIPCHILDREN`
  on the parent so WebView2 doesn't paint over it.
- **Or DirectComposition:** wgpu can make a surface from a DComp visual
  (`SurfaceTargetUnsafe::CompositionVisual`). Put the preview visual in the same window's
  DComp tree above the webview: no extra HWND, no z-order fights, moves with the window.
  Try this if the child window keeps misbehaving.

## Step 4: the installer

- **Icon:** confirm on CI whether `bundle.windows.nsis.installerIcon` is really in the exe
  (PowerShell `[System.Drawing.Icon]::ExtractAssociatedIcon`, save as PNG artifact). The
  owner's Explorer may also show a cached icon for the same file name: test with a renamed
  copy. If Tauri ignores the setting, use a custom NSIS template
  (`bundle.windows.nsis.template`) with `!define MUI_ICON`.
- **Look:** branded NSIS now (no extra plugins, so Smart App Control still trusts it):
  `sidebarImage` 164×314 (Welcome/Finish: logo, dark, step list), `headerImage` 150×57,
  our wording, fewer pages; the Finish "Run Ferrocast" launch without the wait.
- **Our own setup (apps/setup, Style C UI)** comes back only with **Windows code signing**
  (Azure Trusted Signing, ~$10/month, owner's Azure account + identity check). It's switched
  off in `.github/workflows/release.yml` (`if: false && ...`) since 3e5e77c.

## Option to raise with the owner (decision is theirs)

On 1 Nov the owner chose "own engine only, no OBS/libobs". 2.3.0 with libobs was smooth on
Windows. If Steps 0-3 can't reach that quickly, one choice is libobs on Windows only while
Ferrocore's Windows path matures. Ask; don't do it unasked.

## Order

0 (test bench + crash dumps) → 1 (crash) → 2 (GPU path, CPU) → 3 (preview) → 4 (installer).
Every step: run on the Windows bench, numbers before/after, then a build for the owner.
