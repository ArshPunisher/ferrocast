# Windows session, 10-11 Nov 2026: what happened, what was fixed, where it stands

For the next Claude (on the Mac or on this PC). This was written at the end of a long session
on the owner's Windows 11 PC that worked through `docs/plan/WINDOWS-HANDOFF.md` §4-§6. That
file's §8 table has every result; this file adds the story: what got in the way, how each
problem was found and fixed, and what's left. Read the handoff first, then this.

---

## 1. Where things stand (11 Nov)

- **Branch** `own-engine-only` in both repos. Everything below is **committed and pushed**.
- **Not released.** Still 3.1.0. A local installer was built for the owner to try:
  `D:\Structure\ferrocast\target\release\bundle\nsis\Ferrocast_3.1.0_x64-setup.exe`. Local
  builds have no Twitch/YouTube client ids, so sign-in doesn't work in it.
- **Tests:** ferrocore 133 pass, studio 73, server 107, dashboard 243 (vitest). `cargo clippy
  --all-targets` and eslint are clean. tsc is clean.
- **CI** wasn't looked at after these pushes: check `CI` and `Windows smoke` on GitHub.

### Commits (ferrocast)
| Commit | What |
|---|---|
| de68a34 | fix(ferrocore): leave tool and cloaked windows out of the window list |
| 18a4209 | fix(ferrocore): constant quality on FFmpeg's encoders |
| c5f50a6 | perf(ferrocore): video files on the card, at their own rate, in sync |
| 0d64376 | perf(ferrocore): raise the frame loop on Windows, out of throttling |
| b01cab8 | feat(studio): stats name the encoder; FPS is the last second's |
| 55070b5 | fix(studio): a copy of a source has its source's size |
| 7236c20 | fix(studio): a source goes when no scene shows it, as in OBS |
| bdc5d82 | fix(desktop): an instant replay goes on air once its picture is in |
| 0c3f626 | fix(server): serve a panel plugin as a page of its own |
| 410e74f | fix(studio): list web page sources on Windows |
| 732beb8 | test(ferrocore): run the sync test on Windows, steadier GPU tests |
| bfd9bac | fix(tools): the smoke script ends in Windows PowerShell 5.1 too |
| 54d45f4 | feat(tools): a Windows frame pacing check |

### Commits (ferrocast-web)
| Commit | What |
|---|---|
| 115e3fe | fix(dashboard): a panel plugin's page loads from the engine |
| 829173c | feat(dashboard): the Stats panel names the encoder |
| 4d2ac5c | docs(plan): the 10 Nov Windows checks in the status log |

The commits are split by fix. Where two fixes shared a hunk the commit body says so (the ffsrc
rate/sync/NV12 work is one commit, and the new Studio tests ride with 7236c20). Intermediate
commits weren't each built: the tip is what was tested.

---

## 2. The PC and what it needed

- Laptop: Intel i5-10300H (8 threads), NVIDIA GTX 1650 Ti plus Intel UHD, 16 GB RAM, Windows 11
  build 26300. **Power plan "Silent"** (the owner's choice: leave it, and say so next to any
  frame numbers). The Performance plan makes 60 fps much smoother.
- **Smart App Control had to be turned off to build** (the owner did it). When on, it blocked
  the newly built build scripts and proc-macro DLLs. Rust reported that as `E0463 can't find
  crate` for a different crate on each run. The tell is in Event Viewer → CodeIntegrity
  (event 3077 on `target\...\build-script-build.exe` / `*.dll`). It's still off on the owner's PC.
- **Memory:** once a release build plus the test app plus its FFmpegs ran the PC low, and
  Claude Code killed the background build. Build with `cargo build -j 4 ...`, and stop the test
  app first.
- **Release builds take 10-17 minutes** (thin LTO, `codegen-units = 1`). `cargo test --release`
  of the studio crate took over an hour. Run tests in the **dev profile** (`cargo test -j 4 -p
  ...`): after the first build they're quick.
- The dashboard is embedded **when the server crate compiles** (rust-embed). Run `npx
  pnpm@11.22.0 build` in ferrocast-web *before* the cargo build when the dashboard changed.
- Env for every build (PowerShell): `$env:Path += ";$env:USERPROFILE\.cargo\bin;C:\Program
  Files\CMake\bin;C:\Program Files\LLVM\bin"; $env:LIBCLANG_PATH="C:\Program Files\LLVM\bin";
  $env:FERROCAST_WEB_DIST="D:\Structure\ferrocast-web\dist"`.
- Test-app build: `cargo build -j 4 --release -p ferrocast-desktop --features deepfilter
  --config 'profile.release.package.ferrocast-desktop.debug-assertions=true'` (writes the token
  file). Installer: `cargo tauri build --bundles nsis --features deepfilter` in `apps/desktop`.
- `gh` isn't installed here, and CodeRabbit isn't either: reviews were done by hand.

### Running the app for tests (how this session did it)
- Copy `target\release\ferrocast.exe` somewhere else and run the copy. The build can't replace
  a running exe.
- Set `FERROCAST_HOME` and `FERROCAST_DEBUG_TOKEN_FILE` to a test folder.
  `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9333` lets Playwright drive the
  app's own WebView2 (`chromium.connectOverCDP('http://127.0.0.1:9333')`, with Playwright from
  `ferrocast-web/apps/dashboard/node_modules`). CDP screenshots don't include the native
  preview (a child window): capture the window's rectangle for that.
- **The app is single-instance.** A second copy forwards to the first and exits, so the token
  file never appears. The test app and the owner's installed Ferrocast can't run at once.
- The token file is written **only at start**: don't delete it while the app runs.
- **The engine starts lazily:** `/studio/engine/start` says running, but nothing draws (0
  frames) until something asks for a picture. A screenshot of a scene wakes it.
- **The canvas size and fps are fixed for the engine's life:** PUT settings *before*
  `/studio/engine/start`, or the change applies at some later reopen.
- Set `record_folder` to the test folder, or recordings land in the owner's
  `Videos\Ferrocast`.

### Windows PowerShell 5.1 traps (each cost time)
- **Variable names ignore case:** `$s` overwrites `$S`. It broke the smoke script (`$crash` was
  the `-Crash` switch) and two of this session's own scripts.
- `$x | ConvertTo-Json` turns a one-item list into an object: use `ConvertTo-Json -InputObject`.
- `Invoke-RestMethod` gives a JSON array as one object: pipe through `ForEach-Object { $_ }`.
- `powershell -File script.ps1 -Modes a,b` passes `"a,b"` as one string.
- `Start-Process -WindowStyle Hidden` also hides the first window the program opens.
- No `??` operator in 5.1.

---

## 3. What was found and fixed (story, cause, fix)

1. **Window list full of junk.** It showed Ferrocast's single-instance window, NVIDIA's
   overlay, Program Manager, a suspended Settings and the input panel. `windows()` only checked
   visible + titled. Now OBS's rules (no tool windows, no children, nothing cloaked) for the
   picker. Starting a capture still finds any window. (`win/screen.rs`)
2. **Removed sources kept running.** CPU sat at 26-32% with one clip on air: 14 FFmpegs were
   still running for sources removed from every scene. The stage left them in the collection
   and the engine opened every source in it. Now a source goes when no scene shows it, as in
   OBS, also on load. An old test that expected "a source can come back from the list" was
   changed with it. (`stage.rs` `drop_unused`)
3. **"Recording only" setup couldn't record.** The dashboard's new setup (8 Nov) sets
   `record_quality: high`, and own.rs refused constant quality off a Mac. FFmpeg's encoders now
   take a quality (NVENC `-rc vbr -cq`, QSV `-global_quality`, AMF `-rc cqp`, MF
   `-rate_control quality`, x264 `-crf`). (`ffenc.rs`, `own.rs`)
4. **Instant replay ended ~30 s late and was black for up to 10 s.** Media forced `-r 60`
   RGBA (500 MB/s for 1080p): FFmpeg fell behind, so "ended" came late. The app also put the
   replay's scene on air before the file had opened. Now files play at their own rate, and the
   scene waits until the replay plays and its first picture is in. (`ffsrc.rs`, `studio.rs`,
   `Stage::has_picture`)
5. **Panel plugins never worked in the app (any system).** A `srcdoc` frame inherits the
   dashboard's CSP (`script-src 'self'`). The engine now serves `/plugin-panel/{id}` with its
   own policy, and the dashboard frames that URL when it's the engine's own dashboard.
   (`assets.rs`, web `plugins-settings.tsx`)
6. **No web page source on Windows.** `browser_source` was missing from the Windows kinds
   although the hidden-Edge path existed. Added. (`kinds.rs`)
7. **A Copy of a source couldn't be resized.** The editor sized it 0×0. It now takes the
   copied source's size. (`stage.rs` `own_size_of`)
8. **Missed frames behind a game, and when minimized.** `on_time()` did nothing on Windows.
   Now the frame thread is at HIGHEST, out of EcoQoS and timer throttling. 1080p60: busy 9.84% →
   0.19% late, minimized + busy 17.9% → 0.03%. `FERROCAST_FRAME_PRIORITY` = highest (default) /
   above / mmcss / normal, and `FERROCAST_FRAME_THROTTLING=on`, exist to compare. MMCSS measured
   the same. (`engine.rs`)
9. **FPS shown low long after a hiccup.** It was an all-time average. Now it's the last
   second's rate. (`Stage::fps_now`)
10. **Which encoder?** Nothing said. Now logged at output start and in stats
    (`record.encoder`, `stream.encoder`), with "Encoder: NVIDIA graphics card" in the Stats
    panel. This PC: `h264_nvenc`.
11. **Sound 0.36 s ahead of picture for video files (Windows).** The 6 s sync test passed and
    the 60 s one found it. Measuring the two FFmpegs side by side showed FFmpeg hands sound over
    ~0.44 s early (with or without `-readrate_initial_burst 0`), and sound was stamped on
    arrival. Now each sound piece is placed by its position in the file, on the pictures'
    clock (the earliest picture arrival minus its frame index / fps). Sound for moments
    already shown is dropped, or the mixer would raise its latency for good. -31..+21 ms over
    60 s. (`ffsrc.rs` `Shared::start_ns`, `Media::take_sound`)
12. **Video files cost ~10-12% CPU each.** FFmpeg made RGBA on the CPU and piped 4 bytes a
    pixel. Now NV12 (1.5 bytes, no conversion) and a shader (`yuv.wgsl`) makes RGB into the
    source's own texture (`Compositor::upload_nv12`, `Media::open_for_card`, `Fresh::Yuv`). The
    matrix and range come from tags (untagged: BT.709 from 720 lines). The first version
    fringed sharp vertical edges: chroma must be sited left (H.264 4:2:0), fixed. 1 clip 12.2% →
    4.8%, 3 clips at 60 fps 50.7% → 16.8%. Colours vs FFmpeg's own bars: mean 0.47 / 1.05 of
    255 (the RGBA path: 0.69 / 1.66). The Mac is unchanged (AVFoundation).
13. **Tests that never ran on Windows.** ffsrc's `have_ffmpeg()` only looked in Mac/Linux
    paths. The sync test was Mac-only. The countdown test was timing-flaky in debug (font
    loading). The preview test wanted grey 128 where NVIDIA gives 127. All fixed.
14. **Smoke script died at the end in PowerShell 5.1** (`$crash` vs `-Crash`). Fixed.
15. Earlier the same day (before the 3.1.0 checks): the window-list and Smart App Control
    findings above, and the 8 Nov hidden-console fix was already in.

Things checked and fine, no change needed:
- Add a source opens fast.
- Filters, crop, freeze, the 8 effects (D3D12, no shader errors), top layer, countdown, sound
  bars, copy, web text (http and https), keys on screen (dark in a password box; a browser's
  password field isn't detected on Windows; the owner was told).
- Automations, logic plugins, quitting with the Studio running (no crash dump), Settings
  layout at 100/125%/short/375 px.
- App sound capture, background removal (+11.5% CPU at 1080p30), DeepFilterNet (+2.6%).
- Resizing while recording (the preview took 2-5 s to catch up while the PC was compiling).
- 10 minutes at 1080p60: 35,996 frames, longest step 34 ms.

---

## 4. Measuring (so numbers can be compared)

- **`tools/windows-pace.ps1`** (in the repo): 1080p60, a colour and a 1080p clip, recording;
  60 s idle, busy (burners) and minimized + busy. It's steady: use it to compare builds. Final
  build: 0 late in all three.
- **`tools/windows-smoke.ps1`**: the all-round check (screen, an Edge window with a tone,
  desktop sound, the mic). It has an odd run of 80-150 late frames on **every** build (a fresh
  home downloads the background-removal model and FFmpeg meanwhile, and the mic overruns), and
  its "preview placed while cameras are listed" check once took 602 ms (bar 500). Don't judge a
  change by one smoke run.
- Scripts this session kept in its scratch folder (gone now, easy to redo with the API):
  - CPU of one exe with n clips (app + child FFmpegs, % of the machine);
  - colour bars vs a truth PNG (`smptehdbars`, tagged BT.709 / BT.601 files);
  - arrival timing of flashes and beeps from two FFmpegs;
  - the §4.2 checks;
  - Instant replay timing;
  - Keys on screen with own text and password boxes.
  The API calls are all in `docs/API.md`.

---

## 5. The owner (how they work)

- Short, plain English. They ask "tell me in short" often. Show numbers before → after.
- **Never sit idle while a build runs:** do the next task meanwhile (they said so).
- Ask before capturing their screen, mic or keyboard from a test build. On 10 Nov they said yes
  to screen + mic for the smoke test, and yes once to the keys-on-screen check.
- They want things **fully tested** before an installer: when told something was a "later
  improvement" (the NV12 path) they asked for it to be done and tested first.
- Commit only on "commit"/"commit and push"; author is the owner only, no AI credit (the
  `.githooks/commit-msg` hook checks: run commits with `git -c core.hooksPath=.githooks`).
- They said no to paid or SignPath code signing for now. Microsoft Store is the plan
  (`docs/plan/MICROSOFT-STORE.md`).

---

## 6. What's left

- **Re-check on the PC (found by the Mac's audit of this work, 11 Nov, fixed there):**
  - **A looping video file went silent after its first loop** (Windows, Linux). At each loop
    FFmpeg hands a burst of pictures over early, the sound clock (`Shared::start_ns`, the
    earliest start ever seen) jumped ~0.4 s earlier, and every later sound piece was dropped as
    late. The gap each loop leaves in the sound also made it drift. Now the clock is set by the
    first `SETTLE_FRAMES` pictures and held, and a file's sound goes through
    `aresample=async=1:first_pts=0` (`Out::FileSound`), which fills gaps with silence. That
    also gives a sound track that starts after its picture its silence first (the "left" item
    below). Test: `ffsrc::tests::a_looping_file_keeps_its_sound`. On the PC: re-run the 60 s
    sync test and play a looping clip with sound for a few minutes.
  - **VST plugins (new, docs/plan/VST.md)** are Mac only: on Windows the helper needs its
    sandbox first (a low-integrity or AppContainer process in a job object, no network). Until
    then `ferrocast_vsthost::available()` keeps them off there and says so.
  - **A panel plugin could send its frame to another page** (any system, since 3.1.0): a
    sandboxed frame may still navigate itself, and the dashboard kept running the steps that
    page asked for. Now a second load closes the panel and its messages are ignored
    (`plugins-settings.tsx`, e2e `a panel that sends its frame to another page…`).

- **Check CI** on GitHub for these pushes (`CI`, `Windows smoke`).
- **The owner tests the local installer** (recording, a video file in a scene, Instant replay,
  the Stats encoder line).
- **Sign-in on Windows** ("Finish in browser") with a CI-built installer and the owner's
  account.
- Not tried on this PC: two monitors, 150% scaling, unplugging a USB mic mid-recording.
- **Preview catching up after a resize** is slow when the PC is busy (it's moved from the
  window's thread at normal priority).
- **Network media** (`studio/netmedia.rs`, guests' cameras, streams) reads FFmpeg too: it may
  have the same early-sound timing as files had. Not measured.
- A file whose sound track starts later than its picture (a stream `start` offset) still gets
  its sound that much early: FFmpeg starts each output at 0.
- The playlist source (`source/series.rs`) still uses the RGBA path (`Media::open`). It could
  use `open_for_card` too.
- Releasing: only when the owner says (WINDOWS-HANDOFF §9).
