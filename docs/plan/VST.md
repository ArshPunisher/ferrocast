# VST3 sound plugins, run in a locked-down helper (owner's pick, 11 Nov 2026)

The last plugin from OBS-COMPETE.md §3. A VST is native code from someone else, so it never runs
inside Ferrocast: each one runs in its own helper process, `ferrocast-vst`, which has no network
and can't read the owner's documents or Ferrocast's own folder (stream keys, sign-ins).

## How it works

```
mixer thread ──(block of 1024 stereo samples)──► ferrocast-vst (one per VST filter)
             ◄──────────(the block, processed)──   VST3 module, its own window
```

- **Crate** `crates/vsthost` (`ferrocast-vsthost`): the client the engine uses, and the helper
  binary `ferrocast-vst` (`src/bin`). VST3 through the `vst3` crate (pure Rust bindings, MIT/Apache,
  no Steinberg SDK to build). Formats: VST3 first (Mac, Windows, Linux). Audio Units later.
- **Test plugin** `crates/vst-testgain`: our own tiny VST3 (a gain), so tests never need anyone
  else's plugin.
- **Delay:** the mixer hands each block over and waits for it (a few ms at most). On a late or
  dead helper the block passes through dry, the filter is marked, and the helper is started
  again. No block of delay is added, plus the plugin's own latency (reported, the mixer files
  the sound that much earlier, as for the strong noise removal).
- **Protocol** (helper's stdin/stdout): a byte op, a u32 length, the payload. Process (planar
  f32 left then right), set/get state (component and controller state), open/close the window,
  latency. `scan` mode lists plugins as JSON lines.
- **State:** the plugin's own state (VST3 component + controller chunks), base64 in the filter's
  settings (`ferrocast_vst`: `path`, `cid`, `name`, `state`), saved with the collection.
- **Engine:** `AudioEffect::Plugin { slot }`; slots are kept by `afx` (source, filter, path, cid →
  slot; the state lives beside it, so tweaking it in the window doesn't restart the plugin).
  The studio installs the maker like the denoiser.

## Locked down

- **Mac:** the helper starts under `sandbox-exec` with a profile: no network; no writes except
  its own temp folder; no reads of `~/Documents`, `~/Desktop`, `~/Downloads`, `~/Pictures`,
  `~/Movies`, `~/Music`, `~/.ssh`, `~/Library/Keychains`, Mail/Messages, or Ferrocast's home.
  Plugins still read their own files (`/Library/Audio`, `~/Library/Audio`,
  `~/Library/Application Support`) and draw their window.
- **Windows:** to do on the PC: a low-integrity or AppContainer process in a job object, no
  network (firewall rule or AppContainer without the internet capability).
- A crash, a hang or a garbage block ends that helper only; the stream goes on.

## Steps (11 Nov: 1-5 done on the Mac)

Tested: `cargo test -p ferrocast-vsthost` (found, run, state kept, a hang or crash holds nothing
up and ends the helper after 2 s of silence, the Mac sandbox: no network, no Ferrocast folder, no
~/Documents, no writes outside its temp folder) and `-- --ignored` for its window. Live in the
dev app (audit home, a looping tone): running 15 s with no restart, window opened and closed,
state saved without a restart. Notes:
- A sandboxed process can't sandbox another: inside `dev-home.sb` VSTs are off and say why. The
  dev app built with `TAURI_CONFIG='{"identifier":"com.clearcraftlabs.ferrocast.devtest"}'`
  doesn't need that sandbox (no single-instance clash with the installed app).
- The helper's writes go through its own thread: a plugin that stops reading filled its pipe and
  held the mixer 9.7 s before (found by the hang test).
- The mixer keeps a filter whose settings didn't change (every edit set them all again: a VST,
  and the strong noise removal, started over at every edit).
- Release: the Mac job builds `ferrocast-vst` for both architectures and ships it beside the
  program (`externalBin`, release.yml). Not yet run on CI.


1. Helper: load a VST3, scan, process, state (Mac). Test plugin. Tests.
2. Sandbox on the Mac; test that the helper can't read a file in a denied folder or open a socket.
3. Engine: `AudioEffect::Plugin`, slots, the maker, dry on timeout, restart.
4. The plugin's own window (helper's main thread runs the UI; parameter edits reach the audio).
5. Dashboard: "VST plugin" in Add a filter (sound), pick from the scanned list, Open window.
6. Windows: build, sandbox, test on the PC.

Then the plugin catalog: a signed list in a GitHub repo, Browse in Settings › Plugins.
