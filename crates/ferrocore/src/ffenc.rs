//! Encoding through FFmpeg, for systems without a native encoder of ours yet (Windows, Linux)
//! and as a software backup anywhere: raw frames or sound go into an FFmpeg process, FLV comes
//! back out (our own stream format), and its tags become our packets. FFmpeg reaches the GPUs'
//! encoders (NVENC, Quick Sync, AMF, Media Foundation, VideoToolbox) and x264 behind them; the
//! first that works here is used. Ferrocast already ships FFmpeg for Go live.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};

use crate::AvError;
use crate::audio::{AAC_FRAME, AudioConfig};
use crate::video::{Frame, PixelFormat, RateControl, VideoCodec, VideoConfig, VideoPacket};

static FFMPEG: Mutex<Option<PathBuf>> = Mutex::new(None);
static ENCODER: OnceLock<Option<String>> = OnceLock::new();

/// Where FFmpeg is (the app passes the one it ships); else `ffmpeg` on the PATH.
pub fn set_ffmpeg(path: &Path) {
    if let Ok(mut p) = FFMPEG.lock() {
        *p = Some(path.to_path_buf());
    }
}

fn ffmpeg() -> Option<PathBuf> {
    ffmpeg_path()
}

/// Maximum bytes kept from an FFmpeg encoder's stderr across a multi-hour stream.
const MAX_STDERR_BYTES: usize = 8 * 1024;

/// Drains `r` into a bounded [`MAX_STDERR_BYTES`] ring buffer and returns the last 400 chars
/// so verbose encoder warnings never accumulate without bound over a long stream.
fn drain_stderr<R: Read>(mut r: R) -> String {
    let mut ring = VecDeque::with_capacity(MAX_STDERR_BYTES);
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let slice = if n > MAX_STDERR_BYTES {
                    ring.clear();
                    &buf[n - MAX_STDERR_BYTES..n]
                } else {
                    let drop = (ring.len() + n).saturating_sub(MAX_STDERR_BYTES);
                    if drop > 0 {
                        ring.drain(..drop);
                    }
                    &buf[..n]
                };
                ring.extend(slice.iter().copied());
            }
        }
    }
    let bytes: Vec<u8> = ring.into_iter().collect();
    let s = String::from_utf8_lossy(&bytes);
    s.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect()
}

/// FFmpeg's path, as set or found on the PATH.
pub(crate) fn ffmpeg_path() -> Option<PathBuf> {
    if let Some(p) = FFMPEG.lock().ok().and_then(|p| p.clone()) {
        return Some(p);
    }
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join(exe)).find(|p| p.is_file()))
}

/// Whether FFmpeg is here (x264, and encoding off the Mac, need it).
pub fn available() -> bool {
    ffmpeg().is_some()
}

/// `cmd` without a console window of its own. Windows gives every console program a window
/// when the app that starts it has none (an installed Ferrocast): recording opened two empty
/// terminals, and closing one ended FFmpeg and the recording with it. Elsewhere: as it is.
pub fn hidden(cmd: Command) -> Command {
    #[cfg(windows)]
    let cmd = {
        use std::os::windows::process::CommandExt;
        let mut cmd = cmd;
        // CREATE_NO_WINDOW
        cmd.creation_flags(0x0800_0000);
        cmd
    };
    cmd
}

/// Where FFmpeg is (the app's, else the one on the PATH), for other jobs (remuxing).
pub fn path() -> Option<PathBuf> {
    ffmpeg()
}

/// Platform-ordered H.264 FFmpeg candidates, hardware first then `libx264` (C4 single source of truth).
pub fn candidates_for_platform(is_windows: bool, is_macos: bool) -> &'static [&'static str] {
    if is_windows {
        &["h264_nvenc", "h264_qsv", "h264_amf", "h264_mf", "libx264"]
    } else if is_macos {
        &["h264_videotoolbox", "libx264"]
    } else {
        &["h264_nvenc", "h264_qsv", "h264_amf", "libx264"]
    }
}

/// The H.264 encoders tried, best first, on this system.
fn candidates() -> &'static [&'static str] {
    candidates_for_platform(cfg!(windows), cfg!(target_os = "macos"))
}

/// Picks the first encoder in `list` that passes `probe` (shared selection logic for live and bench).
pub fn select_h264_from<'a>(list: &[&'a str], mut probe: impl FnMut(&str) -> bool) -> Option<&'a str> {
    list.iter().copied().find(|&e| probe(e))
}

/// What an encoder needs to be the one we mean: Media Foundation's only as a GPU's (its software
/// one keeps no keyframe interval or steady rate, and x264 beats it).
fn must_have(enc: &str) -> &'static [&'static str] {
    match enc {
        "h264_mf" => &["-hw_encoding", "1"],
        _ => &[],
    }
}

fn probe_with(ff: &Path, e: &str) -> bool {
    hidden(Command::new(ff))
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=black:s=256x144:r=30"])
        .args(["-frames:v", "3", "-c:v", e])
        .args(must_have(e))
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Lists every H.264 encoder FFmpeg can open here (hardware first, then `libx264`).
pub fn probe_encoders() -> Vec<String> {
    probe_encoders_cancellable(None)
}

/// Lists available H.264 FFmpeg encoders while checking an optional cancellation flag between probes.
pub fn probe_encoders_cancellable(cancel: Option<&AtomicBool>) -> Vec<String> {
    let Some(ff) = ffmpeg() else { return Vec::new() };
    let mut found = Vec::new();
    for &enc in candidates() {
        if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
            break;
        }
        if probe_with(&ff, enc) {
            found.push(enc.to_string());
        }
    }
    found
}

/// The first H.264 encoder that encodes a few frames here (asked once), using the same
/// platform candidate list as `candidates()`.
pub fn h264_encoder() -> Option<String> {
    // `FERROCAST_AV_FFMPEG_ENCODER=libx264` picks one (to test it, or to work round a driver).
    if let Ok(e) = std::env::var("FERROCAST_AV_FFMPEG_ENCODER")
        && !e.is_empty()
    {
        return Some(e);
    }
    ENCODER
        .get_or_init(|| {
            let ff = ffmpeg()?;
            select_h264_from(candidates(), |e| probe_with(&ff, e)).map(String::from)
        })
        .clone()
}

/// Returns the live FFmpeg hardware H.264 encoder that `VideoEncoder::new` will actually open
/// off-macOS (`None` on macOS where native VideoToolbox is used, or when only `libx264` works)
/// and whether `libx264` software encoding works through FFmpeg (C4).
pub fn discover_live_h264_and_x264(cancel: Option<&AtomicBool>) -> (Option<String>, bool) {
    if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
        return (None, false);
    }
    if cfg!(target_os = "macos") {
        let has_x264 =
            !cancel.is_some_and(|c| c.load(Ordering::Acquire)) && ffmpeg().is_some_and(|ff| probe_with(&ff, "libx264"));
        return (None, has_x264);
    }
    let active = h264_encoder();
    if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
        return (None, false);
    }
    let live_hw = active.as_deref().filter(|&e| e != "libx264").map(String::from);
    let has_x264 = if active.as_deref() == Some("libx264") {
        true
    } else if active.is_none() {
        // `h264_encoder()` already probed `libx264` at the tail of `candidates()`.
        false
    } else {
        !cancel.is_some_and(|c| c.load(Ordering::Acquire)) && ffmpeg().is_some_and(|ff| probe_with(&ff, "libx264"))
    };
    (live_hw, has_x264)
}

/// One FLV tag: kind (8 audio, 9 video), time in ms, body.
type Tag = (u8, u32, Vec<u8>);

/// What may wait for FFmpeg to read it, at most (about a second of 1080p frames).
const QUEUED_BYTES: usize = 96 * 1024 * 1024;

/// A running FFmpeg: what goes in, the tags coming out, and its last words.
///
/// Input goes through a thread of its own, a few frames queued: FFmpeg reads its pipe only as
/// fast as it encodes, and stops reading for the half second it takes to set up a GPU's
/// encoder after the first frame, so writing from the frame loop stalled the loop (and
/// skipped frames) at the start of every recording. A queue that stays full still holds the
/// loop back, as before, so a slow encoder never piles frames up without end.
struct Proc {
    child: Child,
    stdin: Option<SyncSender<Vec<u8>>>,
    writer: Option<std::thread::JoinHandle<()>>,
    /// Frames currently queued for FFmpeg's input thread, and queue capacity.
    queued: Arc<AtomicUsize>,
    queue_cap: usize,
    /// The pipe broke (FFmpeg stopped).
    broken: Arc<AtomicBool>,
    tags: Receiver<Tag>,
    stderr: Arc<Mutex<String>>,
}

impl Proc {
    /// `chunk`: about how many bytes each write is, to size the queue.
    fn spawn(args: &[String], chunk: usize) -> Result<Self, AvError> {
        let cap = (QUEUED_BYTES / chunk.max(1)).clamp(2, 64);
        Self::spawn_bounded(args, cap)
    }

    fn spawn_bounded(args: &[String], queue_cap: usize) -> Result<Self, AvError> {
        let ff =
            ffmpeg().ok_or_else(|| AvError::Encoder("FFmpeg isn't here, so this system can't encode yet".into()))?;
        let mut cmd = Command::new(ff);
        cmd.args(["-hide_banner", "-loglevel", "error"]).args(args);
        Self::start_with_cap(cmd, queue_cap.clamp(1, 64))
    }

    /// Starts `cmd` as FFmpeg is started: raw frames in, FLV tags out.
    #[cfg(all(test, unix))]
    fn start(cmd: Command, chunk: usize) -> Result<Self, AvError> {
        let cap = (QUEUED_BYTES / chunk.max(1)).clamp(2, 64);
        Self::start_with_cap(cmd, cap)
    }

    fn start_with_cap(cmd: Command, queue_cap: usize) -> Result<Self, AvError> {
        let mut child = hidden(cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AvError::Encoder(format!("FFmpeg didn't start ({e})")))?;
        tracing::info!(pid = child.id(), "FFmpeg started");
        let mut pipe = child.stdin.take().ok_or_else(|| AvError::Encoder("FFmpeg took no input".into()))?;
        let (stdin, queued_rx) = sync_channel::<Vec<u8>>(queue_cap);
        let queued = Arc::new(AtomicUsize::new(0));
        let broken = Arc::new(AtomicBool::new(false));
        let writer = {
            let broken = broken.clone();
            let queued = queued.clone();
            std::thread::Builder::new()
                .name("ferrocast-ffmpeg-in".into())
                .spawn(move || {
                    for bytes in queued_rx {
                        queued.fetch_sub(1, Ordering::AcqRel);
                        if pipe.write_all(&bytes).is_err() {
                            broken.store(true, Ordering::Release);
                            return;
                        }
                    }
                    // The queue closed: so does FFmpeg's input (end of stream).
                })
                .map_err(|e| AvError::Encoder(format!("FFmpeg's input thread didn't start ({e})")))?
        };
        let mut out = child.stdout.take().ok_or_else(|| AvError::Encoder("FFmpeg gave no output".into()))?;
        let (tx, tags) = channel();
        std::thread::spawn(move || {
            let mut head = [0u8; 13];
            if out.read_exact(&mut head).is_err() || &head[..3] != b"FLV" {
                return;
            }
            loop {
                let mut h = [0u8; 11];
                if out.read_exact(&mut h).is_err() {
                    return;
                }
                let size = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
                let ts = u32::from_be_bytes([h[7], h[4], h[5], h[6]]);
                let mut body = vec![0u8; size];
                let mut prev = [0u8; 4];
                if out.read_exact(&mut body).is_err() || out.read_exact(&mut prev).is_err() {
                    return;
                }
                if tx.send((h[0] & 0x1f, ts, body)).is_err() {
                    return;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let keep = stderr.clone();
            let _ = std::thread::Builder::new().name("ferrocast-ffenc-err".into()).spawn(move || {
                let tail = drain_stderr(err);
                if let Ok(mut k) = keep.lock() {
                    *k = tail;
                }
            });
        }
        Ok(Self { child, stdin: Some(stdin), writer: Some(writer), queued, queue_cap, broken, tags, stderr })
    }

    fn queue_status(&self) -> (usize, usize) {
        (self.queued.load(Ordering::Acquire), self.queue_cap)
    }

    fn write(&mut self, bytes: Vec<u8>) -> Result<(), AvError> {
        let w = self.stdin.as_ref().ok_or_else(|| AvError::Encoder("the encoder was already finished".into()))?;
        if self.broken.load(Ordering::Acquire) {
            return Err(self.died());
        }
        // Increment before sending so the writer cannot decrement before this store; roll back on error.
        self.queued.fetch_add(1, Ordering::AcqRel);
        if w.send(bytes).is_err() {
            self.queued.fetch_sub(1, Ordering::AcqRel);
            return Err(self.died());
        }
        Ok(())
    }

    /// Bounded blocking write that aborts immediately and kills FFmpeg when `cancel` is signalled.
    fn write_cancellable(&mut self, mut bytes: Vec<u8>, cancel: Option<&AtomicBool>) -> Result<(), AvError> {
        let Some(flag) = cancel else {
            return self.write(bytes);
        };
        loop {
            if flag.load(Ordering::Acquire) {
                self.stdin = None;
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(AvError::Encoder("FFmpeg cancelled".into()));
            }
            let w = self.stdin.as_ref().ok_or_else(|| AvError::Encoder("the encoder was already finished".into()))?;
            if self.broken.load(Ordering::Acquire) {
                return Err(self.died());
            }
            self.queued.fetch_add(1, Ordering::AcqRel);
            match w.try_send(bytes) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(b)) => {
                    self.queued.fetch_sub(1, Ordering::AcqRel);
                    bytes = b;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.queued.fetch_sub(1, Ordering::AcqRel);
                    return Err(self.died());
                }
            }
        }
    }

    /// As [`Self::write`], but never waits: false when FFmpeg is a whole queue behind (the frame
    /// is left out). The frame loop waited here, with the engine's lock held, whenever a software
    /// encoder couldn't keep up: the preview froze, the sound wasn't mixed and every other
    /// output stalled with it.
    fn try_write(&mut self, bytes: Vec<u8>) -> Result<bool, AvError> {
        let w = self.stdin.as_ref().ok_or_else(|| AvError::Encoder("the encoder was already finished".into()))?;
        if self.broken.load(Ordering::Acquire) {
            return Err(self.died());
        }
        self.queued.fetch_add(1, Ordering::AcqRel);
        match w.try_send(bytes) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => {
                self.queued.fetch_sub(1, Ordering::AcqRel);
                Ok(false)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.queued.fetch_sub(1, Ordering::AcqRel);
                Err(self.died())
            }
        }
    }

    fn died(&mut self) -> AvError {
        let _ = self.child.wait();
        let why = self.stderr.lock().map(|s| s.trim().to_string()).unwrap_or_default();
        AvError::Encoder(if why.is_empty() {
            "FFmpeg stopped encoding".into()
        } else {
            format!("FFmpeg stopped encoding: {why}")
        })
    }

    /// Closes the input and waits for everything still coming, for [`FINISH_WITHIN`] at most:
    /// then FFmpeg is stopped and the file ends with what came (a stuck FFmpeg never holds
    /// Stop forever; GitHub's Windows runner waited over a minute, 18-19 Nov).
    fn finish(&mut self) -> Vec<Tag> {
        // One time for a whole stop (video, sound and extra tracks finish one after another):
        // what's left of it, else the usual.
        let within = FINISH_BY
            .with(std::cell::Cell::get)
            .map_or(FINISH_WITHIN, |by| by.saturating_duration_since(std::time::Instant::now()));
        self.finish_within(within)
    }

    fn finish_within(&mut self, within: std::time::Duration) -> Vec<Tag> {
        let t = std::time::Instant::now();
        let deadline = t + within;
        // The queue's sender gone: the writer writes what's left, then closes FFmpeg's input.
        self.stdin = None;
        if let Some(w) = self.writer.take() {
            while !w.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if w.is_finished() {
                let _ = w.join();
            } else {
                self.writer = Some(w);
            }
        }
        tracing::info!(at_ms = t.elapsed().as_millis() as u64, "FFmpeg's input is written and closed");
        let mut tags = Vec::new();
        let mut late = false;
        loop {
            match self.tags.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok(tag) => tags.push(tag),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    late = true;
                    break;
                }
            }
        }
        // Its output closed isn't proof it ended (CodeRabbit): it's waited for within the same
        // time.
        while !late && matches!(self.child.try_wait(), Ok(None)) {
            if std::time::Instant::now() >= deadline {
                late = true;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        if late {
            tracing::warn!(
                seconds = within.as_secs(),
                tags = tags.len(),
                "FFmpeg didn't finish in time: stopped, the file ends with what came"
            );
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        let _ = self.writer.take().map(let_go);
        tags.extend(self.tags.try_iter());
        tracing::info!(at_ms = t.elapsed().as_millis() as u64, tags = tags.len(), "FFmpeg's output is read to its end");
        tags
    }
}

/// How long stopping waits for FFmpeg to encode what's queued and end (a slow software encode
/// of a full queue takes seconds, not this long).
pub(crate) const FINISH_WITHIN: std::time::Duration = std::time::Duration::from_secs(45);

thread_local! {
    /// When the stop under way on this thread must be done by (see [`finishing_by`]).
    static FINISH_BY: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

/// When the stop under way on this thread must be done by, if one is (for work handed to another
/// thread that finishes an FFmpeg: the deadline lives here).
pub(crate) fn finish_deadline() -> Option<std::time::Instant> {
    FINISH_BY.with(std::cell::Cell::get)
}

/// Runs `f` (an output's whole finish) with every FFmpeg it finishes sharing one deadline, so
/// several FFmpegs (video, sound, extra tracks) never wait 45 s each.
pub(crate) fn finishing_by<T>(by: std::time::Instant, f: impl FnOnce() -> T) -> T {
    /// Puts the deadline before back however `f` ends, a panic included (projects-60): a
    /// thread left with a past one would give its next stop no time at all.
    struct Restore(Option<std::time::Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            FINISH_BY.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(FINISH_BY.with(|c| c.replace(Some(by))));
    f()
}

impl Drop for Proc {
    fn drop(&mut self) {
        // Killed first, so a writer stuck on a full pipe fails and ends.
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.writer.take().map(let_go);
    }
}

/// Waits for FFmpeg's input thread once FFmpeg is gone, unless it's still writing: then it's
/// let go, as its pipe may never break (another program that inherited FFmpeg's input keeps
/// it open; projects-60's hypothesis for the Windows runner's hang). Stop never waits on it.
fn let_go(w: std::thread::JoinHandle<()>) {
    if w.is_finished() {
        let _ = w.join();
    } else {
        tracing::warn!("FFmpeg's input thread was still writing when FFmpeg ended: let go");
    }
}

/// An encoder at a new bitrate, started beside the one running: it is fed the same frames, and
/// the stream changes over to it at one of its keyframes once it has shown it works.
struct Handover {
    proc: Proc,
    cfg: VideoConfig,
    /// Our times of the frames it was given and has not yet given back.
    times: VecDeque<i64>,
    header: Option<Vec<u8>>,
    /// What it made, in order, waiting for the change-over.
    ready: VecDeque<VideoPacket>,
    /// Frames it was given.
    fed: u64,
    /// When it started, and how long it may take to be ready: one that never gets there is let
    /// go (a hung encoder, a queue that never moves).
    started: std::time::Instant,
    max_wait: std::time::Duration,
}

/// How long after a bitrate step that failed the next one is tried.
const STEP_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// The longest a new encoder may take to take over before it is let go.
const HANDOVER_MAX: std::time::Duration = std::time::Duration::from_secs(20);

/// A bitrate step's encoder being started (the process starts on a thread of its own).
type Starting = (Receiver<Result<Proc, AvError>>, VideoConfig);

/// Lets go of an FFmpeg off the caller's thread: stopping one waits for the process and its
/// input thread, and the frame loop must never wait for that.
fn reap(proc: Proc) {
    let spare = std::sync::Arc::new(Mutex::new(Some(proc)));
    let moved = spare.clone();
    let started = std::thread::Builder::new().name("ferrocast-ffenc-reaper".into()).spawn(move || {
        let taken = moved.lock().ok().and_then(|mut p| p.take());
        drop(taken);
    });
    if started.is_err() {
        // No thread to spare: stopped here (killing it is quick).
        let taken = spare.lock().ok().and_then(|mut p| p.take());
        drop(taken);
    }
}

/// H.264 through FFmpeg.
pub(crate) struct Video {
    proc: Proc,
    /// Our times of the frames sent and not yet back (no B-frames: they come back in order).
    times: VecDeque<i64>,
    header: Option<Vec<u8>>,
    width: u32,
    height: u32,
    input: PixelFormat,
    pub(crate) name: String,
    /// What the running encoder was made with.
    cfg: VideoConfig,
    /// A bitrate asked for and not yet started (a failed step waits [`STEP_RETRY`]).
    wanted_kbps: Option<u32>,
    retry_after: Option<std::time::Instant>,
    /// The process for a new bitrate starting on its own thread.
    starting: Option<Starting>,
    /// An encoder at a new bitrate running beside this one (see [`Handover`]).
    next: Option<Handover>,
    /// While a change-over waits: this encoder's latest packets, held back so that what the new
    /// one takes over from is not already gone; the time of the last it let go and of the last it
    /// made.
    held: VecDeque<VideoPacket>,
    emitted_upto: Option<i64>,
    made_upto: Option<i64>,
    /// Frames left out because the encoder was behind (OBS's "skipped frames due to encoding
    /// lag").
    pub(crate) skipped: u64,
    /// When set, writes block on a tiny pipe queue so benchmarks measure true encoder throughput.
    blocking: bool,
    /// When false (stress benchmarks), skipped frames stay local and never increment `DROPPED`.
    pub(crate) record_global_drops: bool,
    /// Optional cancellation signal for blocking benchmark writes (C3).
    cancel: Option<Arc<AtomicBool>>,
    /// Timestamp (ms from `Video` creation) of the last rate-limited encoder lag warning.
    started: std::time::Instant,
    last_lag_warn_ms: Option<u64>,
}

impl std::fmt::Debug for Video {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ffenc::Video").field("encoder", &self.name).finish_non_exhaustive()
    }
}

impl Video {
    pub(crate) fn new(cfg: &VideoConfig) -> Result<Self, AvError> {
        Self::new_inner(cfg, true)
    }

    pub(crate) fn new_benchmark(cfg: &VideoConfig) -> Result<Self, AvError> {
        Self::new_inner(cfg, false)
    }

    fn new_inner(cfg: &VideoConfig, record_global_drops: bool) -> Result<Self, AvError> {
        if cfg.codec != VideoCodec::H264 {
            return Err(AvError::Encoder("HEVC through FFmpeg isn't done yet: H.264 is".into()));
        }
        let enc = if cfg.software {
            "libx264".to_string()
        } else {
            h264_encoder().ok_or_else(|| AvError::Encoder("no H.264 encoder works here through FFmpeg".into()))?
        };
        let mut video = Self::with_proc(Proc::spawn(&args(cfg, &enc), frame_bytes(cfg))?, cfg, enc, false);
        video.record_global_drops = record_global_drops;
        Ok(video)
    }

    pub(crate) fn new_named(
        cfg: &VideoConfig,
        encoder: &str,
        blocking_cap: Option<usize>,
        cancel: Option<Arc<AtomicBool>>,
        record_global_drops: bool,
    ) -> Result<Self, AvError> {
        if cfg.codec != VideoCodec::H264 {
            return Err(AvError::Encoder("HEVC through FFmpeg isn't done yet: H.264 is".into()));
        }
        let proc = match blocking_cap {
            Some(cap) => Proc::spawn_bounded(&args(cfg, encoder), cap)?,
            None => Proc::spawn(&args(cfg, encoder), frame_bytes(cfg))?,
        };
        let mut video = Self::with_proc(proc, cfg, encoder.to_string(), blocking_cap.is_some());
        video.record_global_drops = record_global_drops;
        video.cancel = cancel;
        Ok(video)
    }

    fn with_proc(proc: Proc, cfg: &VideoConfig, name: String, blocking: bool) -> Self {
        Self {
            proc,
            times: VecDeque::new(),
            header: None,
            width: cfg.width,
            height: cfg.height,
            input: cfg.input,
            name,
            cfg: *cfg,
            wanted_kbps: None,
            retry_after: None,
            starting: None,
            next: None,
            held: VecDeque::new(),
            emitted_upto: None,
            made_upto: None,
            skipped: 0,
            blocking,
            record_global_drops: true,
            cancel: None,
            started: std::time::Instant::now(),
            last_lag_warn_ms: None,
        }
    }

    /// A new bitrate: another encoder starts beside this one (on a thread of its own, so the
    /// caller never waits for the process), is given the same frames, and the stream changes over
    /// to it at one of its keyframes once it has shown that it works. If it can't start (a
    /// hardware encoder out of sessions), the stream goes on as it was: a bitrate that couldn't be
    /// changed is never an error of the stream.
    pub(crate) fn set_kbps(&mut self, kbps: u32) -> Result<(), AvError> {
        // A quality has no bitrate to change.
        if matches!(self.cfg.rate, RateControl::Crf(_) | RateControl::Quality(_)) {
            return Ok(());
        }
        let target = self
            .wanted_kbps
            .or_else(|| self.starting.as_ref().map(|s| s.1.bitrate_kbps))
            .or_else(|| self.next.as_ref().map(|h| h.cfg.bitrate_kbps))
            .unwrap_or(self.cfg.bitrate_kbps);
        if kbps == target {
            return Ok(());
        }
        self.wanted_kbps = Some(kbps);
        self.start_step();
        Ok(())
    }

    /// Starts the encoder for the bitrate asked for, unless a failed step is still being waited
    /// out.
    fn start_step(&mut self) {
        let Some(kbps) = self.wanted_kbps else { return };
        if self.retry_after.is_some_and(|t| std::time::Instant::now() < t) {
            return;
        }
        self.wanted_kbps = None;
        // An older step still under way is let go: this one is the one wanted now.
        self.starting = None;
        if let Some(h) = self.next.take() {
            reap(h.proc);
            self.release_held();
        }
        let mut cfg = self.cfg;
        cfg.bitrate_kbps = kbps;
        let (tx, rx) = channel();
        let (a, chunk) = (args(&cfg, &self.name), frame_bytes(&cfg));
        let spawned = std::thread::Builder::new().name("ferrocast-ffenc-start".into()).spawn(move || {
            let _ = tx.send(Proc::spawn(&a, chunk));
        });
        match spawned {
            Ok(_) => self.starting = Some((rx, cfg)),
            Err(e) => self.step_failed(&format!("its start couldn't be made ({e})")),
        }
    }

    /// The new encoder is running: it takes frames from the next one.
    fn begin_handover(&mut self, proc: Proc, cfg: VideoConfig) {
        self.emitted_upto = None;
        self.made_upto = None;
        self.next = Some(Handover {
            proc,
            cfg,
            times: VecDeque::new(),
            header: None,
            ready: VecDeque::new(),
            fed: 0,
            started: std::time::Instant::now(),
            max_wait: HANDOVER_MAX,
        });
    }

    /// A step that didn't work: said once, the stream stays as it was, and the next try waits.
    fn step_failed(&mut self, why: &str) {
        tracing::warn!(
            encoder = %self.name,
            kbps = self.cfg.bitrate_kbps,
            %why,
            "a bitrate step couldn't start another encoder: the stream stays as it was"
        );
        self.retry_after = Some(std::time::Instant::now() + STEP_RETRY);
    }

    /// Lets the new encoder go (it failed or timed out) and gives back what was held.
    fn abandon(&mut self, why: &str) {
        if let Some(h) = self.next.take() {
            reap(h.proc);
        }
        self.step_failed(why);
    }

    fn release_held(&mut self) {
        self.emitted_upto = None;
        self.made_upto = None;
    }
}

/// One raw frame's bytes as `cfg` sends them.
fn frame_bytes(cfg: &VideoConfig) -> usize {
    let px = cfg.width as usize * cfg.height as usize;
    if cfg.input == PixelFormat::Nv12 { px * 3 / 2 } else { px * 4 }
}

/// FFmpeg's arguments for encoder `enc` as `cfg` says.
fn args(cfg: &VideoConfig, enc: &str) -> Vec<String> {
    {
        let enc = enc.to_string();
        let kbps = cfg.bitrate_kbps;
        let gop = (u64::from(cfg.gop_secs()) * u64::from(cfg.fps_num) / u64::from(cfg.fps_den.max(1))).max(1);
        let mut a: Vec<String> = vec![
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            (if cfg.input == PixelFormat::Nv12 { "nv12" } else { "bgra" }).into(),
            "-video_size".into(),
            format!("{}x{}", cfg.width, cfg.height),
            "-framerate".into(),
            format!("{}/{}", cfg.fps_num, cfg.fps_den.max(1)),
        ];
        // Our NV12 is BT.709, limited range: said so, or FFmpeg takes untagged frames for BT.601
        // and turns every colour into BT.709 on the way to the encoder.
        if cfg.input == PixelFormat::Nv12 {
            a.extend(
                ["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]
                    .map(String::from),
            );
        }
        a.extend([
            "-i".into(),
            "pipe:0".into(),
            "-c:v".into(),
            enc.clone(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-g".into(),
            gop.to_string(),
            "-bf".into(),
            "0".into(),
            // One frame out for each in, in order: our own times are matched back by order.
            "-fps_mode".into(),
            "passthrough".into(),
            "-colorspace".into(),
            "bt709".into(),
            "-color_primaries".into(),
            "bt709".into(),
            "-color_trc".into(),
            "bt709".into(),
        ]);
        // x264's lossless is its own profile (High 4:4:4 Predictive): naming one would refuse it.
        if !matches!(cfg.rate, RateControl::Crf(0)) {
            a.extend([
                "-profile:v".into(),
                match (cfg.codec, cfg.profile) {
                    (VideoCodec::Hevc, _) => "main",
                    (_, crate::video::H264Profile::Baseline) => "baseline",
                    (_, crate::video::H264Profile::Main) => "main",
                    (_, crate::video::H264Profile::High) => "high",
                }
                .into(),
            ]);
        }
        let cbr = matches!(cfg.rate, RateControl::Cbr | RateControl::CappedCbr);
        // Constant quality on libobs's scale, 0 (best) … 51, from the 0-1 the engine carries.
        let cq = match cfg.rate {
            RateControl::Quality(f) => Some((((1.0 - f.clamp(0.0, 1.0)) * 51.0).round() as u32).clamp(1, 51)),
            _ => None,
        };
        match (cfg.rate, cq) {
            // A quality, not a bitrate: x264's rate factor (0 is lossless).
            (RateControl::Crf(q), _) => a.extend(["-crf".into(), q.min(51).to_string()]),
            // Each encoder's own quality option, below (VideoToolbox keeps its bitrate here).
            (_, Some(_)) if enc != "h264_videotoolbox" => {}
            _ => a.extend(["-b:v".into(), format!("{kbps}k")]),
        }
        // Strict 1x VBV buffer matching OBS (rtmp-common.c: 2x VBV allowed keyframes/cuts to burst
        // past platform limits); VideoToolbox's FFmpeg CBR omits -maxrate unless capped strictly.
        if cbr && (enc != "h264_videotoolbox" || matches!(cfg.rate, RateControl::CappedCbr)) {
            a.extend(["-maxrate".into(), format!("{kbps}k"), "-bufsize".into(), format!("{kbps}k")]);
        }
        // Constant quality as OBS sets it on each encoder ("high" recordings: 16). Before, the
        // Windows engine refused a recording at that quality (10 Nov, the owner's PC).
        if let Some(q) = cq {
            let q = q.to_string();
            let quality: Vec<String> = match enc.as_str() {
                "libx264" => vec![
                    "-crf".into(),
                    q,
                    "-preset".into(),
                    cfg.preset
                        .and_then(|i| crate::video::X264_PRESETS.get(usize::from(i)).copied())
                        .unwrap_or("veryfast")
                        .into(),
                ],
                "h264_nvenc" => ["-preset", "p4", "-rc", "vbr", "-cq", &q, "-b:v", "0"].map(String::from).to_vec(),
                "h264_qsv" => {
                    ["-preset", "veryfast", "-look_ahead", "0", "-global_quality", &q].map(String::from).to_vec()
                }
                "h264_amf" => ["-usage", "transcoding", "-rc", "cqp", "-qp_i", &q, "-qp_p", &q, "-qp_b", &q]
                    .map(String::from)
                    .to_vec(),
                "h264_mf" => {
                    let pct = (100 - q.parse::<u32>().unwrap_or(16) * 100 / 51).to_string();
                    ["-rate_control", "quality", "-quality", &pct, "-scenario", "archive"].map(String::from).to_vec()
                }
                _ => Vec::new(),
            };
            if !quality.is_empty() {
                a.extend(quality);
                a.extend(must_have(&enc).iter().map(|s| s.to_string()));
                a.extend(["-f".into(), "flv".into(), "pipe:1".into()]);
                return a;
            }
        }
        let extra: &[&str] = match enc.as_str() {
            "libx264" if cbr => {
                &["-preset", "veryfast", "-tune", "zerolatency", "-x264-params", "nal-hrd=cbr:force-cfr=1"]
            }
            // Lossless as libobs's x264 does it: fast enough to keep up, big files.
            "libx264" if cfg.rate == RateControl::Crf(0) => &["-preset", "ultrafast"],
            "libx264" if matches!(cfg.rate, RateControl::Crf(_)) => &["-preset", "veryfast"],
            "libx264" => &["-preset", "veryfast", "-tune", "zerolatency"],
            "h264_nvenc" if cbr => &["-preset", "p4", "-tune", "ll", "-rc", "cbr", "-zerolatency", "1"],
            "h264_nvenc" => &["-preset", "p4", "-tune", "ll", "-zerolatency", "1"],
            "h264_qsv" => &["-preset", "veryfast", "-look_ahead", "0"],
            "h264_amf" if cbr => &["-usage", "lowlatency", "-rc", "cbr"],
            "h264_amf" => &["-usage", "lowlatency"],
            "h264_mf" if cbr => &["-rate_control", "cbr", "-scenario", "live_streaming"],
            "h264_mf" => &["-scenario", "live_streaming"],
            "h264_videotoolbox" if cbr => &["-realtime", "1", "-constant_bit_rate", "1"],
            "h264_videotoolbox" => &["-realtime", "1"],
            _ => &[],
        };
        let mut extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        if enc == "libx264" {
            // The settings' own preset and tune replace the defaults above.
            let preset = cfg.preset.and_then(|i| crate::video::X264_PRESETS.get(usize::from(i)).copied());
            let tune = cfg.tune.and_then(|i| crate::video::X264_TUNES.get(usize::from(i)).copied());
            for (flag, pick) in [("-preset", preset), ("-tune", tune)] {
                if let Some(v) = pick {
                    if let Some(at) = extra.iter().position(|x| x == flag) {
                        extra[at + 1] = v.to_string();
                    } else {
                        extra.extend([flag.to_string(), v.to_string()]);
                    }
                }
            }
        }
        a.extend(extra);
        a.extend(must_have(&enc).iter().map(|s| s.to_string()));
        a.extend(["-f".into(), "flv".into(), "pipe:1".into()]);
        a
    }
}

/// An avcC record's parameter sets (SPS, PPS) as length-prefixed NAL units, as they'd sit in
/// front of a frame (4-byte lengths, as FFmpeg's FLV writes).
fn inline_params(avcc: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 5;
    for _ in 0..2 {
        // SPS count (low 5 bits), then PPS count.
        let n = usize::from(*avcc.get(at)? & if out.is_empty() { 0x1f } else { 0xff });
        at += 1;
        for _ in 0..n {
            let len = usize::from(u16::from_be_bytes([*avcc.get(at)?, *avcc.get(at + 1)?]));
            let nal = avcc.get(at + 2..at + 2 + len)?;
            out.extend_from_slice(&(len as u32).to_be_bytes());
            out.extend_from_slice(nal);
            at += 2 + len;
        }
    }
    (!out.is_empty()).then_some(out)
}

impl Video {
    pub(crate) fn header(&self) -> Option<Vec<u8>> {
        self.header.clone()
    }

    pub(crate) fn encode(&mut self, frame: Frame<'_>, pts_us: i64) -> Result<Vec<VideoPacket>, AvError> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut raw = Vec::with_capacity(if self.input == PixelFormat::Nv12 { w * h * 3 / 2 } else { w * h * 4 });
        match frame {
            Frame::Nv12 { y, y_stride, uv, uv_stride } => {
                for r in 0..h {
                    raw.extend_from_slice(&y[r * y_stride..r * y_stride + w]);
                }
                for r in 0..h / 2 {
                    raw.extend_from_slice(&uv[r * uv_stride..r * uv_stride + w]);
                }
            }
            Frame::Bgra { data, stride } => {
                for r in 0..h {
                    raw.extend_from_slice(&data[r * stride..r * stride + w * 4]);
                }
            }
            // A pooled surface (Mac): read where it sits, packed into FFmpeg's frame as above.
            #[cfg(target_os = "macos")]
            Frame::Surface(px) => {
                crate::video::note_materialised_bytes((w * h * 3 / 2) as u64);
                px.with_planes(|y, y_stride, uv, uv_stride| {
                    for r in 0..h {
                        raw.extend_from_slice(&y[r * y_stride..r * y_stride + w]);
                    }
                    for r in 0..h / 2 {
                        raw.extend_from_slice(&uv[r * uv_stride..r * uv_stride + w]);
                    }
                })
                .ok_or_else(|| AvError::Encoder("a frame couldn't be read".into()))?;
            }
        }
        self.encode_raw(raw, pts_us)
    }

    /// The frames' own rate, for how long a change-over may take and how much is held back.
    fn fps(&self) -> u64 {
        (u64::from(self.cfg.fps_num) / u64::from(self.cfg.fps_den.max(1))).max(1)
    }

    fn encode_raw(&mut self, raw: Vec<u8>, pts_us: i64) -> Result<Vec<VideoPacket>, AvError> {
        // A bitrate step waiting out a failure is tried again; one whose process has started
        // begins its change-over.
        if self.starting.is_none() && self.next.is_none() {
            self.start_step();
        }
        if let Some((rx, cfg)) = self.starting.take() {
            match rx.try_recv() {
                Ok(Ok(proc)) => self.begin_handover(proc, cfg),
                Ok(Err(e)) => self.step_failed(&e.to_string()),
                Err(std::sync::mpsc::TryRecvError::Empty) => self.starting = Some((rx, cfg)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.step_failed("its start was lost"),
            }
        }
        // The new encoder is given the frame too: one that can't take it, or has stopped, is let
        // go and the stream goes on as it was.
        let mut failed = None;
        if let Some(h) = self.next.as_mut() {
            match h.proc.try_write(raw.clone()) {
                Ok(true) => {
                    h.times.push_back(pts_us);
                    h.fed += 1;
                }
                Ok(false) => {}
                Err(e) => failed = Some(e.to_string()),
            }
            if failed.is_none() && matches!(h.proc.child.try_wait(), Ok(Some(_))) {
                failed = Some(h.proc.died().to_string());
            }
        }
        if let Some(why) = failed {
            self.abandon(&why);
        }
        // Each packet gets its time from `times` in order, so a frame left out is just missing:
        // nothing after it moves.
        let mut dropped = false;
        if self.blocking {
            self.proc.write_cancellable(raw, self.cancel.as_deref())?;
            self.times.push_back(pts_us);
        } else if self.proc.try_write(raw)? {
            self.times.push_back(pts_us);
        } else {
            dropped = true;
            self.skipped += 1;
            if self.record_global_drops {
                crate::video::note_dropped(1);
            }
        }
        let (depth, cap) = self.proc.queue_status();
        let now_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if should_log_lag(now_ms, self.last_lag_warn_ms, dropped, depth, cap) {
            self.last_lag_warn_ms = Some(now_ms);
            tracing::warn!(
                encoder = %self.name,
                queue_depth = depth,
                queue_cap = cap,
                skipped = self.skipped,
                bitrate_kbps = self.cfg.bitrate_kbps,
                "output encoder falling behind: frames queued or left out"
            );
        }
        let made = packets(&mut self.times, &mut self.header, self.proc.tags.try_iter().collect());
        if self.next.is_none() {
            // Nothing is changing over (or it was let go just now): what was held goes first.
            let mut out: Vec<VideoPacket> = self.held.drain(..).collect();
            out.extend(made);
            self.release_held();
            return Ok(out);
        }
        self.change_over(made)
    }

    /// While a new encoder runs beside this one: this one's packets go out `hold` behind, the new
    /// one's wait, and at the first keyframe of the new one that this one has reached and not yet
    /// let go, the stream changes over (nothing twice, nothing lost: this one's packets before it,
    /// the new one's from it).
    fn change_over(&mut self, made: Vec<VideoPacket>) -> Result<Vec<VideoPacket>, AvError> {
        let hold = (self.fps() as usize / 2).max(6);
        let limit = (self.fps() * 10).max(100);
        if let Some(last) = made.last() {
            self.made_upto = Some(last.pts_us);
        }
        self.held.extend(made);
        let mut out = Vec::new();
        let mut joined: Option<Handover> = None;
        let mut gave_up = None;
        if let Some(h) = self.next.as_mut() {
            let new = packets(&mut h.times, &mut h.header, h.proc.tags.try_iter().collect());
            h.ready.extend(new);
            // Packets before a keyframe it can still be joined at are of no use.
            while let Some(front) = h.ready.front() {
                if front.key && self.emitted_upto.is_none_or(|e| front.pts_us > e) {
                    break;
                }
                h.ready.pop_front();
            }
            let reached = h.ready.front().is_some_and(|k| self.made_upto.is_some_and(|m| m >= k.pts_us));
            if reached && h.header.is_some() {
                joined = self.next.take();
            } else if h.fed > limit || h.started.elapsed() > h.max_wait {
                gave_up = Some("it didn't take over in time");
            }
        }
        if let Some(mut h) = joined {
            let key = h.ready.front().map_or(i64::MAX, |k| k.pts_us);
            // This one's packets before the change, then it is let go (its later frames are the
            // new one's).
            while self.held.front().is_some_and(|p| p.pts_us < key) {
                out.extend(self.held.pop_front());
            }
            self.held.clear();
            // The new one's parameter sets go in front of its keyframe when they differ.
            if let (Some(old), Some(new)) = (self.header.as_ref(), h.header.as_ref())
                && old != new
                && let Some(params) = inline_params(new)
                && let Some(k) = h.ready.front_mut()
            {
                k.data.splice(0..0, params);
            }
            out.extend(h.ready.drain(..));
            let old = std::mem::replace(&mut self.proc, h.proc);
            reap(old);
            self.times = std::mem::take(&mut h.times);
            self.header = h.header.take().or(self.header.take());
            self.cfg = h.cfg;
            self.release_held();
            self.retry_after = None;
            return Ok(out);
        }
        if let Some(why) = gave_up {
            self.abandon(why);
        }
        // Held back, but only so far.
        let keep = if self.next.is_some() { hold } else { 0 };
        while self.held.len() > keep {
            if let Some(p) = self.held.pop_front() {
                self.emitted_upto = Some(p.pts_us);
                out.push(p);
            }
        }
        Ok(out)
    }

    pub(crate) fn flush(&mut self) -> Result<Vec<VideoPacket>, AvError> {
        let mut out: Vec<VideoPacket> = self.held.drain(..).collect();
        if let Some(h) = self.next.take() {
            reap(h.proc);
        }
        self.starting = None;
        let tags = self.proc.finish();
        out.extend(packets(&mut self.times, &mut self.header, tags));
        Ok(out)
    }
}

/// FLV video tags → packets: the codec header kept, frames given our own times.
fn packets(times: &mut VecDeque<i64>, header: &mut Option<Vec<u8>>, tags: Vec<Tag>) -> Vec<VideoPacket> {
    let mut out = Vec::new();
    for (kind, _, body) in tags {
        if kind != 9 || body.len() < 5 || body[0] & 0x0f != 7 {
            continue;
        }
        match body[1] {
            0 => *header = Some(body[5..].to_vec()),
            1 => {
                let Some(pts) = times.pop_front() else { continue };
                let key = body[0] >> 4 == 1;
                out.push(VideoPacket { pts_us: pts, dts_us: pts, key, data: body[5..].to_vec() });
            }
            _ => {}
        }
    }
    out
}

/// AAC through FFmpeg (its own AAC encoder, which every FFmpeg has).
pub(crate) struct Audio {
    proc: Proc,
    channels: usize,
}

impl std::fmt::Debug for Audio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ffenc::Audio").finish_non_exhaustive()
    }
}

/// FFmpeg's AAC encoder starts this many samples early (its `initial_padding`).
pub(crate) const PRIMING: u32 = 1024;

impl Audio {
    pub(crate) fn new(cfg: &AudioConfig) -> Result<Self, AvError> {
        let a: Vec<String> = [
            "-f",
            "f32le",
            "-ar",
            &cfg.sample_rate.to_string(),
            "-ac",
            &cfg.channels.to_string(),
            "-i",
            "pipe:0",
            "-c:a",
            "aac",
            "-b:a",
            &format!("{}k", cfg.bitrate_kbps),
            "-f",
            "flv",
            "pipe:1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        Ok(Self { proc: Proc::spawn(&a, AAC_FRAME * cfg.channels as usize * 4)?, channels: cfg.channels as usize })
    }

    /// The input ends: every frame still inside comes out.
    pub(crate) fn flush(&mut self) -> Vec<Vec<u8>> {
        self.proc
            .finish()
            .into_iter()
            .filter_map(|(k, _, b)| (k == 8 && b.len() > 2 && b[1] == 1).then(|| b[2..].to_vec()))
            .collect()
    }

    /// One AAC frame's samples in → the frames now out (raw AAC, in order).
    pub(crate) fn encode(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>, AvError> {
        debug_assert_eq!(pcm.len(), AAC_FRAME * self.channels);
        let bytes: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.proc.write(bytes)?;
        Ok(self
            .proc
            .tags
            .try_iter()
            .filter_map(|(k, _, b)| (k == 8 && b.len() > 2 && b[1] == 1).then(|| b[2..].to_vec()))
            .collect())
    }
}

const LAG_WARN_EVERY_MS: u64 = 5_000;

fn should_log_lag(now_ms: u64, prev_ms: Option<u64>, dropped: bool, depth: usize, cap: usize) -> bool {
    let behind = dropped || (cap > 0 && depth >= cap);
    behind && prev_ms.is_none_or(|p| now_ms.saturating_sub(p) >= LAG_WARN_EVERY_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An FFmpeg that never ends (stuck, or its input held open elsewhere: GitHub's Windows
    /// runner, 18-19 Nov) is stopped at the deadline: Stop never hangs.
    #[cfg(unix)]
    #[test]
    fn a_stuck_ffmpeg_never_holds_stop() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec sleep 30"]);
        let mut p = Proc::start(cmd, 1024).unwrap_or_else(|e| panic!("{e}"));
        let t = std::time::Instant::now();
        let tags = p.finish_within(std::time::Duration::from_secs(1));
        assert!(tags.is_empty());
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
    }

    /// One that never reads its input, with more queued than a pipe holds: its input thread is
    /// still writing at the deadline, and Stop still returns.
    #[cfg(unix)]
    #[test]
    fn a_full_input_never_holds_stop() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec sleep 30"]);
        let mut p = Proc::start(cmd, 256 * 1024).unwrap_or_else(|e| panic!("{e}"));
        for _ in 0..2 {
            p.write(vec![0u8; 256 * 1024]).unwrap_or_else(|e| panic!("{e}"));
        }
        let t = std::time::Instant::now();
        let _ = p.finish_within(std::time::Duration::from_secs(1));
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
    }

    /// A stop that panics leaves no deadline behind on its thread.
    #[test]
    fn a_panicking_stop_leaves_no_deadline_behind() {
        let by = std::time::Instant::now();
        let r = std::panic::catch_unwind(|| finishing_by(by, || panic!("a stop that fails")));
        assert!(r.is_err());
        assert_eq!(FINISH_BY.with(std::cell::Cell::get), None);
    }

    /// Several FFmpegs finished in one stop share its time.
    #[cfg(unix)]
    #[test]
    fn one_stop_shares_one_deadline() {
        let stuck = || {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "exec sleep 30"]);
            Proc::start(cmd, 1024).unwrap_or_else(|e| panic!("{e}"))
        };
        let (mut a, mut b, mut c) = (stuck(), stuck(), stuck());
        let t = std::time::Instant::now();
        finishing_by(t + std::time::Duration::from_secs(1), || {
            a.finish();
            b.finish();
            c.finish();
        });
        assert!(t.elapsed() < std::time::Duration::from_secs(3), "{:?}", t.elapsed());
    }

    /// The same when it closes its output but goes on running.
    #[cfg(unix)]
    #[test]
    fn an_ffmpeg_that_closes_its_output_but_runs_on_is_stopped_too() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec 1>&-; exec sleep 30"]);
        let mut p = Proc::start(cmd, 1024).unwrap_or_else(|e| panic!("{e}"));
        let t = std::time::Instant::now();
        let _ = p.finish_within(std::time::Duration::from_secs(1));
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
    }

    fn cfg(rate: RateControl) -> VideoConfig {
        VideoConfig {
            codec: VideoCodec::H264,
            input: PixelFormat::Nv12,
            width: 1920,
            height: 1080,
            fps_num: 60,
            fps_den: 1,
            bitrate_kbps: 6000,
            rate,
            keyframe_secs: 2,
            software: false,
            profile: crate::video::H264Profile::High,
            bframes: false,
            preset: None,
            tune: None,
        }
    }

    fn has(a: &[String], pair: [&str; 2]) -> bool {
        a.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1])
    }

    /// The picked x264 preset and tune replace the defaults (a recording's "Your own").
    #[test]
    fn a_picked_x264_preset_and_tune_replace_the_defaults() {
        let mut c = cfg(RateControl::Crf(20));
        c.preset = crate::video::x264_index(&crate::video::X264_PRESETS, "slow");
        c.tune = crate::video::x264_index(&crate::video::X264_TUNES, "film");
        let a = args(&c, "libx264");
        assert!(has(&a, ["-preset", "slow"]) && has(&a, ["-tune", "film"]), "{a:?}");
        assert!(!has(&a, ["-preset", "veryfast"]), "{a:?}");
        let d = args(&cfg(RateControl::Crf(20)), "libx264");
        assert!(has(&d, ["-preset", "veryfast"]) && !d.iter().any(|x| x == "-tune"), "{d:?}");
    }

    /// "High" recordings (libobs's 16) on each encoder's own quality option, with no bitrate.
    #[test]
    fn constant_quality_is_each_encoders_own() {
        let high = cfg(RateControl::Quality(0.69));
        let nv = args(&high, "h264_nvenc");
        assert!(has(&nv, ["-rc", "vbr"]) && has(&nv, ["-cq", "16"]) && has(&nv, ["-b:v", "0"]), "{nv:?}");
        assert!(!has(&nv, ["-b:v", "6000k"]) && !nv.iter().any(|s| s == "-maxrate"));
        assert!(has(&args(&high, "h264_qsv"), ["-global_quality", "16"]));
        assert!(has(&args(&high, "h264_amf"), ["-rc", "cqp"]));
        assert!(has(&args(&high, "h264_mf"), ["-rate_control", "quality"]));
        assert!(has(&args(&high, "libx264"), ["-crf", "16"]));
        assert_eq!(args(&high, "h264_nvenc").last().map(String::as_str), Some("pipe:1"));
        // A stream keeps its steady bitrate with a 1x VBV buffer (matching OBS).
        let cbr = args(&cfg(RateControl::Cbr), "h264_nvenc");
        assert!(has(&cbr, ["-b:v", "6000k"]) && has(&cbr, ["-rc", "cbr"]));
        assert!(has(&cbr, ["-maxrate", "6000k"]) && has(&cbr, ["-bufsize", "6000k"]));
        assert!(!has(&cbr, ["-bufsize", "12000k"]));
        let x264 = args(&cfg(RateControl::CappedCbr), "libx264");
        assert!(has(&x264, ["-maxrate", "6000k"]) && has(&x264, ["-bufsize", "6000k"]));
    }

    /// FFmpeg a whole queue behind (here: reading one frame a second) never holds the caller up:
    /// frames are left out instead (the frame loop waited, with the engine's lock held).
    #[test]
    fn a_slow_encoder_never_holds_the_frame_loop() {
        if ffmpeg().is_none() {
            return;
        }
        let args: Vec<String> = [
            "-re", "-f", "rawvideo", "-pix_fmt", "nv12", "-s", "64x64", "-r", "1", "-i", "pipe:0", "-f", "flv",
            "pipe:1",
        ]
        .map(String::from)
        .to_vec();
        let chunk = 64 * 64 * 3 / 2;
        let Ok(mut p) = Proc::spawn(&args, chunk) else { return };
        let t = std::time::Instant::now();
        let mut left_out = 0;
        for _ in 0..300 {
            if !p.try_write(vec![0u8; chunk]).unwrap_or(true) {
                left_out += 1;
            }
        }
        let took = t.elapsed();
        eprintln!("{left_out} of 300 left out, in {took:?}");
        assert!(took < std::time::Duration::from_secs(1), "never waited: {took:?}");
        assert!(left_out > 100, "frames left out instead: {left_out}");
        let _ = p.child.kill();
    }

    // A tiny stand-in for FFmpeg: reads 6-byte frames (a 2x2 NV12 picture) and writes one FLV video
    // tag for each, the first byte of the frame as its payload; a keyframe every `gop`; the FLV
    // header and codec header after `delay` seconds (an encoder opening); and, once its input is
    // closed, `slow` seconds more before it ends (an encoder that takes time to close).
    #[cfg(unix)]
    const FAKE_ENCODER: &str = r#"gop=$1; delay=$2; slow=$3; n=0
while :; do
  b=$(dd bs=6 count=1 2>/dev/null | od -An -tu1 | awk '{print $1}')
  [ -z "$b" ] && break
  if [ $n -eq 0 ]; then
    sleep $delay
    printf 'FLV\001\001\000\000\000\011\000\000\000\000'
    printf '\011\000\000\007\000\000\000\000\000\000\000\027\000\000\000\000AB\000\000\000\022'
  fi
  if [ $((n % gop)) -eq 0 ]; then k='\027'; else k='\047'; fi
  printf "\011\000\000\006\000\000\000\000\000\000\000${k}\001\000\000\000\\$(printf %03o "$b")\000\000\000\021"
  n=$((n+1))
done
sleep $slow"#;

    #[cfg(unix)]
    fn fake_proc(gop: u32, delay: &str, slow: &str) -> Proc {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", FAKE_ENCODER, "sh", &gop.to_string(), delay, slow]);
        Proc::start(cmd, 6).unwrap_or_else(|e| panic!("{e}"))
    }

    /// An encoder that stops at once (a hardware encoder refusing another session).
    #[cfg(unix)]
    fn dead_proc() -> Proc {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 7"]);
        Proc::start(cmd, 6).unwrap_or_else(|e| panic!("{e}"))
    }

    #[cfg(unix)]
    fn fake_video(old: Proc) -> Video {
        let mut c = cfg(RateControl::Cbr);
        (c.width, c.height) = (2, 2);
        Video::with_proc(old, &c, "fake".into(), false)
    }

    /// Frame `i` (its number is its first byte) at time `i`.
    #[cfg(unix)]
    fn feed(v: &mut Video, i: u8) -> Result<Vec<VideoPacket>, AvError> {
        let (y, uv) = ([i, 0, 0, 0], [0u8, 0]);
        v.encode(Frame::Nv12 { y: &y, y_stride: 2, uv: &uv, uv_stride: 2 }, i64::from(i))
    }

    #[cfg(unix)]
    fn start_handover(v: &mut Video, p: Proc) {
        let mut c = v.cfg;
        c.bitrate_kbps = 3000;
        v.begin_handover(p, c);
    }

    /// A bitrate step whose new encoder can't start (NVENC out of sessions) leaves the stream's
    /// encode as it was: no error, no frame lost.
    #[cfg(unix)]
    #[test]
    fn a_new_encoder_that_dies_at_once_leaves_the_encode_running() {
        let mut v = fake_video(fake_proc(4, "0", "0"));
        let mut got = Vec::new();
        for i in 0..10 {
            got.extend(feed(&mut v, i).unwrap_or_else(|e| panic!("{e}")));
        }
        start_handover(&mut v, dead_proc());
        for i in 10..40 {
            match feed(&mut v, i) {
                Ok(p) => got.extend(p),
                Err(e) => panic!("a failed bitrate step ended the encode at frame {i}: {e}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        got.extend(v.flush().unwrap_or_else(|e| panic!("{e}")));
        let pts: Vec<i64> = got.iter().map(|p| p.pts_us).collect();
        assert_eq!(pts, (0..40).collect::<Vec<i64>>(), "every frame once, in order");
    }

    /// Swapping to the new encoder never waits for the old one to close (the frame loop did,
    /// with the engine's lock held), and the stream goes on: every frame once, in order, with a
    /// keyframe where it changes over.
    #[cfg(unix)]
    #[test]
    fn a_swap_never_waits_for_the_old_encoder_and_loses_no_frame() {
        let mut v = fake_video(fake_proc(4, "0", "2"));
        let mut got = Vec::new();
        for i in 0..5 {
            got.extend(feed(&mut v, i).unwrap_or_else(|e| panic!("{e}")));
        }
        start_handover(&mut v, fake_proc(4, "0", "0"));
        let mut worst = std::time::Duration::ZERO;
        for i in 5..60 {
            let t = std::time::Instant::now();
            got.extend(feed(&mut v, i).unwrap_or_else(|e| panic!("{e}")));
            worst = worst.max(t.elapsed());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(worst < std::time::Duration::from_millis(500), "a frame waited {worst:?}");
        got.extend(v.flush().unwrap_or_else(|e| panic!("{e}")));
        let pts: Vec<i64> = got.iter().map(|p| p.pts_us).collect();
        assert_eq!(pts, (0..60).collect::<Vec<i64>>(), "every frame once, in order");
        assert!(got.iter().filter(|p| p.key).count() >= 2, "keyframes");
    }

    /// An encoder that takes its time to open (an NVENC session: a few hundred ms) takes over later,
    /// at one of its keyframes, and the stream is whole: every frame once, in order.
    #[cfg(unix)]
    #[test]
    fn a_slow_to_open_encoder_takes_over_at_a_later_keyframe() {
        let mut v = fake_video(fake_proc(4, "0", "0"));
        let mut got = Vec::new();
        for i in 0..5 {
            got.extend(feed(&mut v, i).unwrap_or_else(|e| panic!("{e}")));
        }
        start_handover(&mut v, fake_proc(4, "0.4", "0"));
        for i in 5..150 {
            got.extend(feed(&mut v, i).unwrap_or_else(|e| panic!("{e}")));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(v.next.is_none(), "it took over");
        assert_eq!(v.cfg.bitrate_kbps, 3000, "at its bitrate");
        got.extend(v.flush().unwrap_or_else(|e| panic!("{e}")));
        let pts: Vec<i64> = got.iter().map(|p| p.pts_us).collect();
        assert_eq!(pts, (0..150).collect::<Vec<i64>>(), "every frame once, in order");
        // The change-over is at a keyframe: from the frame the new one took over at, the stream is
        // the new one's (a keyframe every 4 of its frames).
        let at = got.iter().position(|p| p.pts_us >= 5 && p.key).unwrap_or(0);
        assert!(got[at].key);
    }

    /// An encoder that never gets ready (hung) is let go after its time, the stream untouched, and
    /// what was held back is given out.
    #[cfg(unix)]
    #[test]
    fn an_encoder_that_never_gets_ready_is_let_go() {
        let mut v = fake_video(fake_proc(4, "0", "0"));
        let mut got = Vec::new();
        for i in 0..5 {
            got.extend(feed(&mut v, i).unwrap_or_else(|e| panic!("{e}")));
        }
        start_handover(&mut v, fake_proc(4, "30", "0"));
        if let Some(h) = v.next.as_mut() {
            h.max_wait = std::time::Duration::from_millis(300);
        }
        for i in 5..60 {
            got.extend(feed(&mut v, i).unwrap_or_else(|e| panic!("{e}")));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(v.next.is_none(), "let go");
        assert_eq!(v.cfg.bitrate_kbps, 6000, "still the bitrate it was");
        got.extend(v.flush().unwrap_or_else(|e| panic!("{e}")));
        let pts: Vec<i64> = got.iter().map(|p| p.pts_us).collect();
        assert_eq!(pts, (0..60).collect::<Vec<i64>>(), "every frame once, in order");
    }

    /// Frames an encoder leaves out because it's behind are counted where the Studio's stats read
    /// them.
    #[cfg(unix)]
    #[test]
    fn frames_left_out_are_counted() {
        let _serial = crate::video::DROPPED_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut stuck = Command::new("sh");
        stuck.args(["-c", "exec sleep 30"]);
        let big = 256 * 256 * 3 / 2;
        let mut c = cfg(RateControl::Cbr);
        (c.width, c.height) = (256, 256);
        let mut v =
            Video::with_proc(Proc::start(stuck, big).unwrap_or_else(|e| panic!("{e}")), &c, "fake".into(), false);
        let before = crate::video::dropped_frames();
        for i in 0..200u8 {
            let _ = v.encode_raw(vec![i; big], i64::from(i));
        }
        let after = crate::video::dropped_frames();
        assert!(v.skipped >= 100, "{} left out", v.skipped);
        assert!(after - before >= v.skipped, "counted: {} of {}", after - before, v.skipped);
    }

    /// With the real FFmpeg and x264: a bitrate step in the middle of a stream changes over at a
    /// keyframe, and no frame is lost or repeated.
    #[test]
    fn a_real_bitrate_step_keeps_every_frame() {
        if ffmpeg().is_none() {
            return;
        }
        let mut c = cfg(RateControl::Cbr);
        (c.width, c.height, c.fps_num, c.keyframe_secs, c.software, c.bitrate_kbps) = (128, 72, 30, 1, true, 800);
        let Ok(mut v) = Video::new(&c) else { return };
        let (w, h) = (128usize, 72usize);
        let mut got: Vec<VideoPacket> = Vec::new();
        for i in 0..150i64 {
            if i == 40 {
                v.set_kbps(400).unwrap_or_else(|e| panic!("{e}"));
            }
            let y: Vec<u8> = (0..w * h).map(|p| ((p + i as usize * 3) % 251) as u8).collect();
            let uv = vec![128u8; w * h / 2];
            let frame = Frame::Nv12 { y: &y, y_stride: w, uv: &uv, uv_stride: w };
            got.extend(v.encode(frame, i * 33_333).unwrap_or_else(|e| panic!("frame {i}: {e}")));
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        got.extend(v.flush().unwrap_or_else(|e| panic!("{e}")));
        let pts: Vec<i64> = got.iter().map(|p| p.pts_us).collect();
        let want: Vec<i64> = (0..150).map(|i| i * 33_333).collect();
        assert_eq!(pts, want, "every frame once, in order");
        assert!(got[0].key, "starts on a keyframe");
        assert_eq!(v.cfg.bitrate_kbps, 400, "the step happened");
    }

    #[test]
    fn drain_stderr_stays_bounded_and_keeps_tail() {
        let mut big = "x".repeat(1_000_000);
        big.push_str("FINAL_ERROR_TAIL");
        let got = drain_stderr(std::io::Cursor::new(big.as_bytes()));
        assert_eq!(got.chars().count(), 400);
        assert!(got.ends_with("FINAL_ERROR_TAIL"), "got tail: {got}");
    }

    #[cfg(unix)]
    #[test]
    fn cancellable_write_unblocks_and_kills_stalled_encoder_on_cancel() {
        let mut stuck = Command::new("sh");
        stuck.args(["-c", "exec sleep 30"]);
        let proc = Proc::start_with_cap(stuck, 1).unwrap_or_else(|e| panic!("{e}"));
        let mut c = cfg(RateControl::Cbr);
        (c.width, c.height) = (128, 128);
        let big = 128 * 128 * 3 / 2;
        let cancel = Arc::new(AtomicBool::new(false));
        let mut v = Video::with_proc(proc, &c, "fake".into(), true);
        v.cancel = Some(cancel.clone());

        let cancel_bg = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(60));
            cancel_bg.store(true, Ordering::Release);
        });
        let started = std::time::Instant::now();
        let mut cancelled_err = false;
        for i in 0..20 {
            if let Err(e) = v.encode_raw(vec![0u8; big], i) {
                assert!(e.to_string().contains("cancelled"), "unexpected error: {e}");
                cancelled_err = true;
                break;
            }
        }
        assert!(cancelled_err, "blocking write should abort once cancel fires");
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "blocked past cancel: {:?}", started.elapsed());
    }

    #[test]
    fn candidates_for_platform_and_live_selection_share_same_order_on_all_oses() {
        let linux = candidates_for_platform(false, false);
        assert_eq!(linux, &["h264_nvenc", "h264_qsv", "h264_amf", "libx264"]);
        assert_eq!(select_h264_from(linux, |e| e == "h264_qsv"), Some("h264_qsv"));
        assert_eq!(select_h264_from(linux, |e| e == "h264_amf"), Some("h264_amf"));
        let windows = candidates_for_platform(true, false);
        assert_eq!(windows, &["h264_nvenc", "h264_qsv", "h264_amf", "h264_mf", "libx264"]);
        let macos = candidates_for_platform(false, true);
        assert_eq!(macos, &["h264_videotoolbox", "libx264"]);
    }

    #[cfg(unix)]
    #[test]
    fn h1_benchmark_skips_stay_local_without_polluting_global_dropped_frames() {
        let _serial = crate::video::DROPPED_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut stuck = Command::new("sh");
        stuck.args(["-c", "exec sleep 30"]);
        let big = 256 * 256 * 3 / 2;
        let mut c = cfg(RateControl::Cbr);
        (c.width, c.height) = (256, 256);
        let mut v =
            Video::with_proc(Proc::start(stuck, big).unwrap_or_else(|e| panic!("{e}")), &c, "fake".into(), false);
        v.record_global_drops = false;
        let before = crate::video::dropped_frames();
        for i in 0..200u8 {
            let _ = v.encode_raw(vec![i; big], i64::from(i));
        }
        let after = crate::video::dropped_frames();
        assert!(v.skipped >= 100, "{} left out locally", v.skipped);
        assert_eq!(after.saturating_sub(before), 0, "benchmark skips must not pollute global live dropped_frames()");
        let (depth, cap) = v.proc.queue_status();
        assert!(cap >= 2 && depth <= cap, "queue status bounded: depth={depth} cap={cap}");
    }

    #[test]
    fn ffenc_lag_logger_rate_limits_and_flags_full_queue_or_drops() {
        assert!(!should_log_lag(1_000, None, false, 1, 8));
        assert!(should_log_lag(1_000, None, false, 8, 8), "full queue triggers first warn");
        assert!(should_log_lag(1_000, None, true, 0, 8), "dropped frame triggers first warn");
        assert!(!should_log_lag(4_000, Some(1_000), true, 8, 8), "suppressed within 5s rate limit");
        assert!(should_log_lag(6_000, Some(1_000), true, 8, 8), "emits again after 5s");
    }
}
