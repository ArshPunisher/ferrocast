# Windows hand-off (written 10 Nov 2026, on the Mac)

> **Run on the Windows PC 10-11 Nov:** results are in §8. The story (what got in the way, each
> fix and its cause, what's left) is in `docs/plan/WINDOWS-SESSION-10NOV.md`.

For Claude running on the owner's Windows 11 PC. Read this first, all of it, then
`docs/plan/WINDOWS-NEXT.md` (the 7-8 Nov Windows work and how this PC was set up). Everything
below was built and tested on a Mac and only **compiled** and smoke-tested for Windows on
GitHub's GPU-less runner. Your job is to **run it here, see it, measure it, fix what's wrong**.
Never call a Windows thing done without running it on this PC.

---

## 0. From the Mac, 11 Nov: do these first on this PC

Everything below was built or fixed on the Mac and needs this PC. Same rules as always (§3):
a test home (`FERROCAST_HOME`), never the owner's data; commit only when the owner says.

1. **Looping video sound** (fixed on the Mac, `ffsrc.rs` `Out::FileSound`): play a looping
   clip with sound in a scene for a few minutes; the sound must stay and stay in sync. Re-run
   the 60 s sync test (`tools/`), numbers in §8.
2. **The capture switch** (Screen ↔ Window ↔ App, `engine.rs` `hands_over`): no black flash
   while switching; Window with nothing picked keeps the old picture until a pick.
3. **Studio Mode** (OBS's Duplicate Scene, `stage.rs` `send_copy_on_air`): edit a source in the
   preview; what's on air must not move until Transition. Then the new bar under the two
   pictures: nothing of either picture may draw over the bar or the labels (the child HWND
   reaches 10 px past its box; the page keeps 12 px clear). Try 125% and 150% scaling.
4. **Pixel guides** while dragging a source in the preview: red lines to each canvas edge with
   the px distance; they go when it's let go. Check the label size at 150% scaling.
5. **Source icons F2** (two-tone by kind, filled eye/lock) and the compact drawer: look only.
6. **Camera size and frame rate** (§4.x): pick a size and rate the camera can't do; the picture
   must still come.
7. **The local installer test** the owner still owes from 10 Nov: a recording, a video file in
   a scene, Instant replay, the Stats encoder line.
8. **Sign-in on Windows** ("Finish in browser") with a CI-built installer and the owner's
   account.
9. **Not tried yet here:** two monitors (different scaling), 150% scaling, unplugging a USB mic
   mid-recording (the watchdog must bring it back).
10. **VST plugins on Windows:** off until their helper has a sandbox. Build it here:
    `crates/vsthost/src/client.rs` starts `ferrocast-vst`; on Windows run it as a low-integrity
    (or AppContainer) process in a job object (kill on close, one process, memory cap) with no
    network capability, and only the plugin's own folder readable beyond system DLLs. Then make
    `ferrocast_vsthost::available()` say yes on Windows, and run `cargo test -p
    ferrocast-vsthost` (the test plugin `crates/vst-testgain` builds as a `.vst3` here too).
    Check a free VST3 (e.g. a gain or EQ) and that a crash or hang in it leaves the sound dry.
11. **Twitch Enhanced Broadcasting** (if built by then; see `docs/plan/OBS-COMPETE.md`): a short
    test stream on Twitch with NVENC.
12. **Playlist speed-up** (`source/series.rs`): a playlist's videos now go to the card as NV12,
    as a single video file does. Play a playlist of two 1080p clips and compare its CPU with
    one clip on its own (they should be close now); check the clips change cleanly.
13. **Guest cameras over TURN:** once the relay has its TURN key, a guest on a phone's mobile
    data (strict network) while the PC is on another strict network must still connect.
14. **CI:** look at the newest `CI` and `Windows smoke` runs on GitHub for these commits.

## 1. Where things stand

| | |
|---|---|
| Branch | `own-engine-only` in both repos (`ferrocast`, `ferrocast-web`). Ferrocast's own engine (Ferrocore), no OBS/libobs (owner's choice, 1 Nov). |
| Released | **3.0.6** (published). **3.1.0** tagged 10 Nov and released from the Mac session (if `gh release view v3.1.0` still says draft, see §9). |
| CI | Green on the 3.1.0 commit: `CI` (fmt, clippy, tests on 3 systems) and `Windows smoke`. |
| Windows smoke on GitHub's runner (3.1.0) | All checks pass. No GPU there (WARP on 4 cores): 23.1 of 30 fps, 72% CPU, 302 of 349 frames late. Those numbers mean nothing for a real PC; get them **here**. |
| Last time on this PC | 7-8 Nov: crash fixed, iGPU → HighPerformance, preview, installer images, crash dumps, hidden FFmpeg consoles. 1080p60 recording 60.2 fps, 4.6 ms a frame, ~10.5% CPU. |

### What's new since the last Windows session (never run on Windows)

3.0.2 to 3.1.0 were all built on the Mac. On Windows, check each (§4):

- **3.0.2 to 3.0.6:** new filters and crop UI, capture redesign, Settings layout A, blue
  picks, shorter setup, "Finish in browser", a quit-crash fix (Studio closes first), Pro
  without restart, Add-a-source off the main thread (`options_cached`, `open_soon`).
- **3.1.0 sources:** Instant replay, Web text, Keys on screen, Countdown, Sound bars, Copy of
  a source.
- **3.1.0 filters:** Freeze, Effects gallery (8 shader presets).
- **3.1.0 features:** Top layer over every scene, "When X, do Y" automations.
- **3.1.0 plugins:** effect, logic (WebAssembly in `wasmi`) and panel plugins
  (Settings › Plugins).
- **Mac only, needs a Windows version:** the frame thread's priority and App Nap off
  (`on_time()` in `crates/ferrocore/src/engine.rs` does nothing on Windows; see §5).

---

## 2. Setting up and running (this PC already has most of it)

Tools (from `WINDOWS-NEXT.md`): Rust MSVC + VS Build Tools "Desktop development with C++",
Node 22 + pnpm **11.22.0** (`npx pnpm@11.22.0`), `cargo install tauri-cli --version "^2"
--locked`, CMake, LLVM with `LIBCLANG_PATH="C:\Program Files\LLVM\bin"`, Git, `gh` (sign it in:
`gh auth login`, the owner does logins), Sysinternals ProcDump, WinDbg (cdb).

```powershell
# both repos side by side, e.g. C:\src\ferrocast and C:\src\ferrocast-web
git -C C:\src\ferrocast pull ; git -C C:\src\ferrocast-web pull

# the dashboard (the app embeds it)
cd C:\src\ferrocast-web ; npx pnpm@11.22.0 install --frozen-lockfile ; npx pnpm@11.22.0 build
$env:FERROCAST_WEB_DIST = 'C:\src\ferrocast-web\dist'

# a dev build (writes its token file), or a release build that still does:
cd C:\src\ferrocast
cargo build -p ferrocast-desktop
cargo build --release -p ferrocast-desktop --config 'profile.release.package.ferrocast-desktop.debug-assertions=true'
```

Run it against a **test home** (never the owner's data) and drive it over the local API.
Close the installed Ferrocast first: the app is single-instance.

```powershell
$T = 'C:\fc-test' ; New-Item -ItemType Directory -Force "$T\home","$T\rec" | Out-Null
$env:FERROCAST_HOME = "$T\home" ; $env:FERROCAST_DEBUG_TOKEN_FILE = "$T\token.txt"
Start-Process .\target\release\ferrocast.exe
Start-Sleep 8
$base, $tok = (Get-Content "$T\token.txt").Split(' ')
function api($m, $p, $body) {
  $a = @{ Method = $m; Uri = "$base/api/v1$p"; Headers = @{ Authorization = "Bearer $tok" } }
  if ($body) { $a.Body = ($body | ConvertTo-Json -Depth 10 -Compress); $a.ContentType = 'application/json' }
  Invoke-RestMethod @a
}
api POST /studio/engine/start
$s = api GET /studio/engine/settings ; $s.record_folder = "$T\rec" ; $s.PSObject.Properties.Remove('record_folder_default')
api PUT /studio/engine/settings $s
```

Notes:
- **Smart App Control must be off to build** (owner turned it off 10 Nov). When on, it blocks
  the newly built build scripts and proc-macro DLLs, and Rust reports "can't find crate"
  (E0463) for a different crate each run. The CodeIntegrity log (event 3077) shows the blocks.
- Run the smoke script from Windows PowerShell 5.1 too, not only `pwsh`: a `$crash`
  variable was the `-Crash` switch there (names ignore case) and broke the end of the run.
- Recordings may still go to `Videos\Ferrocast` even with `FERROCAST_HOME`: set
  `record_folder` as above, and delete test files.
- Logs: `$T\home\logs\ferrocast.log`.
- Crash dumps: `$T\home\crashes`, or run under `procdump64 -ma -e -x <dir> <exe>`.
- **Privacy:** the test app must not capture the owner's screen, camera or mic unless they say
  so. Use the smoke script's Edge window with a tone, a Colour source, or a video file.
- The whole smoke test, as on CI: `tools\windows-smoke.ps1 -Exe target\release\ferrocast.exe
  -Out C:\fc-smoke -Seconds 20 -Fps 30` (`-Fps 60`, `-Crash` for the crash report check).
  On this PC it must reach `-MinShare 0.9` (the default).

API reference: `docs/API.md` (Studio engine section). Useful calls:

```powershell
api GET  /studio/engine/scenes
api POST /studio/engine/scenes @{ name = 'Tests' }
api POST /studio/engine/scenes/Tests/items @{ kind = 'color_source_v3'; name = 'Red'; settings = @{ color = 4278190335; width = 400; height = 300 } }
api POST /studio/engine/scenes/Tests/current
api POST /studio/engine/sources/Tests/screenshot      # → { file } PNG of the scene
api GET  /studio/engine/stats                         # engine fps, frame_ms, lagged, cpu, record
api POST /studio/engine/record @{ on = $true }        # … @{ on = $false } → the file
api POST /test @{ kind = 'follow' }                   # a test event through the real pipeline
```

---

## 3. Owner's rules (follow them exactly)

- **Commits:** commit only when the owner says "commit" or "commit and push". Small focused
  commits, conventional subjects with a scope (`fix(ferrocore): …`, at most 72 characters after
  the colon; the hook `tools/check-commit-msg.sh` checks), body says why.
- **No AI credit:** author is the owner only. Never `Co-Authored-By`, "Generated with Claude
  Code" or any AI attribution.
- **Safety:** never commit secrets or `.env`, never force-push, push only `own-engine-only`.
- **Review:** before committing, `coderabbit review --agent --uncommitted` (free plan, 3 an hour,
  never `--use-credits`). It skips new untracked files: `git add -N` them first. If it can't
  run, do your own review at the same bar and say so.
- **Format and lint:** `cargo fmt --all` and `cargo clippy --all-targets` (CI runs both: the
  last red CI was only rustfmt).
- **No agents:** no subagents or parallel agents without asking.
- **Talking to the owner:** plain, short English. Put minor explanations behind (i) tips in
  the UI rather than long text.
- **Releases:** don't publish one unless the owner says "release" (see §9 for how).
- **libobs:** stays out. Raise it only as a question if the own engine can't get there.
- **Measure first:** before/after numbers for every perf change, on this PC.

---

## 4. Check every feature built since 8 Nov, on this PC

For each one, do it, look at it (screenshot through the API or the window), read the log,
and write the result in the table in §8. On a problem: reproduce, find the cause (log, crash
dump, a test), fix, test again here.

### 4.1 Studio basics (the 8 Nov night list, never checked here)
1. **Add a source:** open it with the Studio running.
   - Expected: no grey box over the dialog.
   - Camera, Screen and Capture card lists show at once.
   - Unplug the camera: "No camera found".
   - Tiles/List view is kept after closing the app.
2. **Adding sources doesn't freeze the window:** add a camera, a window, a video file
   (`open_soon` opens them off the main thread).
3. **Filters and crop UI (3.0.2):** add and remove filters on a source; crop by dragging.
4. **Quitting with the Studio running:** no crash dump in `crashes`, and no "Ferrocast
   crashed last time" on the next start (3.0.4's fix).

### 4.2 Built-in sources and filters (3.1.0)
All appear in Add a source / Add a filter in the dashboard (Build, Media, Effects…). Kinds:

| Feature | Kind | Check here | Expected |
|---|---|---|---|
| Instant replay | `ferrocast_replay` | Scene "Replay" with it (fit). Replay buffer on (`POST /studio/engine/replay {on:true}`); after ~10 s `POST /studio/engine/replay/save` | Within ~1 s the Replay scene goes on air, the clip plays from the start, then the scene before comes back when it ends (`GET /scenes` `current`). Uses `ffsrc` Media `Ended` on Windows: verify it reports `ended`. Two saves in a row: the clock restarts. |
| Web text | `ferrocast_web_text` | `python -m http.server` with a JSON file; settings `url`, `path` `data.viewers`, `words` `Viewers: {}` | Screenshot shows the value; change the file and it updates within `every` s. A bad URL: log warns once, nothing drawn. Uses reqwest blocking (TLS via schannel on Windows: try an `https://` API too) |
| Keys on screen | `ferrocast_inputs` | Layout `wasd`; press W in Notepad **with the owner's OK** | W lights while held (GetAsyncKeyState). With focus in a classic password box (e.g. Run as other user dialog), nothing lights. Note: a browser's password field isn't detected on Windows (no system-wide Secure Input); tell the owner, consider adding a "pause while typing" rule |
| Countdown | `ferrocast_countdown` | `minutes 1` | "Starting in 0:59" ticking (shared text renderer, DirectWrite-free cosmic-text fonts: check the font looks right) |
| Sound bars | `ferrocast_visualizer` | With the Edge tone playing | The tone's bar lit (Goertzel; 440 Hz in a low-middle bar) |
| Copy of a source | `ferrocast_clone` | Copy of the Edge window | Same picture twice; not in the mixer |
| Freeze filter | `ferrocast_freeze` | On a moving source; switch on/off | Picture holds while on (compare two screenshots: identical), moves when off |
| Effects | `ferrocast_shader` + `preset` | Each of crt, pixelate, vignette, chromatic, glitch, wave, old_film, night_vision | Each draws (D3D12 WGSL path; a shader that fails to compile on D3D12/FXC/DXC but not Metal is the risk here: watch the log) |
| Top layer | `PUT /studio/engine/top-layer {scene}` | A "Logo" scene over Main, switch scenes | Logo over every scene, transitions included; not over itself |

### 4.3 Automations and plugins (engine, should be the same on every system; still check)
- **Automations** (Settings › Automations): rule "follow → scene Tests, wait 2, scene Main";
  `api POST /test @{kind='follow'}`. Expected: the scene goes to Tests, then back. A rule with
  no steps gets a 422 that says why.
- **Logic plugin:** build a tiny one (see `docs/PLUGINS.md`, or the `.wat` in
  `crates/engine/src/plugins.rs` tests compiled with the `wat` crate). Install it in Settings ›
  Plugins and fire `api POST /test @{kind='raid'}`. Expected: its allowed step runs; one it
  didn't list is refused, with a log line. `wasmi` uses `auto-dispatch`; without it a debug
  build overflows the stack.
- **Panel plugin:** the panel opens in the dashboard's sandboxed frame (WebView2), and its
  buttons work. Check WebView2 honours `sandbox="allow-scripts"` + the CSP in `srcdoc` (it's
  Chromium, so it should).

### 4.4 Settings, setup, sign-in (dashboard)
- **Settings layout A:** at 100% and 125% scaling, and at a short window.
- **Ferrocast blue:** picked items show it.
- **Setup:** 3 steps, ending at Connect.
- **Sign-in:** "Finish in browser" opens the default browser again.
- **Windows wording:** Ctrl shortcuts, "Graphics card" (`lib/os.ts`).

---

### 4.x Added on the Mac on 10 Nov evening (check them here)

- **Camera Size and Frame rate** (the source drawer's Settings): on Windows they become
  ffmpeg's dshow `-video_size` / `-framerate` (`engine::camera_args`). A size the camera can't
  do should fall back to its own (`win/camera.rs` retries without them), but ffmpeg starts
  before it fails, so check: pick 4K and 60 on a camera that can't, and the picture must still
  come. If it doesn't, read the camera's modes first (`ffmpeg -f dshow -list_options true -i
  video=…`) and pick the nearest, as the Mac does (`apple/camera.rs` `pick_format`).
- **Source status** (`GET …/sources/{name}/status`): a Web text's last read and a Browser
  source's page check; try a dead address and a real one.
- **Screen capture "Leave out"** is Mac only (ScreenCaptureKit); Windows' capture has no app
  exclusion yet.

## 5. Engine and frames: what to measure and what to build

How the engine runs a frame (all systems):

- **Frame loop:** `run()` in `crates/ferrocore/src/engine.rs` is one thread that wakes every
  frame (`std::thread::sleep` up to 5 ms at a time). If more than a frame late it skips
  ahead, as libobs does. Then `frame()` composites on the GPU (`compose.rs`, wgpu,
  **Direct3D 12** first on Windows), feeds outputs, mixes sound (`mix_until`), and
  `gpu.submit` + `poll`.
- **Late frames:** counted as `lagged` (Stats "Missed").
- **Outputs:** FFmpeg-encoded on Windows (`ffenc.rs`, its own thread with a 96 MiB queue since
  8 Nov). Encoder order: `h264_nvenc`, `h264_qsv`, `h264_amf`, `h264_mf`, `libx264`
  (first that works in a quick probe).
- **Captures:** WGC (`win/wgc.rs`) copies each frame GPU → staging → CPU → wgpu upload (two
  GPU devices: D3D11 for WGC, D3D12 for wgpu). Cameras and files go through FFmpeg (`ffsrc.rs`),
  sound through cpal/WASAPI, per-app sound through process loopback (`win/app_sound.rs`).

### 5.1 Measure first (write the numbers in §8)
1. `cargo test -p ferrocore --release pacing_holds -- --ignored --nocapture` (1080p60,
   `FERROCAST_PACE_SECS=60`): the share of late frames, **idle** and **with the CPU busy**
   (e.g. a game, or `cargo build` running). The Mac was ~1% → 0 after its fix.
2. **Sound against picture:** the A/V sync test `a_flash_and_its_beep_stay_together`
   (engine.rs) and the `av_sync`, `media_sync`, `cpu_soak` examples are `#[cfg(target_os =
   "macos")]`.
   - First task here: make the sync test run on Windows. It plays a clip through the Media
     source (`ffsrc` on Windows) and matches each flash with its beep.
   - Run `cargo test -p ferrocore --release a_flash_and_its_beep_stay_together -- --nocapture`
     with `FERROCAST_SYNC_SECS=60` (target: every beep within one frame, no drift).
3. **CPU:** port `cpu_soak` to Windows displays (`win::screen` / WGC), or use the smoke script's
   `cpu_percent_of_machine`, at 30 and 60 fps.
4. Smoke test at 30 and 60 fps (§2): `engine_fps`, `frame_ms`, `lagged`, `record_dropped`, CPU.
5. **Minimized and in the background:** start a recording, minimize Ferrocast, put a game or
   a busy app in front for 2 minutes. Compare `lagged` with the window in front. This is where
   Windows 11 throttles (EcoQoS), as macOS's App Nap did.
6. **Which encoder was picked.** `VideoEncoder::name()` in `video.rs` says it ("h264_nvenc",
   "libx264"…), but nothing logs or shows it yet.
   - Small task: log it when an output starts (`tracing::info!`) and add it to
     `/studio/engine/stats`.
   - Expected: NVIDIA `h264_nvenc`, Intel `h264_qsv`, AMD `h264_amf`; `libx264` means the
     hardware probe failed: find out why.
   - Then check a 10-minute 1080p60 recording: about 36,000 frames
     (`ffprobe -count_frames`), no gaps.

### 5.2 Build (only after 5.1, with before/after numbers)
1. **The frame thread's priority on Windows** (the Mac has `on_time()`; Windows has nothing):
   - In `on_time()` for Windows: `SetThreadPriority(GetCurrentThread(),
     THREAD_PRIORITY_HIGHEST)`, or MMCSS `AvSetMmThreadCharacteristicsW("Games")` (OBS uses
     high priority for its video thread).
   - Opt out of power throttling for the process and the frame thread:
     `SetProcessInformation(ProcessPowerThrottling)` and
     `SetThreadInformation(ThreadPowerThrottling)` with `ControlMask` =
     `PROCESS_POWER_THROTTLING_EXECUTION_SPEED` and `StateMask` = 0. This is Windows' App Nap.
   - Check that `std::thread::sleep` really wakes within ~1 ms. Rust uses a high-resolution
     waitable timer on Windows 10 1803+; measure it in `pacing_holds`. If not, use
     `CreateWaitableTimerExW(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION)`.
   - The `windows` crate is already a dependency of ferrocore. Add the features you need in
     `crates/ferrocore/Cargo.toml`.
   - Add a test like the Mac's (the thread's priority after `on_time()`).
2. **Zero-copy capture** (`WINDOWS-NEXT.md` Step 2), only if 5.1 shows capture cost matters
   (1440p/4K, 60 fps):
   - Put WGC's D3D11 device on the same adapter as wgpu.
   - Use a shared NT-handle texture opened in D3D12 and wrapped with
     `wgpu::hal::dx12` `texture_from_raw`.
   - Keep the CPU path as the fallback.
3. **GPU readback for encoding:** today one readback per output frame to NV12 for FFmpeg.
   Measure its ms at 1080p60 before touching it.

---

## 6. Other Windows areas to look at
- **Preview:** a child HWND (`crates/studio/src/ffi/win.rs`). Check resize while recording,
  125%/150% scaling, two monitors with different scaling, and dialogs over it (holes).
- **Web page sources:** run in a hidden Edge over DevTools (`crates/studio/src/ffi/headless.rs`).
  Check a browser source and an overlay URL: CPU, and it closes with the app (no leftover
  `msedge.exe` with our profile).
- **Sound:**
  - Desktop sound (Ferrocast's own left out), a mic, and an app's sound (Chrome).
  - Monitoring to the speakers.
  - Unplugging a USB mic mid-recording: the watchdog restarts it.
- **Remove background / noise removal:** Remove background is RVM via tract, Strong noise is
  DeepFilterNet (release builds with `--features deepfilter`). Check CPU at 1080p30.
- **Not on Windows yet:** the virtual camera (needs a signed camera driver: owner said no paid
  signing), so don't promise it.
- **Installer / Store:**
  - NSIS installer with branded images.
  - Smart App Control can block a new unsigned build the first time (`WINDOWS-NEXT.md`).
  - Microsoft Store MSIX is the free signed route (`docs/plan/MICROSOFT-STORE.md`). 3.0.1 was
    submitted 9 Nov (ID 9N0L9FPR0P8B), passed certification and was published on 11 Nov.
    A newer version needs a new submission (`windows-store.yml` builds the MSIX).

---

## 7. Order of work

1. Pull both repos, build (§2), run the smoke test at 30 and 60 fps. Note the numbers.
2. §4 checks, top to bottom. Fix what fails (each fix: cause, test, check here).
3. §5.1 measurements, then §5.2.1 (thread priority + power throttling) with before/after.
4. §6 areas.
5. Write the results into §8 below. Commit **only when the owner says** (and CodeRabbit first).

---

## 8. Results (fill in on the Windows PC)

| Check | Result | Numbers / notes |
|---|---|---|
| Smoke 30 fps | Pass (10 Nov) | 29.9 fps, 3 late, 0 of 645 recorded frames dropped, 5.3% CPU, sound -16.5 dB. NVIDIA GTX 1650 Ti, PC on the "Silent" power plan |
| Smoke 60 fps | Pass, just (10 Nov) | Silent plan: 54.8 fps (bar 54), 265 late, 47 of 1320 dropped, 12.2% CPU. Performance plan: 59.5 fps, 27 late, 10 dropped, 7.6% CPU. The power plan is most of it  After the priority and media fixes: 59.6 fps, 11 late, 8 dropped. The smoke test has the odd run of 80-150 late on every build (a fresh home downloads the background-removal model and FFmpeg meanwhile, and the mic overruns), and its "preview placed while cameras are listed" check once took 602 ms (bar 500) while the camera list took 3.7 s: compare builds with `tools/windows-pace.ps1`, which is steady |
| 4.1 Add a source / freezes / filters / quit | Pass, after fixes | No grey box; lists at once; this PC has no camera ("No camera found" is right); List view kept. **Fixed:** the window list showed tool and cloaked windows (our single-instance window, NVIDIA's overlay, Program Manager, suspended Settings, the input panel): now OBS's rules (`win/screen.rs` `pickable`). Adding a video stalls the window's thread once, ≤ 250 ms (`OPEN_WAIT`, by design). Filters on/off/remove and crop work. **Fixed:** a removed source kept running in the engine (14 FFmpegs, 32% CPU, frames late, a camera would stay on) and its name stayed taken: a source goes when no scene shows it, as in OBS (`stage.rs` `drop_unused`, also on load; Undo brings it back) |
| Instant replay | Pass, after fixes | Re-checked: on air 1.6 s after Save with its picture already in, back 11.1 s later as the clip ended. **Fixed:** media forced to 60 RGBA fps (500 MB/s for 1080p) fell behind, so the clip ended ~30 s late and the CPU worked hard: now the file's own rate (`ffsrc.rs`). **Fixed:** the scene went on air before the file opened (up to 10 s black): now when it plays and its first picture is in (`studio.rs` `replay_wait`, `Stage::has_picture`) |
| Web text (http and https) | Pass | Value drawn, updates within its 5 s minimum, https (schannel) works, a bad address warns once and draws nothing |
| Keys on screen | Pass | W lights while held, dark in a classic password box. A browser's password field isn't detected on Windows (told the owner) |
| Countdown, Sound bars, Copy, Freeze, Effects ×8, Top layer | Pass, after a fix | Countdown ticks, the font looks right. Bars: the 440 Hz tone lights one low-middle bar (a hidden source plays no sound, as in OBS). Copy: same picture, not in the mixer. **Fixed:** a Copy was 0×0 to the editor, so it couldn't be sized (`stage.rs` `own_size_of`). Freeze holds (0 change), moves when off. All 8 effects draw on D3D12, no shader errors. Top layer is over the scene in the recording |
| Automations, logic plugin, panel plugin | Pass, panel fixed | Follow rule: Tests at 0.1 s, back at 2.2 s; a rule with no steps says why. Logic plugin: its log line, its allowed step ran, a step it didn't list was refused and logged. **Fixed (every system):** a panel's script never ran in the app, as a `srcdoc` frame is held to the dashboard's CSP (`script-src 'self'`): the engine now serves `/plugin-panel/{id}` with its own policy (`assets.rs`, dashboard `plugins-settings.tsx`) |
| Settings / setup / sign-in | Pass, sign-in not tried | Layout fine at 100%, 125%, a short window and 375 px; picks in Ferrocast blue; setup 3 steps to Connect; no Mac words. **Fixed:** "Recording only" set high quality, which the Windows engine refused: constant quality on NVENC (`-cq`), QSV (`-global_quality`), AMF (`-rc cqp`), MF and x264 (`ffenc.rs`). Sign-in needs a CI build (client ids) and the owner's account |
| Frames late at 1080p60, idle / busy (before → after) | Measured with the app (`tools/windows-pace.ps1`, a colour + a 1080p clip, recording, 60 s each, Silent plan) | Before: idle 0.08%, busy (burners on 6 of 8 threads) 9.84% late. **After** `on_time()` on Windows (the frame thread at THREAD_PRIORITY_HIGHEST, the thread and app out of power throttling, `engine.rs`): idle 0.03%, busy 0.19% (7 of 3,608). `FERROCAST_FRAME_PRIORITY` = highest (default), above, mmcss ("Games") or normal, and `FERROCAST_FRAME_THROTTLING=on`, to compare or as a way out. MMCSS measured the same (0.06% / 0.19% / 0.08%); in the smoke test (screen, Edge window, desktop sound, mic) every mode, normal included, had runs of 0-15 late and the odd run of 110-120, so those spikes aren't the priority (a fresh home downloads the background-removal model and FFmpeg meanwhile)  Final build (with a 1080p clip on the card's NV12 path): **0 late** idle, busy and minimized (3,602-3,613 frames each). A 10-minute 1080p60 recording: 35,996 frames (59.99 a second), longest step 34 ms, 6 late |
| A/V sync test on Windows (ported) | Ported; **fixed** a 0.36 s offset | `a_flash_and_its_beep_stay_together` and its helpers now build on Windows (`cfg` macOS or Windows); it needs `ffmpeg` on the PATH (skipped without). The 6 s default passed but 60 s (`FERROCAST_SYNC_SECS=60`) showed a file's sound 0.35-0.39 s **before** its picture, steady, no drift: FFmpeg hands sound over ~0.44 s ahead of real time (`-re`, with or without `-readrate_initial_burst 0`), pictures about on time, and sound was stamped when it came. Now each piece is placed by its place in the file on the pictures' clock (`ffsrc.rs` `Shared::start_ns`, `Media::take_sound`): -31 to +21 ms over 60 s, within a frame. Left: a file whose sound track starts later than its picture (a stream `start` offset) still has its sound that much early (FFmpeg starts each output at 0). Tests: ferrocore 107 pass; studio 73 pass (`headless::a_page_reaches_the_engine` failed once under a full parallel run, passes alone: Edge starting late under load) |
| CPU at 30 / 60 fps | Measured; video files on the card | `cpu.ps1` (scratch): one 1080p30 clip at 30 fps, 13.2% of the machine (the app 3.6%, its two FFmpegs 9.5%). Three clips at 60 fps: 78% CPU, 12 fps, 594 late in 20 s before the media fix → 49%, no late frames after.  **Then (10 Nov night) NV12 on the card:** FFmpeg hands a file's pictures over as NV12 (1.5 bytes a pixel, no RGBA made) and `yuv.wgsl` turns them into RGB in the source's own texture (`Compositor::upload_nv12`, `Media::open_for_card`, `Fresh::Yuv`; BT.709 or BT.601 and TV or full range from the file's tags, untagged: BT.709 from 720 lines; chroma sited left as H.264's). One 1080p30 clip at 30 fps: 12.2% → **4.8%** of the machine; three at 60 fps: 50.7% → **16.8%**, no late frames. Colours against FFmpeg's own bars: mean difference 0.67 (BT.709) and 1.05 (BT.601) of 255, the RGBA path 0.69 and 1.66. 60 fps files, seek, restart, looping and Instant replay checked in the app; the ffsrc tests now run on Windows too (they looked for FFmpeg in Mac and Linux paths only). The Mac keeps AVFoundation's pictures |
| Minimized behind a game (before → after) | Measured | Before: minimized and busy 17.9% late (646 of 3,604): Windows throttles a minimized app. **After:** 0.03% (1 of 3,612), recording dropped 1 |
| Encoder picked | h264_nvenc | Logged when an output starts ("an output starts encoder=…") and in `/studio/engine/stats` `record.encoder` / `stream.encoder`; the Stats panel says "Encoder: NVIDIA graphics card". 10 minutes at 1080p60: 35,996 frames, no gap over two frames |
| Status bar FPS | Fixed | It was the share of frames made since the Studio started, so one busy minute kept it at "20 / 30" while every frame came on time. Now the last second's rate, as OBS shows it (`Stage::fps_now`) |
| Preview while resizing | Pass, slow to follow | 30 resizes while recording: no GPU errors, the app lived, the file was written. With the PC compiling, the picture took 2-5 s to move to its new place |
| Preview, web sources, sound, background removal | Pass, web pages fixed | One app's sound (the Edge tone window) recorded at -13.8 dB. Remove background on a 1080p30 clip: +11.5% CPU, 30 fps held. Strong noise removal (DeepFilterNet) on the mic: +2.6%. Not done here: two monitors, 150% scaling, unplugging a USB mic (needs the owner).  Web page source: drawn by the hidden Edge, transparent where the page is; its Edge and temp profile go with a normal quit. **Fixed:** `browser_source` wasn't in the Windows kinds, so Add a source had no web page or overlay (`kinds.rs`) |

**What's next (10 Nov):**
- The preview following a resize faster when the PC is busy (it's moved from the window's
  thread, normal priority).
- Sign-in on Windows ("Finish in browser") with a CI-built installer and the owner's account;
  two monitors and 150% scaling; unplugging a USB mic mid-recording.
- The pacing check is `tools/windows-pace.ps1`. The other scripts used here (`fc.ps1`, `cdp.mjs`, `cpu.ps1`, `check42.ps1`…) lived in
  the session's scratch folder; the useful ones could become `tools/windows-*.ps1`.

---

## 9. Releasing (only when the owner says "release")

1. Bump the version: `version` in `Cargo.toml`, then `cargo update --workspace`.
2. Add a `## x.y.z` section at the top of `CHANGELOG.md`, written for streamers.
3. Commit `chore(release): x.y.z`.
4. Push both repos, then `git tag vX.Y.Z` and `git push origin vX.Y.Z`.
5. Cancel the run the tag starts: `gh run list --workflow release.yml`, then
   `gh run cancel <id>`.
6. Start it with the web branch:
   `gh workflow run release.yml --ref vX.Y.Z -f web_ref=own-engine-only`.
7. When it's done, check the draft has all 14 files:
   - `.rpm`, `.deb`, `.AppImage`, `Ferrocast.app.tar.gz` and `_x64-setup.exe`, each with its
     `.sig`;
   - the `.dmg`, `ferrocast.rb`, `SHA256SUMS.txt`, and `latest.json` naming 6 platforms.
8. Publish: `gh release edit vX.Y.Z --draft=false --latest`.
9. If a macOS job fails for want of a runner: `gh run rerun <id> --failed`.
