//! Ferrocast's side: the helper started (locked down), a block sent and waited for, its state
//! read and set. Every wait has a limit: a slow, hung or crashed plugin leaves the sound as it
//! was and never holds the mixer up for long.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::Found;
use crate::proto::{self, read_frame, write_frame};

/// How long a block may take before it's left as it was (the mixer's block is 21 ms).
pub const BLOCK_BUDGET: Duration = Duration::from_millis(8);
/// How long the helper may take to load its plugin.
const START_WAIT: Duration = Duration::from_secs(15);
/// How long a state or window request may take.
const ASK_WAIT: Duration = Duration::from_secs(5);

/// Where the helper is: `FERROCAST_VST_HELPER`, else beside Ferrocast's own program.
pub fn helper_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FERROCAST_VST_HELPER") {
        return Some(PathBuf::from(p));
    }
    let me = std::env::current_exe().ok()?;
    let name = if cfg!(windows) { "ferrocast-vst.exe" } else { "ferrocast-vst" };
    let at = me.parent()?.join(name);
    at.is_file().then_some(at)
}

static PRIVATE: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();

/// Folders a plugin may never read (Ferrocast's own: stream keys, sign-ins), on top of the
/// owner's documents and the like.
pub fn keep_private(dirs: Vec<PathBuf>) {
    let m = PRIVATE.get_or_init(Mutex::default);
    if let Ok(mut l) = m.lock() {
        l.extend(dirs);
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn private_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = PRIVATE.get().and_then(|m| m.lock().ok().map(|l| l.clone())).unwrap_or_default();
    if let Some(h) = std::env::var_os("FERROCAST_HOME") {
        out.push(PathBuf::from(h));
    }
    out
}

/// The Mac's sandbox profile for the helper: no network at all; writes only to its own temp
/// folder; none of the owner's documents, keys, mail or messages, nor Ferrocast's folders.
/// The plugin still reads the system, its own bundle, and the audio and support folders
/// plugins keep their files in, and draws its window.
pub fn mac_profile(home: &Path, temp: &Path, private: &[PathBuf], readable: &[PathBuf]) -> String {
    let q = |p: &Path| format!("\"{}\"", p.display().to_string().replace('\\', "\\\\").replace('"', "\\\""));
    let mut denied: Vec<PathBuf> = [
        "Documents",
        "Desktop",
        "Downloads",
        "Pictures",
        "Movies",
        "Music",
        "Public",
        ".ssh",
        ".gnupg",
        ".aws",
        ".config",
        "Library/Keychains",
        "Library/Mail",
        "Library/Messages",
        "Library/Cookies",
        "Library/Safari",
        "Library/Mobile Documents",
        "Library/CloudStorage",
        "Library/Application Support/Ferrocast",
        "Library/Application Support/Ferrocast-dev",
        "Library/Application Support/Google",
        "Library/Application Support/Firefox",
        "Library/Application Support/obs-studio",
    ]
    .iter()
    .map(|d| home.join(d))
    .collect();
    denied.extend(private.iter().cloned());
    let deny_read = denied.iter().map(|d| format!("(subpath {})", q(d))).collect::<Vec<_>>().join(" ");
    // The helper's own folder and the plugin's bundle stay readable wherever they are (a later
    // rule wins): AppKit reads the program's folder to make a window, and a plugin its bundle.
    let allow_read = readable.iter().map(|d| format!("(subpath {})", q(d))).collect::<Vec<_>>().join(" ");
    let allow = if allow_read.is_empty() { String::new() } else { format!("(allow file-read* {allow_read})\n") };
    // Ferrocast's own folders are never readable, even inside an allowed one.
    let private_read = private.iter().map(|d| format!("(subpath {})", q(d))).collect::<Vec<_>>().join(" ");
    let private = if private_read.is_empty() { String::new() } else { format!("(deny file-read* {private_read})\n") };
    format!(
        "(version 1)\n(allow default)\n(deny network*)\n\
         (deny file-write* (require-not (require-any (subpath {temp}) (literal \"/dev/null\") (literal \"/dev/dtracehelper\"))))\n\
         (deny file-read* {deny_read})\n{allow}{private}",
        temp = q(temp),
    )
}

/// The helper with `args`, locked down where this system can (the Mac's sandbox; Windows to
/// come, see docs/plan/VST.md).
fn command(exe: &Path, args: &[&str], plugin: Option<&Path>) -> Command {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/var/empty"), PathBuf::from);
        let temp = std::env::temp_dir().join(format!("ferrocast-vst-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp);
        // The temp folder as the system sees it (/var → /private/var), as the sandbox matches.
        let temp = std::fs::canonicalize(&temp).unwrap_or(temp);
        let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let mut readable: Vec<PathBuf> = exe.parent().map(real).into_iter().collect();
        readable.extend(plugin.map(real));
        let private: Vec<PathBuf> = private_dirs().iter().map(|p| real(p)).collect();
        let profile = mac_profile(&home, &temp, &private, &readable);
        let mut c = Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg(profile).arg(exe).args(args);
        c.env("TMPDIR", &temp).current_dir(&temp);
        c
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = plugin;
        let mut c = Command::new(exe);
        c.args(args);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window for it.
            c.creation_flags(0x0800_0000);
        }
        c
    }
}

/// Runs the helper's `probe` (`read <path>`, `write <path>`, `net <addr>`) locked down as a
/// plugin would be: whether it could. For the sandbox's tests.
pub fn probe(exe: &Path, what: &str, target: &str) -> bool {
    command(exe, &["probe", what, target], None)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The sound effects in every VST3 bundle in this system's plugin folders (each bundle looked
/// at by its own helper, so one that crashes is just left out).
pub fn scan() -> Vec<Found> {
    let Some(exe) = helper_exe() else { return Vec::new() };
    scan_bundles(&exe, &crate::helper::module::bundles_in(&crate::helper::module::plugin_dirs()))
}

/// The sound effects in `bundles`, through the helper at `exe`.
pub fn scan_bundles(exe: &Path, bundles: &[PathBuf]) -> Vec<Found> {
    let mut out = Vec::new();
    for b in bundles {
        let Some(path) = b.to_str() else { continue };
        let mut c = command(exe, &["scan", path], Some(b));
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let Ok(mut child) = c.spawn() else { continue };
        // Read while it runs: a bundle with hundreds of effects would fill the pipe and the
        // helper would wait on it until it was ended (CodeRabbit).
        let reader = child.stdout.take().map(|mut so| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = std::io::Read::read_to_string(&mut so, &mut text);
                text
            })
        });
        let until = Instant::now() + START_WAIT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let text = reader.and_then(|t| t.join().ok()).unwrap_or_default();
        match status {
            Some(s) if s.success() => out.extend(text.lines().filter_map(|l| serde_json::from_str::<Found>(l).ok())),
            _ => tracing::warn!(bundle = %b.display(), "a VST plugin couldn't be looked at (left out)"),
        }
    }
    out
}

/// One plugin running in its helper.
#[derive(Debug)]
pub struct Helper {
    child: Child,
    /// To the writer thread, which alone writes to the helper: a helper that stops reading
    /// fills its pipe, and then a write would wait for it (seconds, in the mixer).
    to_helper: SyncSender<(u8, Vec<u8>)>,
    replies: Receiver<(u8, Vec<u8>)>,
    seq: u32,
    /// Its own delay, in samples.
    pub latency: u32,
    pub name: String,
    /// Its window changed something since the state was last read.
    pub changed: bool,
    /// It stopped (crashed, or hung and was ended): blocks pass as they are.
    pub dead: Option<String>,
    /// Blocks left as they were because it was late.
    pub late: u64,
    /// Since when it's said nothing at all (late blocks, no late answers either).
    silent_since: Option<Instant>,
    budget: Duration,
}

/// How long a plugin may say nothing at all before it's taken as hung and ended: long enough
/// for a slow first block or a busy moment, short enough that the sound comes back soon.
pub const HUNG_AFTER: Duration = Duration::from_secs(2);

impl Helper {
    /// Starts the helper `exe` for effect `cid` of `bundle` at `rate`, blocks up to `max_block`,
    /// with its saved `state` (component, controller) when there is one.
    pub fn start(
        exe: &Path,
        bundle: &Path,
        cid: &str,
        rate: u32,
        max_block: usize,
        state: Option<(&[u8], &[u8])>,
    ) -> Result<Helper, String> {
        let path = bundle.to_str().ok_or("the plugin's path isn't text")?;
        let mut c = command(exe, &["run", path, cid, &rate.to_string(), &max_block.to_string()], Some(bundle));
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = c.spawn().map_err(|e| format!("the VST helper didn't start: {e}"))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return Err("the VST helper has no pipes".into());
        };
        // A few frames may wait to be written; past that, a block is dropped (left dry).
        let (wtx, wrx) = std::sync::mpsc::sync_channel::<(u8, Vec<u8>)>(4);
        let mut stdin = stdin;
        std::thread::Builder::new()
            .name("ferrocast-vst-write".into())
            .spawn(move || {
                while let Ok((op, payload)) = wrx.recv() {
                    if write_frame(&mut stdin, op, &payload).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("ferrocast-vst-read".into())
            .spawn(move || {
                let mut r = BufReader::new(stdout);
                while let Ok(f) = read_frame(&mut r) {
                    if tx.send(f).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let mut h = Helper {
            child,
            to_helper: wtx,
            replies: rx,
            seq: 0,
            latency: 0,
            name: String::new(),
            changed: false,
            dead: None,
            late: 0,
            silent_since: None,
            budget: BLOCK_BUDGET,
        };
        match h.wait_for(&[proto::READY, proto::FAILED], START_WAIT) {
            Some((proto::READY, b)) if b.len() >= 4 => {
                h.latency = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                h.name = String::from_utf8_lossy(&b[4..]).into_owned();
            }
            Some((_, why)) => return Err(String::from_utf8_lossy(&why).into_owned()),
            None => return Err(h.dead.clone().unwrap_or_else(|| "the VST plugin didn't start in time".into())),
        }
        if let Some((comp, ctrl)) = state {
            h.set_state(comp, ctrl)?;
        }
        Ok(h)
    }

    /// How long a block may take (the default is `BLOCK_BUDGET`).
    pub fn set_budget(&mut self, d: Duration) {
        self.budget = d;
    }

    /// The next reply of one of `ops` within `wait`, noting "changed" on the way; None (and
    /// `dead` set when it's gone) when none came.
    fn wait_for(&mut self, ops: &[u8], wait: Duration) -> Option<(u8, Vec<u8>)> {
        let until = Instant::now() + wait;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.replies.recv_timeout(left) {
                Ok((proto::CHANGED, _)) => self.changed = true,
                Ok((op, b)) if ops.contains(&op) => return Some((op, b)),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => {
                    self.dead.get_or_insert_with(|| "the VST plugin stopped (it crashed)".into());
                    return None;
                }
            }
        }
    }

    /// Hands a frame to the writer thread, waiting at most `wait` for room. False: it wasn't
    /// sent (no room in time, or the helper's gone).
    fn send(&mut self, op: u8, payload: &[u8], wait: Duration) -> bool {
        if self.dead.is_some() {
            return false;
        }
        let until = Instant::now() + wait;
        let mut frame = (op, payload.to_vec());
        loop {
            match self.to_helper.try_send(frame) {
                Ok(()) => return true,
                Err(TrySendError::Full(f)) if Instant::now() < until => {
                    frame = f;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Full(_)) => return false,
                Err(TrySendError::Disconnected(_)) => {
                    self.dead = Some("the VST plugin stopped (it crashed)".into());
                    return false;
                }
            }
        }
    }

    /// Notes a block it said nothing about; ends it once it's been silent for `HUNG_AFTER`.
    fn note_silent(&mut self) {
        self.late += 1;
        let since = *self.silent_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= HUNG_AFTER && self.dead.is_none() {
            self.dead = Some("the VST plugin stopped answering, so it was closed".into());
            let _ = self.child.kill();
        }
    }

    /// One block through the plugin, in place. False: it was left as it was (late, failed or
    /// gone). Silent for `HUNG_AFTER` (not even a late answer): taken as hung and ended.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) -> bool {
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        if !self.send(proto::PROCESS, &proto::block_bytes(seq, left, right), Duration::ZERO) {
            // Its queue is full (it isn't reading): this block stays dry.
            if self.dead.is_none() {
                self.note_silent();
            }
            return false;
        }
        let until = Instant::now() + self.budget;
        loop {
            let left_time = until.saturating_duration_since(Instant::now());
            match self.wait_for(&[proto::PROCESS, proto::FAILED], left_time) {
                Some((proto::PROCESS, b)) => match proto::block_of(&b) {
                    Some((s, l, r)) if s == seq && l.len() == left.len() && r.len() == right.len() => {
                        self.silent_since = None;
                        // Nothing a plugin sends back reaches the mix unless it's a number.
                        if l.iter().chain(&r).all(|v| v.is_finite()) {
                            left.copy_from_slice(&l);
                            right.copy_from_slice(&r);
                            return true;
                        }
                        return false;
                    }
                    // An older block that came late: it's alive, just slow; waited past.
                    Some(_) => self.silent_since = None,
                    None => return false,
                },
                Some(_) => return false,
                None => {
                    self.note_silent();
                    return false;
                }
            }
        }
    }

    /// Its state now: the component's and the controller's.
    pub fn state(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        if !self.send(proto::GET_STATE, &[], ASK_WAIT) {
            return None;
        }
        let (_, b) =
            self.wait_for(&[proto::GET_STATE, proto::FAILED], ASK_WAIT).filter(|(op, _)| *op == proto::GET_STATE)?;
        self.changed = false;
        proto::state_of(&b)
    }

    pub fn set_state(&mut self, comp: &[u8], ctrl: &[u8]) -> Result<(), String> {
        if !self.send(proto::SET_STATE, &proto::state_bytes(comp, ctrl), ASK_WAIT) {
            return Err(self.dead.clone().unwrap_or_default());
        }
        match self.wait_for(&[proto::OK, proto::FAILED], ASK_WAIT) {
            Some((proto::OK, _)) => Ok(()),
            Some((_, why)) => Err(String::from_utf8_lossy(&why).into_owned()),
            None => Err("the VST plugin didn't answer".into()),
        }
    }

    /// Opens (or closes) the plugin's own window.
    pub fn window(&mut self, open: bool) -> Result<(), String> {
        if !self.send(if open { proto::OPEN_WINDOW } else { proto::CLOSE_WINDOW }, &[], ASK_WAIT) {
            return Err(self.dead.clone().unwrap_or_default());
        }
        match self.wait_for(&[proto::OK, proto::FAILED], ASK_WAIT) {
            Some((proto::OK, _)) => Ok(()),
            Some((_, why)) => Err(String::from_utf8_lossy(&why).into_owned()),
            None => Err("the VST plugin didn't answer".into()),
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // Its stdin closing ends it; if it's stuck, it's ended.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
