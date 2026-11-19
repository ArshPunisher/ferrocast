//! Sources read through FFmpeg, where the system has no capture API of ours yet (Windows:
//! screens with `ddagrab`/`gdigrab`, cameras with `dshow`, and video files): FFmpeg reads the
//! input and hands over RGBA pictures (fitted into the canvas) and 48 kHz stereo sound on its
//! output, which a thread keeps for the engine, stamped with the engine's clock as they arrive.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::engine::now_ns;
use crate::{AvError, Picture};

/// Sound kept before it's taken (about 5 s of 1024-frame pieces).
const MAX_SOUND: usize = 240;
/// Frames of sound per piece.
const PIECE: usize = 1024;
/// How far ahead the media reader decodes (matches macOS AVFoundation reader).
const AHEAD_NS: u64 = 300_000_000;
/// Buffered video frames kept at most.
const MAX_FRAMES: usize = 12;
/// Byte budget for buffered video frames (~64 MB caps 4K while allowing ~8-20 frames at 1080p).
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Maximum Matroska block size accepted from FFmpeg (guards against corrupt streams).
const MAX_BLOCK_BYTES: u64 = 128 * 1024 * 1024;
/// A file's sound goes to the mixer when it plays within this long, not 300 ms ahead: what the
/// mixer already holds still plays after a pause, a seek or a cut to another scene, so what waits
/// here is what those can still stop.
const SOUND_HOLD_NS: u64 = 40_000_000;

/// FFmpeg, as the engine knows it (the app's own, or the PATH's).
pub fn ffmpeg() -> Result<PathBuf, AvError> {
    crate::ffenc::ffmpeg_path().ok_or_else(|| {
        AvError::Source("this needs FFmpeg, which Ferrocast fetches the first time: try again in a minute".into())
    })
}

/// A picture `src` fitted into `max`, its shape kept, in even numbers (what encoders want).
pub fn fit(src: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (src.0.max(1) as f64, src.1.max(1) as f64);
    let k = (max.0 as f64 / sw).min(max.1 as f64 / sh).min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(sw * k), even(sh * k))
}

/// What FFmpeg says an input is, from its first lines: the picture's size, whether there's
/// sound, and how long it runs (files). None when it can't open it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Probe {
    pub size: Option<(u32, u32)>,
    pub audio: bool,
    pub duration_ns: u64,
    /// Pictures a second, rounded up (29.97 is 30); 0 when FFmpeg doesn't say.
    pub fps: u32,
    /// The colour matrix FFmpeg names for the picture: Some(true) BT.709, Some(false) BT.601
    /// (bt470bg, smpte170m), None when it says nothing.
    pub bt709: Option<bool>,
    /// Full range ("pc", or a yuvj format) rather than TV range (16-235).
    pub full_range: bool,
    /// The video stream carries an alpha channel (e.g. yuva420p, rgba, bgra, ProRes 4444, or
    /// VP8/VP9 with `alpha_mode: 1`): must stay RGBA instead of NV12.
    pub has_alpha: bool,
    /// Explicit input video decoder required to preserve alpha (e.g. `"libvpx-vp9"` or `"libvpx"`
    /// for WebM VP9/VP8 with `alpha_mode: 1`, matching OBS `decode.c:mp_decode_init`).
    pub video_decoder: Option<&'static str>,
}

/// A picture as FFmpeg's NV12 (the Y plane, then U and V interleaved at half size), and how its
/// colours are read: turned into RGB on the card (`Compositor::upload_nv12`), so no RGBA copy is
/// made on the CPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12Picture {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// BT.709's matrix, else BT.601's.
    pub bt709: bool,
    pub full_range: bool,
}

/// FFmpeg's description of `input` (its arguments, `-i` and all), read in at most `wait`.
pub fn probe(ffmpeg: &Path, input: &[String], wait: Duration) -> Option<Probe> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin"])
        .args(input)
        // One picture, and a second at most of everything else (a long file's sound would
        // otherwise be read to its end).
        .args(["-frames:v", "1", "-t", "1", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().ok()?;
    let err = child.stderr.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::BufReader::new(err).read_to_string(&mut text);
        let _ = tx.send(text);
    });
    let text = match rx.recv_timeout(wait) {
        Ok(t) => t,
        Err(_) => {
            let _ = child.kill();
            return None;
        }
    };
    let _ = child.wait();
    parse_probe(&text)
}

/// Whether pictures may be decoded on the GPU (`-hwaccel auto`: D3D11/DXVA on Windows,
/// VideoToolbox on a Mac). FFmpeg falls back to the CPU by itself when the GPU can't; setting
/// `FERROCAST_CPU_DECODE` keeps everything on the CPU.
pub fn hw_decode_allowed() -> bool {
    std::env::var_os("FERROCAST_CPU_DECODE").is_none_or(|v| v.is_empty() || v == "0")
}

/// One picture of `input` decoded with `-hwaccel auto`, to be sure the GPU path works for this
/// file before playing it that way.
fn hw_decodes(ffmpeg: &Path, input: &[String]) -> bool {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-hwaccel", "auto"])
        .args(input)
        .args(["-an", "-frames:v", "1", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = cmd.spawn() else { return false };
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Whether an FFmpeg pixel format name carries an alpha channel.
fn pix_fmt_has_alpha(tok: &str) -> bool {
    let base = tok.split('(').next().unwrap_or(tok);
    base.starts_with("yuva")
        || base.starts_with("rgba")
        || base.starts_with("bgra")
        || base.starts_with("argb")
        || base.starts_with("abgr")
        || base.starts_with("gbrap")
        || base.starts_with("ayuv")
        || base.starts_with("ya8")
        || base.starts_with("ya16")
        || base == "pal8"
}

/// The probe's facts from FFmpeg's text (the first video and audio stream).
pub fn parse_probe(text: &str) -> Option<Probe> {
    let mut p = Probe::default();
    let mut any = false;
    let mut in_video_stream = false;
    let mut video_codec: Option<&str> = None;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("Output #") {
            break;
        }
        if l.starts_with("Input #") {
            in_video_stream = false;
        }
        if let Some(d) = l.strip_prefix("Duration: ").and_then(|r| r.split(',').next()) {
            let parts: Vec<f64> = d.split(':').filter_map(|x| x.trim().parse().ok()).collect();
            if parts.len() == 3 {
                p.duration_ns = ((parts[0] * 3600.0 + parts[1] * 60.0 + parts[2]) * 1e9) as u64;
            }
        }
        if in_video_stream
            && !l.starts_with("Stream #")
            && let Some((k, v)) = l.split_once(':')
            && k.trim().eq_ignore_ascii_case("alpha_mode")
            && v.trim() == "1"
        {
            p.has_alpha = true;
            if video_codec == Some("vp9") {
                p.video_decoder = Some("libvpx-vp9");
            } else if video_codec == Some("vp8") {
                p.video_decoder = Some("libvpx");
            }
        }
        if !l.starts_with("Stream #") {
            continue;
        }
        in_video_stream = false;
        if l.contains("Video:") && p.size.is_none() {
            any = true;
            in_video_stream = true;
            // The first "WxH" token after "Video:" (FFmpeg writes it as one).
            let after = l.split("Video:").nth(1).unwrap_or_default();
            video_codec = after.trim_start().split([' ', ',']).next();
            p.size = after.split([' ', ',']).find_map(|tok| {
                let (w, h) = tok.split_once('x')?;
                Some((w.parse().ok()?, h.trim_end_matches(']').parse().ok()?))
            });
            // "…, 29.97 fps, 30 tbr, …": its own rate (else the guessed one, tbr).
            let rate =
                |unit: &str| l.split(", ").find_map(|part| part.trim().strip_suffix(unit)?.trim().parse::<f64>().ok());
            p.fps = rate(" fps").or_else(|| rate(" tbr")).map_or(0, |f| f.ceil().clamp(0.0, 240.0) as u32);
            // "yuv420p(tv, bt709, progressive)", "yuvj420p(pc, bt470bg/bt709/unknown)": the range,
            // then the matrix (the first of matrix/primaries/transfer when they differ).
            // The pixel format's own brackets, not the codec's ("h264 (High) (avc1 / …)").
            let tags: Vec<&str> = after
                .match_indices('(')
                .find(|(at, _)| {
                    let word = after[..*at].rsplit([' ', ',']).next().unwrap_or_default();
                    word.starts_with("yuv") || word.starts_with("nv")
                })
                .and_then(|(at, _)| after[at + 1..].split_once(')'))
                .map(|(inside, _)| inside.split(',').map(str::trim).collect())
                .unwrap_or_default();
            let ranged = matches!(tags.first(), Some(&"tv" | &"pc"));
            p.full_range = tags.first() == Some(&"pc") || after.split([' ', ',']).any(|t| t.starts_with("yuvj"));
            let matrix = if ranged { tags.get(1).and_then(|t| t.split('/').next()) } else { None };
            p.bt709 = match matrix {
                Some("bt709") => Some(true),
                Some("bt470bg" | "smpte170m" | "bt601") => Some(false),
                _ => None,
            };
            // Check pixel format field (second comma-separated section after "Video:") for alpha formats.
            if let Some(pix_section) = after.split(',').nth(1)
                && pix_section.split_whitespace().any(pix_fmt_has_alpha)
            {
                p.has_alpha = true;
            }
        } else if l.contains("Audio:") {
            any = true;
            p.audio = true;
        }
    }
    any.then_some(p)
}

#[derive(Debug, Default)]
struct Shared {
    picture: Mutex<Option<(Picture, u64)>>,
    /// Sound as it came: its samples and when it came (ns).
    sound: Mutex<VecDeque<(Vec<f32>, u64)>>,
    frames: AtomicU64,
    stop: AtomicBool,
    ended: AtomicBool,
    error: Mutex<Option<String>>,
}

/// One FFmpeg reading a live input (screen, camera, mic); dropping it stops FFmpeg.
#[derive(Debug)]
struct Reader {
    child: Option<Child>,
    threads: Vec<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(c) = &mut self.child {
            let _ = c.kill();
            let _ = c.wait();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// What a live reader hands over.
#[derive(Debug, Clone, Copy)]
enum Out {
    /// RGBA pictures this size.
    Pictures(u32, u32),
    /// 48 kHz stereo floats.
    Sound,
}

impl Reader {
    fn start(ffmpeg: &Path, input: &[String], out: Out, fps: u32, shared: Arc<Shared>) -> Result<Self, AvError> {
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]).args(input);
        match out {
            Out::Pictures(w, h) => {
                let vf = format!("scale={w}:{h}:flags=bilinear,format=rgba");
                cmd.args(["-an", "-vf", &vf, "-r", &fps.max(1).to_string(), "-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
            }
            Out::Sound => {
                cmd.args(["-vn", "-ac", "2", "-ar", "48000", "-f", "f32le", "-"]);
            }
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window flashing up for it.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| AvError::Source(format!("FFmpeg didn't start ({e})")))?;
        let mut stdout = child.stdout.take().ok_or_else(|| AvError::Source("FFmpeg gave no output".into()))?;
        let stderr = child.stderr.take();
        let s = shared.clone();
        let reading = std::thread::Builder::new()
            .name("ferrocast-ffsrc".into())
            .spawn(move || {
                match out {
                    Out::Pictures(w, h) => {
                        let mut buf = vec![0u8; (w * h * 4) as usize];
                        while !s.stop.load(Ordering::Acquire) && stdout.read_exact(&mut buf).is_ok() {
                            let at = now_ns();
                            let p = Picture { width: w, height: h, rgba: buf.clone(), premultiplied: false };
                            if let Ok(mut l) = s.picture.lock() {
                                *l = Some((p, at));
                            }
                            s.frames.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Out::Sound => {
                        let mut buf = vec![0u8; PIECE * 2 * 4];
                        while !s.stop.load(Ordering::Acquire) && stdout.read_exact(&mut buf).is_ok() {
                            let pcm: Vec<f32> = buf.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
                            if let Ok(mut q) = s.sound.lock() {
                                q.push_back((pcm, now_ns()));
                                while q.len() > MAX_SOUND {
                                    q.pop_front();
                                }
                            }
                        }
                    }
                }
                s.ended.store(true, Ordering::Release);
            })
            .map_err(|e| AvError::Source(e.to_string()))?;
        let mut threads = vec![reading];
        if let Some(err) = stderr {
            let s = shared.clone();
            if let Ok(t) = std::thread::Builder::new().name("ferrocast-ffsrc-log".into()).spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    if !line.trim().is_empty()
                        && let Ok(mut e) = s.error.lock()
                    {
                        *e = Some(line.trim().chars().take(300).collect());
                    }
                }
            }) {
                threads.push(t);
            }
        }
        Ok(Reader { child: Some(child), threads, shared })
    }
}

/// Live sound alone from an input (a microphone, what the computer plays).
#[derive(Debug)]
pub struct LiveSound(Reader);

impl LiveSound {
    /// Starts reading `input` (FFmpeg's input arguments) as 48 kHz stereo.
    pub fn start(input: &[String]) -> Result<Self, AvError> {
        let ff = ffmpeg()?;
        Ok(LiveSound(Reader::start(&ff, input, Out::Sound, 30, Arc::new(Shared::default()))?))
    }

    /// Sound since the last call, oldest first.
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.0.shared.sound.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default()
    }

    /// FFmpeg stopped (the device went), and its last words.
    pub fn ended(&self) -> Option<String> {
        self.0
            .shared
            .ended
            .load(Ordering::Acquire)
            .then(|| self.0.shared.error.lock().ok().and_then(|e| e.clone()).unwrap_or_default())
    }
}

/// Live pictures (and sound, when asked) from an input: a screen, a camera.
#[derive(Debug)]
pub struct Live {
    pictures: Reader,
    sound: Option<Reader>,
    pub size: (u32, u32),
}

impl Live {
    /// Starts reading `input` (FFmpeg's input arguments), pictures fitted into `max` at up to
    /// `fps`; with `sound`, its sound from `sound_input` (None: the same input).
    pub fn start(input: &[String], max: (u32, u32), fps: u32, sound_input: Option<&[String]>) -> Result<Self, AvError> {
        let ff = ffmpeg()?;
        let p = probe(&ff, input, Duration::from_secs(10))
            .ok_or_else(|| AvError::Source("FFmpeg couldn't open it".into()))?;
        let size = fit(p.size.ok_or_else(|| AvError::Source("it has no picture".into()))?, max);
        let pictures = Reader::start(&ff, input, Out::Pictures(size.0, size.1), fps, Arc::new(Shared::default()))?;
        let sound = match sound_input {
            Some(i) => Some(Reader::start(&ff, i, Out::Sound, fps, Arc::new(Shared::default()))?),
            None => None,
        };
        Ok(Live { pictures, sound, size })
    }

    /// The newest picture and when it came (taken: None until the next one).
    pub fn take(&self) -> Option<(Picture, u64)> {
        self.pictures.shared.picture.lock().ok()?.take()
    }

    /// Sound since the last call, oldest first.
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.sound
            .as_ref()
            .and_then(|s| s.shared.sound.lock().ok().map(|mut q| q.drain(..).collect()))
            .unwrap_or_default()
    }

    pub fn frames(&self) -> u64 {
        self.pictures.shared.frames.load(Ordering::Relaxed)
    }

    /// FFmpeg stopped (the device went, the window closed), and its last words.
    pub fn ended(&self) -> Option<String> {
        self.pictures
            .shared
            .ended
            .load(Ordering::Acquire)
            .then(|| self.pictures.shared.error.lock().ok().and_then(|e| e.clone()).unwrap_or_default())
    }
}

/// OBS's media controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Play,
    Pause,
    /// From the start, playing.
    Restart,
    /// Back to the start, showing nothing until played.
    Stop,
    /// To this place in the file (ns); playing or paused as it was.
    Seek(u64),
}

/// Where a file's playing stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaState {
    Playing,
    Paused,
    Stopped,
    Ended,
}

/// Playback options for a media file (`ffmpeg_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaOptions {
    pub looping: bool,
    /// Restart from 0 when the source becomes active on air, and stop when hidden.
    pub restart_on_activate: bool,
    /// Close the file and decoder when hidden, and reopen when active again.
    pub close_when_inactive: bool,
    /// Clear the last picture when playback ends (or stops on deactivate).
    pub clear_on_media_end: bool,
    /// Playback speed in percent (1..=200, default 100).
    pub speed_percent: u32,
    /// Allow GPU hardware decoding (default true).
    pub hw_decode: bool,
}

impl Default for MediaOptions {
    fn default() -> Self {
        Self {
            looping: false,
            restart_on_activate: false,
            close_when_inactive: false,
            clear_on_media_end: false,
            speed_percent: 100,
            hw_decode: true,
        }
    }
}

impl MediaOptions {
    pub fn looping(looping: bool) -> Self {
        Self { looping, ..Self::default() }
    }

    pub fn speed(self) -> u32 {
        self.speed_percent.clamp(1, 200)
    }
}

/// Host time (ns) at which a sample with media `pts_ns` plays, given that `base_pts_ns` plays at
/// `base_host_ns` and playback runs at `speed_percent` (1..=200). Uses signed arithmetic so
/// seeking past system uptime (`base_pts_ns > base_host_ns`) never underflows or freezes.
pub(crate) fn schedule_at(base_host_ns: u64, base_pts_ns: i128, pts_ns: i128, speed_percent: u32) -> u64 {
    let speed = i128::from(speed_percent.clamp(1, 200));
    let delta_host = (pts_ns - base_pts_ns) * 100 / speed;
    (i128::from(base_host_ns) + delta_host).max(0) as u64
}

/// Streaming linear resampler for interleaved stereo f32 PCM at `speed_percent` (1..=200),
/// matching OBS's `samples_per_sec = sample_rate * speed / 100` playback-rate scaling across
/// chunk boundaries without clicks. Passthrough at 100%.
#[derive(Debug, Clone)]
pub(crate) struct SpeedResampler {
    speed_percent: u32,
    prev: [f32; 2],
    has_prev: bool,
    pos: f64,
}

impl SpeedResampler {
    pub(crate) fn new(speed_percent: u32) -> Self {
        Self { speed_percent: speed_percent.clamp(1, 200), prev: [0.0; 2], has_prev: false, pos: 0.0 }
    }

    pub(crate) fn reset(&mut self) {
        self.prev = [0.0; 2];
        self.has_prev = false;
        self.pos = 0.0;
    }

    pub(crate) fn process(&mut self, pcm: Vec<f32>) -> Vec<f32> {
        if self.speed_percent == 100 || pcm.len() < 2 {
            return pcm;
        }
        let n = pcm.len() / 2;
        if !self.has_prev {
            self.prev = [pcm[0], pcm[1]];
            self.has_prev = true;
            self.pos = 1.0;
        }
        let step = f64::from(self.speed_percent) / 100.0;
        let est = ((n as f64 / step).ceil() as usize + 2) * 2;
        let mut out = Vec::with_capacity(est);
        let frame = |idx: usize| -> [f32; 2] {
            if idx == 0 {
                self.prev
            } else {
                let off = (idx - 1) * 2;
                [pcm[off], pcm[off + 1]]
            }
        };
        let limit = n as f64;
        while self.pos < limit {
            let i0 = self.pos.floor() as usize;
            let i1 = (i0 + 1).min(n);
            let frac = (self.pos - i0 as f64) as f32;
            let a = frame(i0);
            let b = frame(i1);
            out.push(a[0] + (b[0] - a[0]) * frac);
            out.push(a[1] + (b[1] - a[1]) * frac);
            self.pos += step;
        }
        let last = (n - 1) * 2;
        self.prev = [pcm[last], pcm[last + 1]];
        self.pos -= limit;
        out
    }
}

/// One decoded block read from FFmpeg's single-process Matroska output stream.
#[derive(Debug)]
enum MkvBlock {
    Video { pts_ns: i128, data: Vec<u8> },
    Audio { pts_ns: i128, data: Vec<u8> },
}

/// Minimal streaming EBML/Matroska demuxer for FFmpeg's `-f matroska pipe:1` output containing
/// rawvideo (NV12 or RGBA) and interleaved `pcm_f32le` audio on one shared PTS timeline.
struct MkvStream<R: Read> {
    r: R,
    ts_scale: u64,
    cluster_ts: u64,
    video_track: Option<u64>,
    audio_track: Option<u64>,
}

impl<R: Read> MkvStream<R> {
    fn new(r: R) -> Self {
        Self { r, ts_scale: 1_000_000, cluster_ts: 0, video_track: None, audio_track: None }
    }

    fn next_block(&mut self) -> io::Result<Option<MkvBlock>> {
        loop {
            let Some(id) = read_ebml_id(&mut self.r)? else {
                return Ok(None);
            };
            let size = read_ebml_size(&mut self.r)?;
            match id {
                // Master containers we descend into without skipping (handles unknown-size streams).
                0x1853_8067 | 0x1549_A966 | 0x1654_AE6B | 0xA0 => {}
                // Cluster: reset cluster timestamp until its Timestamp (0xE7) child arrives.
                0x1F43_B675 => {
                    self.cluster_ts = 0;
                }
                // Info -> TimestampScale (ns per tick, default 1_000_000).
                0x002A_D7B1 => {
                    let len = size.unwrap_or(0);
                    let v = read_ebml_uint(&mut self.r, len)?;
                    if v > 0 {
                        self.ts_scale = v;
                    }
                }
                // Tracks -> TrackEntry: map TrackNumber (0xD7) and TrackType (0x83).
                0xAE => {
                    if let Some(len) = size
                        && len <= 64 * 1024
                    {
                        let mut buf = vec![0u8; len as usize];
                        self.r.read_exact(&mut buf)?;
                        if let Some((num, kind)) = parse_track_entry(&buf) {
                            match kind {
                                1 => self.video_track = Some(num),
                                2 => self.audio_track = Some(num),
                                _ => {}
                            }
                        }
                    } else if let Some(len) = size {
                        skip_exact(&mut self.r, len)?;
                    } else {
                        return Ok(None);
                    }
                }
                // Cluster -> Timestamp.
                0xE7 => {
                    let len = size.unwrap_or(0);
                    self.cluster_ts = read_ebml_uint(&mut self.r, len)?;
                }
                // SimpleBlock (0xA3) / Block (0xA1).
                0xA3 | 0xA1 => {
                    let Some(total) = size else {
                        return Ok(None);
                    };
                    if !(4..=MAX_BLOCK_BYTES).contains(&total) {
                        if total <= MAX_BLOCK_BYTES {
                            skip_exact(&mut self.r, total)?;
                            continue;
                        }
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "Matroska block too large"));
                    }
                    let (track, vint_len) = read_vint_value(&mut self.r)?;
                    let mut hdr = [0u8; 3];
                    self.r.read_exact(&mut hdr)?;
                    let hdr_len = vint_len as u64 + 3;
                    if total < hdr_len {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated Matroska block"));
                    }
                    let payload_len = (total - hdr_len) as usize;
                    let rel_ts = i16::from_be_bytes([hdr[0], hdr[1]]);
                    let lacing = (hdr[2] >> 1) & 0x03;
                    let pts_ns = (i128::from(self.cluster_ts) + i128::from(rel_ts)) * i128::from(self.ts_scale);
                    if lacing == 0 && Some(track) == self.video_track {
                        let mut data = vec![0u8; payload_len];
                        self.r.read_exact(&mut data)?;
                        return Ok(Some(MkvBlock::Video { pts_ns, data }));
                    }
                    if lacing == 0 && Some(track) == self.audio_track {
                        let mut data = vec![0u8; payload_len];
                        self.r.read_exact(&mut data)?;
                        return Ok(Some(MkvBlock::Audio { pts_ns, data }));
                    }
                    skip_exact(&mut self.r, payload_len as u64)?;
                }
                // Any other element (EBML header, SeekHead, Tags, Void, Cues…): skip.
                _ => match size {
                    Some(len) => skip_exact(&mut self.r, len)?,
                    None => return Ok(None),
                },
            }
        }
    }
}

fn read_byte(r: &mut impl Read) -> io::Result<Option<u8>> {
    let mut b = [0u8; 1];
    match r.read(&mut b) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(b[0])),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => read_byte(r),
        Err(e) => Err(e),
    }
}

fn read_ebml_id(r: &mut impl Read) -> io::Result<Option<u32>> {
    let Some(b0) = read_byte(r)? else {
        return Ok(None);
    };
    let z = b0.leading_zeros() as usize;
    if z >= 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid EBML element ID"));
    }
    let mut id = u32::from(b0);
    let mut rest = [0u8; 3];
    if z > 0 {
        r.read_exact(&mut rest[..z])?;
        for &b in &rest[..z] {
            id = (id << 8) | u32::from(b);
        }
    }
    Ok(Some(id))
}

fn read_vint_value(r: &mut impl Read) -> io::Result<(u64, usize)> {
    let mut b0 = [0u8; 1];
    r.read_exact(&mut b0)?;
    let z = b0[0].leading_zeros() as usize;
    if z >= 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid EBML VINT"));
    }
    let mask = if z == 7 { 0 } else { 0x7F >> z };
    let mut val = u64::from(b0[0] & mask);
    let mut rest = [0u8; 7];
    if z > 0 {
        r.read_exact(&mut rest[..z])?;
        for &b in &rest[..z] {
            val = (val << 8) | u64::from(b);
        }
    }
    Ok((val, z + 1))
}

fn read_ebml_size(r: &mut impl Read) -> io::Result<Option<u64>> {
    let (val, len) = read_vint_value(r)?;
    let unknown = (1u64 << (7 * len)) - 1;
    if val == unknown { Ok(None) } else { Ok(Some(val)) }
}

fn read_ebml_uint(r: &mut impl Read, len: u64) -> io::Result<u64> {
    if len > 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "EBML uint > 8 bytes"));
    }
    let mut buf = [0u8; 8];
    let n = len as usize;
    r.read_exact(&mut buf[..n])?;
    let mut v = 0u64;
    for &b in &buf[..n] {
        v = (v << 8) | u64::from(b);
    }
    Ok(v)
}

fn skip_exact(r: &mut impl Read, mut len: u64) -> io::Result<()> {
    let mut scratch = [0u8; 4096];
    while len > 0 {
        let take = (len as usize).min(scratch.len());
        r.read_exact(&mut scratch[..take])?;
        len -= take as u64;
    }
    Ok(())
}

fn parse_track_entry(mut slice: &[u8]) -> Option<(u64, u64)> {
    let (mut num, mut kind) = (None, None);
    while !slice.is_empty() {
        let id = read_ebml_id(&mut slice).ok()??;
        let len = read_ebml_size(&mut slice).ok()??;
        match id {
            0xD7 => num = read_ebml_uint(&mut slice, len).ok(),
            0x83 => kind = read_ebml_uint(&mut slice, len).ok(),
            _ => skip_exact(&mut slice, len).ok()?,
        }
    }
    Some((num?, kind?))
}

#[derive(Debug)]
enum FrameData {
    Rgba(Picture),
    Nv12(Nv12Picture),
}

impl FrameData {
    fn byte_len(&self) -> usize {
        match self {
            FrameData::Rgba(p) => p.rgba.len(),
            FrameData::Nv12(p) => p.data.len(),
        }
    }
}

#[derive(Debug, Default)]
struct MediaCmd {
    queue: VecDeque<Control>,
    paused: bool,
    stopped: bool,
    ended: bool,
}

#[derive(Debug, Default)]
struct MediaShared {
    /// Decoded video frames: pixel data, host display time (ns), file position (ns).
    frames: Mutex<VecDeque<(FrameData, u64, u64)>>,
    /// Decoded stereo 48 kHz sound pieces: PCM, host play time (ns), file position (ns).
    sound: Mutex<VecDeque<(Vec<f32>, u64, u64)>>,
    cmd: Mutex<MediaCmd>,
    wake: Condvar,
    position: AtomicU64,
    /// Active FFmpeg child for this pass, stored so `control()` / `Drop` can issue a non-blocking
    /// `kill()` without ever calling `wait()` on the caller's thread.
    active_child: Mutex<Option<Child>>,
    hw: AtomicBool,
    error: Mutex<Option<String>>,
}

impl MediaShared {
    fn new(hw: bool, start_stopped: bool) -> Self {
        Self {
            cmd: Mutex::new(MediaCmd {
                queue: VecDeque::new(),
                paused: start_stopped,
                stopped: start_stopped,
                ended: false,
            }),
            hw: AtomicBool::new(hw),
            ..Self::default()
        }
    }

    fn finished(&self) -> bool {
        let ended = self.cmd.lock().map(|st| st.ended).unwrap_or(false);
        ended
            && self.frames.lock().map(|q| q.is_empty()).unwrap_or(true)
            && self.sound.lock().map(|q| q.is_empty()).unwrap_or(true)
    }

    fn clear_queues(&self) {
        if let Ok(mut q) = self.frames.lock() {
            q.clear();
        }
        if let Ok(mut q) = self.sound.lock() {
            q.clear();
        }
    }
}

fn frame_queue_has_room(q: &VecDeque<(FrameData, u64, u64)>) -> bool {
    if q.len() >= MAX_FRAMES {
        return false;
    }
    if q.len() < 2 {
        return true;
    }
    q.iter().map(|(f, ..)| f.byte_len()).sum::<usize>() < MAX_FRAME_BYTES
}

/// Sound pieces due to play by `now + hold`, popped in order; later pieces wait in the queue so
/// pause/seek/stop can drop them before they reach the mixer.
fn take_due_sound(q: &mut VecDeque<(Vec<f32>, u64, u64)>, now: u64, hold: u64) -> Vec<(Vec<f32>, u64, u64)> {
    let mut out = Vec::new();
    while q.front().is_some_and(|(_, at, _)| *at <= now.saturating_add(hold)) {
        out.extend(q.pop_front());
    }
    out
}

struct PassSpec {
    ffmpeg: PathBuf,
    path: PathBuf,
    has_video: bool,
    has_audio: bool,
    size: (u32, u32),
    nv12: Option<(bool, bool)>,
    video_decoder: Option<&'static str>,
    looping: bool,
    speed_percent: u32,
    duration_ns: u64,
    hwaccel: &'static str,
}

fn build_pass_args(spec: &PassSpec, from_ns: u64, use_hw: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into(), "-nostdin".into()];
    if use_hw && spec.has_video && spec.video_decoder.is_none() {
        a.extend(["-hwaccel".into(), spec.hwaccel.into()]);
    }
    if let Some(dec) = spec.video_decoder
        && spec.has_video
    {
        a.extend(["-c:v".into(), dec.into()]);
    }
    if from_ns > 0 {
        a.extend(["-ss".into(), format!("{:.6}", from_ns as f64 / 1e9)]);
    }
    a.extend(["-i".into(), spec.path.display().to_string()]);
    if spec.has_video {
        let (w, h) = spec.size;
        let pix = if spec.nv12.is_some() { "nv12" } else { "rgba" };
        let vf = format!("scale={w}:{h}:flags=bilinear,format={pix}");
        a.extend([
            "-map".into(),
            "0:v:0".into(),
            "-fps_mode".into(),
            "passthrough".into(),
            "-vf".into(),
            vf,
            "-c:v".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            pix.into(),
        ]);
    } else {
        a.push("-vn".into());
    }
    if spec.has_audio {
        a.extend([
            "-map".into(),
            "0:a:0".into(),
            "-c:a".into(),
            "pcm_f32le".into(),
            "-ac".into(),
            "2".into(),
            "-ar".into(),
            "48000".into(),
        ]);
    } else {
        a.push("-an".into());
    }
    a.extend(["-flush_packets".into(), "1".into(), "-f".into(), "matroska".into(), "pipe:1".into()]);
    a
}

/// Drains `stderr` completely to EOF into a bounded tail string so FFmpeg never blocks or dies
/// from `SIGPIPE` on stderr output.
fn drain_stderr(mut err: impl Read) -> String {
    const KEEP: usize = 8 * 1024;
    let mut ring: VecDeque<u8> = VecDeque::new();
    let mut buf = [0u8; 4096];
    loop {
        match err.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if ring.len() == KEEP {
                        ring.pop_front();
                    }
                    ring.push_back(b);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let bytes: Vec<u8> = ring.into_iter().collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Dedicated reader thread for `ffsrc::Media`: runs one FFmpeg process per pass outputting both
/// video and audio in Matroska on one shared PTS timeline, paces output to `AHEAD_NS`, handles
/// non-blocking controls, seamless loops, and automatic HW->SW fallback.
fn read_media(spec: PassSpec, start_stopped: bool, shared: Arc<MediaShared>, stop: Arc<AtomicBool>) {
    let clamp = |n: u64| if spec.duration_ns > 0 { n.min(spec.duration_ns) } else { n };
    let expected_video_bytes = if spec.nv12.is_some() {
        (spec.size.0 * spec.size.1 * 3 / 2) as usize
    } else {
        (spec.size.0 * spec.size.1 * 4) as usize
    };
    let mut reader_state = if start_stopped { MediaState::Stopped } else { MediaState::Playing };
    let mut resampler = SpeedResampler::new(spec.speed_percent);
    let (mut from, mut follows): (u64, Option<u64>) = (0, None);

    'passes: loop {
        // Paused, stopped, or ended: wait for a control without spinning.
        while reader_state != MediaState::Playing {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let cmd = {
                let Ok(mut st) = shared.cmd.lock() else { return };
                while st.queue.is_empty() && !stop.load(Ordering::Acquire) {
                    let Ok((guard, _)) = shared.wake.wait_timeout(st, Duration::from_millis(50)) else { return };
                    st = guard;
                }
                st.queue.pop_front()
            };
            match cmd {
                Some(Control::Play) => {
                    if matches!(reader_state, MediaState::Stopped | MediaState::Ended) {
                        from = 0;
                    }
                    reader_state = MediaState::Playing;
                }
                Some(Control::Restart) => {
                    from = 0;
                    shared.position.store(0, Ordering::Relaxed);
                    reader_state = MediaState::Playing;
                }
                Some(Control::Stop) => {
                    from = 0;
                    shared.position.store(0, Ordering::Relaxed);
                    shared.clear_queues();
                    reader_state = MediaState::Stopped;
                }
                Some(Control::Pause) => {
                    if reader_state != MediaState::Stopped {
                        reader_state = MediaState::Paused;
                    }
                }
                Some(Control::Seek(n)) => {
                    from = clamp(n);
                    shared.position.store(from, Ordering::Relaxed);
                    shared.clear_queues();
                    if matches!(reader_state, MediaState::Ended | MediaState::Stopped) {
                        reader_state = MediaState::Paused;
                    }
                }
                None => {}
            }
            follows = None;
        }

        // Drain any already-queued control before spawning FFmpeg so rapid seeks coalesce cleanly.
        if let Some(c) = shared.cmd.lock().ok().and_then(|mut st| st.queue.pop_front()) {
            match c {
                Control::Play => {}
                Control::Pause => {
                    from = shared.position.load(Ordering::Relaxed);
                    reader_state = MediaState::Paused;
                    shared.clear_queues();
                    follows = None;
                    continue 'passes;
                }
                Control::Seek(n) => {
                    from = clamp(n);
                    shared.position.store(from, Ordering::Relaxed);
                    shared.clear_queues();
                    follows = None;
                    continue 'passes;
                }
                Control::Restart => {
                    from = 0;
                    shared.position.store(0, Ordering::Relaxed);
                    shared.clear_queues();
                    follows = None;
                    continue 'passes;
                }
                Control::Stop => {
                    from = 0;
                    shared.position.store(0, Ordering::Relaxed);
                    reader_state = MediaState::Stopped;
                    shared.clear_queues();
                    follows = None;
                    continue 'passes;
                }
            }
        }

        let use_hw = shared.hw.load(Ordering::Relaxed) && spec.has_video;
        let args = build_pass_args(&spec, from, use_hw);
        let mut cmd = Command::new(&spec.ffmpeg);
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                if let Ok(mut err) = shared.error.lock() {
                    *err = Some(format!("FFmpeg didn't start ({e})"));
                }
                shared.clear_queues();
                reader_state = MediaState::Ended;
                if let Ok(mut st) = shared.cmd.lock()
                    && st.queue.is_empty()
                {
                    st.ended = true;
                }
                follows = None;
                continue 'passes;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            reader_state = MediaState::Ended;
            continue 'passes;
        };
        let err_thread = child.stderr.take().map(|err| {
            std::thread::Builder::new().name("ferrocast-ffsrc-err".into()).spawn(move || drain_stderr(err)).ok()
        });
        {
            // Store child for non-blocking kill() by control()/Drop, checking if stop/control arrived
            // while spawning so we never miss an interrupt.
            let pending_interrupt = stop.load(Ordering::Acquire)
                || shared.cmd.lock().map(|st| st.queue.iter().any(|c| *c != Control::Play)).unwrap_or(false);
            if pending_interrupt {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(Some(t)) = err_thread {
                    let _ = t.join();
                }
                if stop.load(Ordering::Acquire) {
                    return;
                }
                shared.clear_queues();
                follows = None;
                continue 'passes;
            }
            if let Ok(mut slot) = shared.active_child.lock() {
                *slot = Some(child);
            }
        }

        resampler.reset();
        let mut demux = MkvStream::new(BufReader::with_capacity(256 * 1024, stdout));
        let mut base_host_ns: Option<u64> = None;
        let mut base_pts_ns: i128 = 0;
        let mut decoded_units = 0u64;
        let mut last_end: i128 = 0;
        let mut prev_v_pts: Option<i128> = None;
        let mut last_v_step: i128 = 33_333_333;
        let mut audio_cursor_at: Option<u64> = None;
        let mut interrupted = false;

        'blocks: loop {
            if stop.load(Ordering::Acquire) {
                interrupted = true;
                break 'blocks;
            }
            if shared.cmd.lock().map(|st| st.queue.iter().any(|c| *c != Control::Play)).unwrap_or(false) {
                interrupted = true;
                break 'blocks;
            }
            let block = match demux.next_block() {
                Ok(Some(b)) => b,
                Ok(None) | Err(_) => break 'blocks,
            };
            let pts_ns = match &block {
                MkvBlock::Video { pts_ns, .. } | MkvBlock::Audio { pts_ns, .. } => *pts_ns,
            };
            let b_host = match base_host_ns {
                Some(h) => h,
                None => {
                    base_pts_ns = pts_ns;
                    last_end = pts_ns;
                    // Anchor after the first block is decoded so FFmpeg startup/seek latency
                    // doesn't make the first frame arrive late; 60 ms gives the mixer's 40 ms
                    // sound hold clean headroom.
                    let h = follows.unwrap_or_else(|| now_ns() + 60_000_000);
                    base_host_ns = Some(h);
                    h
                }
            };
            let at = schedule_at(b_host, base_pts_ns, pts_ns, spec.speed_percent);
            let is_video = matches!(block, MkvBlock::Video { .. });

            // Backpressure: wait while this block plays more than AHEAD_NS in the future or the
            // destination queue is full; OS pipe backpressure pauses FFmpeg with 0% CPU.
            loop {
                if stop.load(Ordering::Acquire) {
                    interrupted = true;
                    break 'blocks;
                }
                let has_cmd = shared.cmd.lock().map(|st| st.queue.iter().any(|c| *c != Control::Play)).unwrap_or(false);
                if has_cmd {
                    interrupted = true;
                    break 'blocks;
                }
                let room = if is_video {
                    shared.frames.lock().map(|q| frame_queue_has_room(&q)).unwrap_or(false)
                } else {
                    shared.sound.lock().map(|q| q.len() < MAX_SOUND).unwrap_or(false)
                };
                if room && at <= now_ns() + AHEAD_NS {
                    break;
                }
                if let Ok(st) = shared.cmd.lock()
                    && st.queue.is_empty()
                    && !stop.load(Ordering::Acquire)
                {
                    let _ = shared.wake.wait_timeout(st, Duration::from_millis(5));
                }
            }

            match block {
                MkvBlock::Video { pts_ns, data } => {
                    if data.len() != expected_video_bytes {
                        continue 'blocks;
                    }
                    let frame = match spec.nv12 {
                        Some((bt709, full_range)) => FrameData::Nv12(Nv12Picture {
                            width: spec.size.0,
                            height: spec.size.1,
                            data,
                            bt709,
                            full_range,
                        }),
                        None => FrameData::Rgba(Picture {
                            width: spec.size.0,
                            height: spec.size.1,
                            rgba: data,
                            premultiplied: false,
                        }),
                    };
                    let file_pos = (i128::from(from) + pts_ns).max(0) as u64;
                    if let Ok(mut q) = shared.frames.lock() {
                        q.push_back((frame, at, file_pos));
                    }
                    if let Some(pv) = prev_v_pts
                        && pts_ns > pv
                    {
                        last_v_step = (pts_ns - pv).min(1_000_000_000);
                    }
                    prev_v_pts = Some(pts_ns);
                    last_end = last_end.max(pts_ns + last_v_step);
                    decoded_units += 1;
                }
                MkvBlock::Audio { pts_ns, data } => {
                    let raw: Vec<f32> = data.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
                    let orig_frames = (raw.len() / 2) as i128;
                    if orig_frames == 0 {
                        continue 'blocks;
                    }
                    last_end = last_end.max(pts_ns + orig_frames * 1_000_000_000 / 48_000);
                    let pcm = resampler.process(raw);
                    let out_frames = pcm.len() / 2;
                    if out_frames == 0 {
                        continue 'blocks;
                    }
                    // Smooth 1 ms Matroska PTS rounding into sample-exact monotonic timestamps;
                    // re-anchor whenever the container jumps by >25 ms.
                    let base_audio_at = match audio_cursor_at {
                        Some(cur) if at.abs_diff(cur) <= 25_000_000 => cur,
                        _ => at,
                    };
                    let base_file_pos = (i128::from(from) + pts_ns).max(0) as u64;
                    if let Ok(mut q) = shared.sound.lock() {
                        for (idx, chunk) in pcm.chunks(PIECE * 2).enumerate() {
                            let off_frames = (idx * PIECE) as u64;
                            let piece_at = base_audio_at + off_frames * 1_000_000_000 / 48_000;
                            let media_off = off_frames * u64::from(spec.speed_percent) * 1_000_000_000 / (48_000 * 100);
                            q.push_back((chunk.to_vec(), piece_at, base_file_pos + media_off));
                        }
                    }
                    audio_cursor_at = Some(base_audio_at + (out_frames as u64) * 1_000_000_000 / 48_000);
                    decoded_units += 1;
                }
            }
            if decoded_units == 1
                && let Ok(mut err) = shared.error.lock()
            {
                *err = None;
            }
        }

        // Reap the FFmpeg child on this thread (never on the caller's thread).
        drop(demux);
        let status = if let Some(mut c) = shared.active_child.lock().ok().and_then(|mut slot| slot.take()) {
            if interrupted || stop.load(Ordering::Acquire) {
                let _ = c.kill();
            }
            c.wait().ok()
        } else {
            None
        };
        let stderr_text = err_thread.and_then(|opt| opt.and_then(|t| t.join().ok())).unwrap_or_default();

        if stop.load(Ordering::Acquire) {
            return;
        }
        // Check if a control command interrupted this pass (either caught above or killed our child).
        let pending_cmd =
            interrupted || shared.cmd.lock().map(|st| st.queue.iter().any(|c| *c != Control::Play)).unwrap_or(false);
        if pending_cmd {
            shared.clear_queues();
            follows = None;
            continue 'passes;
        }

        let failed = decoded_units == 0 || !status.is_some_and(|s| s.success());
        if failed && use_hw {
            tracing::info!(file = %spec.path.display(), "hardware video decode failed, falling back to CPU");
            shared.hw.store(false, Ordering::Relaxed);
            shared.clear_queues();
            follows = None;
            continue 'passes;
        }
        if failed {
            let why = if stderr_text.is_empty() {
                "the media stopped being readable".to_string()
            } else {
                stderr_text.lines().last().unwrap_or(&stderr_text).chars().take(300).collect()
            };
            if let Ok(mut err) = shared.error.lock() {
                *err = Some(why);
            }
            if decoded_units == 0 {
                shared.clear_queues();
            }
            reader_state = MediaState::Ended;
            if let Ok(mut st) = shared.cmd.lock()
                && st.queue.is_empty()
            {
                st.ended = true;
                st.paused = false;
                st.stopped = false;
            }
            follows = None;
            continue 'passes;
        }

        if !spec.looping {
            reader_state = MediaState::Ended;
            if let Ok(mut st) = shared.cmd.lock()
                && st.queue.is_empty()
            {
                st.ended = true;
                st.paused = false;
                st.stopped = false;
            }
            follows = None;
            continue 'passes;
        }

        // Seamless loop: next pass always restarts at 0 (even after seeking) anchored at this
        // pass's exact end on the master clock.
        let b_host = base_host_ns.unwrap_or_else(now_ns);
        let pass_end =
            if from == 0 && spec.duration_ns > 0 { i128::from(spec.duration_ns).max(last_end) } else { last_end };
        follows = Some(schedule_at(b_host, base_pts_ns, pass_end, spec.speed_percent));
        from = 0;
    }
}

/// A file playing through one FFmpeg process per pass on a shared PTS timeline.
#[derive(Debug)]
pub struct Media {
    /// Its pictures started on the GPU decoder (see `hw_active()` after runtime fallback).
    pub hw: bool,
    shared: Arc<MediaShared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub has_video: bool,
    pub has_audio: bool,
    pub duration_ns: u64,
    /// True when frames are delivered as NV12 (`frame_nv12`), false when RGBA (`frame_at`).
    is_nv12: bool,
}

impl Media {
    /// Opens `path` and starts playing it (RGBA frames).
    pub fn open(path: &Path, looping: bool) -> Result<Self, AvError> {
        Self::open_fit(path, looping, (1920, 1080))
    }

    /// The same, pictures fitted into `max`.
    pub fn open_fit(path: &Path, looping: bool, max: (u32, u32)) -> Result<Self, AvError> {
        Self::open_as(path, MediaOptions::looping(looping), max, false, false, "auto")
    }

    /// Opens `path` for the card: opaque videos come as NV12 (`frame_nv12`), turned into RGB on
    /// the GPU; videos with alpha keep RGBA (`frame_at`).
    pub fn open_for_card(path: &Path, looping: bool) -> Result<Self, AvError> {
        Self::open_for_card_with(path, MediaOptions::looping(looping), false)
    }

    /// Opens `path` for the card with full playback options; when `start_stopped` is true, stays
    /// idle at 0 until `Control::Play` or `Control::Restart`.
    pub fn open_for_card_with(path: &Path, options: MediaOptions, start_stopped: bool) -> Result<Self, AvError> {
        Self::open_as(path, options, (1920, 1080), true, start_stopped, "auto")
    }

    /// Opens `path` using `MediaOptions` (for `win::media` / `linux::media` unified entry point).
    pub fn open_with(path: &Path, options: MediaOptions, start_stopped: bool) -> Result<Self, AvError> {
        Self::open_for_card_with(path, options, start_stopped)
    }

    fn open_as(
        path: &Path,
        options: MediaOptions,
        max: (u32, u32),
        for_card: bool,
        start_stopped: bool,
        hwaccel: &'static str,
    ) -> Result<Self, AvError> {
        if !path.is_file() {
            return Err(AvError::Source(format!("{} isn't there", path.display())));
        }
        let ff = ffmpeg()?;
        let p = probe(&ff, &["-i".into(), path.display().to_string()], Duration::from_secs(10))
            .ok_or_else(|| AvError::Source(format!("{} couldn't be opened", path.display())))?;
        let has_video = p.size.is_some();
        let has_audio = p.audio;
        if !has_video && !has_audio {
            return Err(AvError::Source(format!("{} has no picture or sound", path.display())));
        }
        let file = ["-i".to_string(), path.display().to_string()];
        // Alpha videos stay on CPU RGBA (hardware decoders and NV12 strip alpha to opaque black).
        let hw = has_video
            && !p.has_alpha
            && options.hw_decode
            && hw_decode_allowed()
            && (hwaccel != "auto" || hw_decodes(&ff, &file));
        if has_video && !hw {
            tracing::info!(file = %path.display(), alpha = p.has_alpha, "the video is decoded on the CPU");
        }
        let size = p.size.map_or(max, |s| fit(s, max));
        // A file that doesn't name its matrix: BT.709 from 720 lines up, BT.601 below.
        // Never force NV12 for sources with an alpha channel (WebM VP9 stingers, ProRes 4444, MOV alpha).
        let nv12 = (for_card && has_video && !p.has_alpha)
            .then(|| (p.bt709.unwrap_or(p.size.is_some_and(|(_, h)| h >= 720)), p.full_range));
        let shared = Arc::new(MediaShared::new(hw, start_stopped));
        let stop = Arc::new(AtomicBool::new(false));
        let spec = PassSpec {
            ffmpeg: ff,
            path: path.to_path_buf(),
            has_video,
            has_audio,
            size,
            nv12,
            video_decoder: p.video_decoder,
            looping: options.looping,
            speed_percent: options.speed(),
            duration_ns: p.duration_ns,
            hwaccel,
        };
        let thread = {
            let (shared, stop) = (shared.clone(), stop.clone());
            std::thread::Builder::new()
                .name("ferrocast-ffsrc-media".into())
                .spawn(move || read_media(spec, start_stopped, shared, stop))
                .map_err(|e| AvError::Source(format!("the media couldn't start ({e})")))?
        };
        Ok(Self {
            hw,
            shared,
            stop,
            thread: Some(thread),
            has_video,
            has_audio,
            duration_ns: p.duration_ns,
            is_nv12: nv12.is_some(),
        })
    }

    /// Whether GPU hardware decoding is currently active (false after runtime CPU fallback).
    pub fn hw_active(&self) -> bool {
        self.shared.hw.load(Ordering::Relaxed)
    }

    /// The newest RGBA picture due at `now` (`0` uses `now_ns()`); `None` in NV12 mode.
    pub fn frame_at(&self, now: u64) -> Option<Picture> {
        if !self.has_video || self.is_nv12 {
            return None;
        }
        let t = if now == 0 { now_ns() } else { now };
        let mut q = self.shared.frames.lock().ok()?;
        let mut chosen = None;
        while q.front().is_some_and(|(_, at, _)| *at <= t) {
            if let Some((FrameData::Rgba(p), _, pos)) = q.pop_front() {
                self.shared.position.store(pos, Ordering::Relaxed);
                chosen = Some(p);
            }
        }
        chosen
    }

    /// The newest NV12 picture due now, when opened for the card (`open_for_card`).
    pub fn frame_nv12(&self) -> Option<Nv12Picture> {
        if !self.has_video || !self.is_nv12 {
            return None;
        }
        let t = now_ns();
        let mut q = self.shared.frames.lock().ok()?;
        let mut chosen = None;
        while q.front().is_some_and(|(_, at, _)| *at <= t) {
            if let Some((FrameData::Nv12(p), _, pos)) = q.pop_front() {
                self.shared.position.store(pos, Ordering::Relaxed);
                chosen = Some(p);
            }
        }
        chosen
    }

    /// Plays, pauses, restarts, stops or seeks without blocking (never waits on or spawns FFmpeg
    /// on the caller's thread).
    pub fn control(&self, c: Control) {
        if let Ok(mut st) = self.shared.cmd.lock() {
            if matches!(c, Control::Seek(_)) && matches!(st.queue.back(), Some(Control::Seek(_))) {
                st.queue.pop_back();
            }
            st.queue.push_back(c);
            match c {
                Control::Play | Control::Restart => {
                    st.paused = false;
                    st.stopped = false;
                    st.ended = false;
                    if matches!(c, Control::Restart) {
                        self.shared.position.store(0, Ordering::Relaxed);
                    }
                }
                Control::Pause => {
                    st.paused = true;
                }
                Control::Stop => {
                    st.paused = true;
                    st.stopped = true;
                    st.ended = false;
                    self.shared.position.store(0, Ordering::Relaxed);
                }
                Control::Seek(ns) => {
                    let clamped = if self.duration_ns > 0 { ns.min(self.duration_ns) } else { ns };
                    self.shared.position.store(clamped, Ordering::Relaxed);
                    if st.ended || st.stopped {
                        st.ended = false;
                        st.stopped = false;
                        st.paused = true;
                    }
                }
            }
            self.shared.wake.notify_all();
        }
        // Interrupt any active blocking read immediately without waiting for process exit.
        if c != Control::Play
            && let Ok(mut slot) = self.shared.active_child.lock()
            && let Some(child) = slot.as_mut()
        {
            let _ = child.kill();
        }
    }

    pub fn state(&self) -> (MediaState, u64) {
        let (ended, stopped, paused) =
            self.shared.cmd.lock().map(|st| (st.ended, st.stopped, st.paused)).unwrap_or((false, false, false));
        let state = if ended && self.shared.finished() {
            MediaState::Ended
        } else if stopped {
            MediaState::Stopped
        } else if paused {
            MediaState::Paused
        } else {
            MediaState::Playing
        };
        (state, self.shared.position.load(Ordering::Relaxed))
    }

    /// Sound due to play within `SOUND_HOLD_NS` (40 ms), on the same PTS clock as the pictures.
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        let pieces =
            self.shared.sound.lock().map(|mut q| take_due_sound(&mut q, now_ns(), SOUND_HOLD_NS)).unwrap_or_default();
        if !self.has_video
            && let Some((_, _, pos)) = pieces.last()
        {
            self.shared.position.store(*pos, Ordering::Relaxed);
        }
        pieces.into_iter().map(|(pcm, at, _)| (pcm, at)).collect()
    }

    pub fn ended(&self) -> bool {
        self.shared.finished()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().ok()?.clone()
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(_st) = self.shared.cmd.lock() {
            self.shared.wake.notify_all();
        }
        if let Ok(mut slot) = self.shared.active_child.lock()
            && let Some(child) = slot.as_mut()
        {
            let _ = child.kill();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_fit_and_stay_even() {
        assert_eq!(fit((2560, 1440), (1920, 1080)), (1920, 1080));
        assert_eq!(fit((1080, 1920), (1920, 1080)), (608, 1080));
        assert_eq!(fit((640, 360), (1920, 1080)), (640, 360), "never made bigger");
        assert_eq!(fit((1, 1), (1920, 1080)), (2, 2));
    }

    #[test]
    fn ffmpegs_description_is_read() {
        let text = "Input #0, gdigrab, from 'desktop':\n  Duration: N/A, start: 1.0, bitrate: 1 kb/s\n  \
            Stream #0:0: Video: bmp, bgra, 2560x1440, 30 fps\nInput #1\n  Duration: 00:01:02.50, start: 0\n  \
            Stream #1:0[0x1](und): Audio: aac (LC), 48000 Hz, stereo";
        let p = parse_probe(text).unwrap_or_else(|| panic!("probe failed"));
        assert_eq!(p.size, Some((2560, 1440)));
        assert!(p.audio);
        assert_eq!(p.duration_ns, 62_500_000_000);
        assert_eq!(p.fps, 30);
        let file = "  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(tv, bt709, progressive), \
            1920x1080 [SAR 1:1 DAR 16:9], 5967 kb/s, 29.97 fps, 29.97 tbr, 30k tbn (default)";
        assert_eq!(parse_probe(file).unwrap_or_else(|| panic!("probe")).fps, 30, "rounded up");
        // Its colours: the pixel format's brackets, not the codec's.
        let tagged = parse_probe(file).unwrap_or_else(|| panic!("probe"));
        assert_eq!((tagged.bt709, tagged.full_range), (Some(true), false));
        let jpeg = parse_probe(
            "  Stream #0:0: Video: mjpeg (Baseline), yuvj420p(pc, bt470bg/unknown/unknown), 1280x720, 30 fps",
        )
        .unwrap_or_else(|| panic!("probe"));
        assert_eq!((jpeg.bt709, jpeg.full_range), (Some(false), true));
        assert_eq!(
            parse_probe("  Stream #0:0: Video: h264 (High), yuv420p(progressive), 1920x1080, 30 fps")
                .unwrap_or_else(|| panic!("probe"))
                .bt709,
            None
        );
        assert_eq!(
            parse_probe("  Stream #0:0: Video: vp9, yuv420p, 640x360, 25 tbr").unwrap_or_else(|| panic!("probe")).fps,
            25
        );
        // dshow's line has a pixel format with brackets before the size.
        let cam =
            "  Stream #0:0: Video: rawvideo (YUY2 / 0x32595559), yuyv422(tv, bt470bg/bt709/unknown), 1280x720, 30 fps";
        assert_eq!(parse_probe(cam).unwrap_or_else(|| panic!("probe")).size, Some((1280, 720)));
        assert!(parse_probe("dummy: I/O error").is_none());
    }

    fn have_ffmpeg() -> bool {
        if let Some(p) = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
            .iter()
            .map(Path::new)
            .find(|p| p.is_file())
        {
            crate::ffenc::set_ffmpeg(p);
            return true;
        }
        // Windows (and anywhere else): one on the PATH.
        crate::ffenc::ffmpeg_path().is_some()
    }

    /// A live input (FFmpeg's test pattern standing in for a screen or camera), fitted, with
    /// its sound.
    #[test]
    fn a_live_input_gives_fitted_pictures_and_sound() {
        if !have_ffmpeg() {
            return;
        }
        let input: Vec<String> =
            ["-re", "-f", "lavfi", "-i", "testsrc=size=1280x720:rate=30"].iter().map(|s| s.to_string()).collect();
        let sound: Vec<String> =
            ["-re", "-f", "lavfi", "-i", "sine=frequency=440"].iter().map(|s| s.to_string()).collect();
        let live = Live::start(&input, (640, 640), 30, Some(&sound)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(live.size, (640, 360));
        std::thread::sleep(Duration::from_millis(1500));
        let (p, t) = live.take().unwrap_or_else(|| panic!("a picture"));
        assert_eq!((p.width, p.height, p.rgba.len()), (640, 360, 640 * 360 * 4));
        assert!(t > 0 && live.frames() >= 10, "{}", live.frames());
        let s = live.take_sound();
        assert!(!s.is_empty() && s[0].0.len() == PIECE * 2);
        assert!(s.iter().flat_map(|(p, _)| p).any(|v| v.abs() > 0.05), "the tone is heard");
        assert!(live.ended().is_none());
    }

    /// Verifies Matroska rawvideo RGBA preserves exact channel order and top-down row orientation
    /// (top half red, bottom half blue).
    #[test]
    fn matroska_rgba_keeps_channel_order_and_top_down_orientation() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ffsrc-orient-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("split.mp4");
        let ok = Command::new(ffmpeg().unwrap_or_else(|e| panic!("{e}")))
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=64x32:r=30:d=1",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=64x32:r=30:d=1",
                "-filter_complex",
                "[0:v][1:v]vstack=inputs=2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&file)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "ffmpeg made split clip");
        let m = Media::open(&file, false).unwrap_or_else(|e| panic!("{e}"));
        let started = Instant::now();
        let mut pic = None;
        while started.elapsed() < Duration::from_secs(2) && pic.is_none() {
            pic = m.frame_at(0);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
        let p = pic.unwrap_or_else(|| panic!("expected a decoded RGBA frame"));
        assert_eq!((p.width, p.height), (64, 64));
        // Top row center (y=8, x=32): red.
        let top = ((8 * 64 + 32) * 4) as usize;
        // Bottom row center (y=56, x=32): blue.
        let bot = ((56 * 64 + 32) * 4) as usize;
        assert!(
            p.rgba[top] > 200 && p.rgba[top + 2] < 50 && p.rgba[top + 3] == 255,
            "top red: {:?}",
            &p.rgba[top..top + 4]
        );
        assert!(
            p.rgba[bot] < 50 && p.rgba[bot + 2] > 200 && p.rgba[bot + 3] == 255,
            "bot blue: {:?}",
            &p.rgba[bot..bot + 4]
        );
    }

    /// A file: plays, pauses where it was, seeks, and ends when it isn't looping.
    #[test]
    fn a_file_plays_pauses_seeks_and_ends() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ffsrc-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("clip.mp4");
        let ok = Command::new(ffmpeg().unwrap_or_else(|e| panic!("{e}")))
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=30:duration=2"])
            .args(["-f", "lavfi", "-i", "sine=duration=2", "-c:v", "libx264", "-c:a", "aac", "-shortest"])
            .arg(&file)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok);
        let m = Media::open(&file, false).unwrap_or_else(|e| panic!("{e}"));
        assert!(m.has_video && m.has_audio && (1_900_000_000..2_200_000_000).contains(&m.duration_ns));
        // The GPU decodes it where it can; a file it can't read never goes that way.
        eprintln!("decoded on the GPU: {}", m.hw);
        assert!(!hw_decodes(
            &ffmpeg().unwrap_or_else(|e| panic!("{e}")),
            &["-i".into(), dir.join("missing.mp4").display().to_string()]
        ));
        let poll = |ms: u64| {
            let end = Instant::now() + Duration::from_millis(ms);
            let (mut got_pic, mut got_snd) = (false, false);
            while Instant::now() < end {
                if m.frame_at(0).is_some() {
                    got_pic = true;
                }
                if !m.take_sound().is_empty() {
                    got_snd = true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            (got_pic, got_snd)
        };
        let (got_pic, got_snd) = poll(750);
        assert!(got_pic, "pictures come");
        assert!(got_snd, "sound comes");
        m.control(Control::Pause);
        let (s, at) = m.state();
        assert_eq!(s, MediaState::Paused);
        assert!(at > 300_000_000, "paused where it was: {at}");
        m.control(Control::Seek(1_500_000_000));
        assert_eq!(m.state().1, 1_500_000_000);
        m.control(Control::Play);
        // Played as the engine plays it: pictures and sound taken as they come.
        let until = Instant::now() + Duration::from_millis(2000);
        while Instant::now() < until && !m.ended() {
            let _ = m.frame_at(0);
            let _ = m.take_sound();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(m.state().0, MediaState::Ended, "played to the end");
        assert!(m.ended());

        // For the card: NV12 pictures (no RGBA), the file's own matrix, sound on the same clock.
        let card = Media::open_for_card(&file, true).unwrap_or_else(|e| panic!("{e}"));
        let mut p = None;
        let mut sound = Vec::new();
        let until = Instant::now() + Duration::from_millis(1000);
        while Instant::now() < until && (p.is_none() || sound.len() < 4) {
            if let Some(f) = card.frame_nv12() {
                p = Some(f);
            }
            sound.extend(card.take_sound());
            std::thread::sleep(Duration::from_millis(10));
        }
        let p = p.unwrap_or_else(|| panic!("NV12 pictures come"));
        assert_eq!((p.width, p.height, p.data.len()), (320, 180, 320 * 180 * 3 / 2));
        assert!(!p.bt709, "an untagged 180-line picture is read as BT.601");
        assert!(card.frame_at(0).is_none(), "no RGBA made");
        assert!(!sound.is_empty(), "sound comes on the pictures' clock");
        assert!(sound.windows(2).all(|w| w[1].1 > w[0].1), "in order on the pictures' clock");
        drop(card);
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 29.97 fps clip with a 5 s GOP: seeking to 2.5 s must (1) return immediately without
    /// blocking the caller, (2) coalesce rapid scrubber drags, and (3) keep video and audio in
    /// sync within 50 ms (M8-04).
    #[test]
    fn seek_on_29_97_long_gop_stays_in_sync_and_never_blocks() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ffsrc-2997-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("clip2997.mp4");
        // Flash white and beep at t=3.0s..3.2s inside a 29.97 fps 5 s GOP clip (one keyframe at 0s).
        let ok = Command::new(ffmpeg().unwrap_or_else(|e| panic!("{e}")))
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=160x90:r=30000/1001:d=5,drawbox=c=white:t=fill:enable='between(t\\,3.0\\,3.2)'")
            .args(["-f", "lavfi", "-i"])
            .arg("aevalsrc='if(between(t\\,3.0\\,3.2)\\,0.6*sin(2*PI*1000*t)\\,0)':s=48000:c=stereo:d=5")
            .args(["-c:v", "libx264", "-g", "150", "-keyint_min", "150", "-sc_threshold", "0", "-pix_fmt", "yuv420p"])
            .args(["-c:a", "aac", "-shortest"])
            .arg(&file)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "ffmpeg generated 29.97 long-GOP clip");

        let m = Media::open(&file, false).unwrap_or_else(|e| panic!("{e}"));
        std::thread::sleep(Duration::from_millis(250));
        // Rapid scrubber drag: 20 seeks must complete immediately without blocking the State lock.
        let drag_start = Instant::now();
        for i in 0..20u64 {
            m.control(Control::Seek(i * 100_000_000));
        }
        m.control(Control::Seek(2_500_000_000));
        let drag_ms = drag_start.elapsed().as_millis();
        assert!(drag_ms < 50, "21 control(Seek) calls must not block: took {drag_ms} ms");

        // Watch for the 3.0 s flash and 3.0 s beep after seeking to 2.5 s.
        let (mut flash_at, mut beep_at): (Option<u64>, Option<u64>) = (None, None);
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until && (flash_at.is_none() || beep_at.is_none()) {
            let now = now_ns();
            if let Some(p) = m.frame_at(now)
                && flash_at.is_none()
                && p.rgba.first().copied().unwrap_or(0) > 200
            {
                flash_at = Some(now);
            }
            for (pcm, at) in m.take_sound() {
                if beep_at.is_none() && pcm.iter().any(|s| s.abs() > 0.2) {
                    beep_at = Some(at);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
        let (f, b) = (
            flash_at.unwrap_or_else(|| panic!("expected white flash at 3.0 s after seek to 2.5 s")),
            beep_at.unwrap_or_else(|| panic!("expected 1 kHz beep at 3.0 s after seek to 2.5 s")),
        );
        let diff_ms = f.abs_diff(b) / 1_000_000;
        assert!(diff_ms <= 65, "video flash and audio beep after long-GOP seek drifted by {diff_ms} ms");
    }

    /// A variable-frame-rate (VFR) clip plays using true container PTS without stalling or
    /// mis-timing frames (M8-04).
    #[test]
    fn vfr_clip_plays_with_true_pts() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ffsrc-vfr-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("vfr.mkv");
        // Drop every other frame for the first half via mpdecimate / variable setpts so PTS gaps vary.
        let ok = Command::new(ffmpeg().unwrap_or_else(|e| panic!("{e}")))
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x90:rate=30:duration=1.5"])
            .args(["-vf", "setpts='if(lt(N,15),N*0.06/TB,(15*0.06+(N-15)*0.02)/TB)'", "-fps_mode", "vfr"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&file)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "ffmpeg generated VFR clip");

        let m = Media::open_for_card(&file, false).unwrap_or_else(|e| panic!("{e}"));
        let started = Instant::now();
        let mut frames = 0u32;
        while !m.ended() && started.elapsed() < Duration::from_secs(5) {
            if m.frame_nv12().is_some() {
                frames += 1;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        let ended = m.ended();
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ended, "VFR clip played cleanly to EOF");
        assert!(frames >= 30, "expected most of the ~45 VFR frames, got {frames}");
    }

    /// A looping file keeps its sound after looping (even when audio is shorter than video) AND
    /// loops back to 0 after a seek rather than re-seeking to the seek offset (M8-04).
    #[test]
    fn a_looping_file_keeps_its_sound() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ffsrc-loop-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("loop.mp4");
        // Its sound is shorter than its picture, as many clips' are.
        let ok = Command::new(ffmpeg().unwrap_or_else(|e| panic!("{e}")))
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=30:duration=1"])
            .args(["-f", "lavfi", "-i", "sine=duration=0.8", "-map", "0:v", "-map", "1:a"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&file)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok);
        let m = Media::open_for_card(&file, true).unwrap_or_else(|e| panic!("{e}"));
        // Seek to 0.7 s first: after reaching 1.0 s it must wrap to 0.0 s (not 0.7 s!).
        m.control(Control::Seek(700_000_000));
        let start = Instant::now();
        let (mut late, mut wrapped_to_zero) = (0usize, false);
        while start.elapsed() < Duration::from_millis(4500) {
            let _ = m.frame_nv12();
            let got = m.take_sound().len();
            if start.elapsed() > Duration::from_millis(600) && m.state().1 < 400_000_000 {
                wrapped_to_zero = true;
            }
            if start.elapsed() > Duration::from_millis(3500) {
                late += got;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(wrapped_to_zero, "looping clip must restart at 0 after a seek, not re-seek to 0.7 s");
        assert!(late > 25, "sound after it looped: {late} pieces in its last second");
    }

    /// If hardware decode fails at runtime, ffsrc automatically retries on CPU and produces frames
    /// instead of black-screening permanently (M8-04).
    #[test]
    fn hw_decode_failure_falls_back_to_software() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ffsrc-hwfb-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("clip.mp4");
        let ok = Command::new(ffmpeg().unwrap_or_else(|e| panic!("{e}")))
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x90:rate=30:duration=1"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&file)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok);
        // Force a non-existent hwaccel name so the first pass fails immediately like an exhausted NVDEC session.
        let m = Media::open_as(&file, MediaOptions::default(), (1920, 1080), true, false, "ferrocast_bogus_hw")
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(m.hw, "started attempting hardware decode");
        let started = Instant::now();
        let mut got_frame = false;
        while started.elapsed() < Duration::from_secs(3) && !got_frame {
            if m.frame_nv12().is_some() {
                got_frame = true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let hw_after = m.hw_active();
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(got_frame, "CPU fallback must deliver frames after HW failure");
        assert!(!hw_after, "hw_active flips false after fallback");
    }

    /// A file's sound reaches the mixer when it's about to play, not up to 0.4 s before: sound the
    /// mixer already holds plays on after a pause, a seek or a cut to another scene.
    #[test]
    fn a_files_sound_is_handed_over_when_due() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ffsrc-due-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("clip.mp4");
        let ok = Command::new(ffmpeg().unwrap_or_else(|e| panic!("{e}")))
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=30:duration=3"])
            .args(["-f", "lavfi", "-i", "sine=duration=3", "-c:v", "libx264", "-c:a", "aac", "-shortest"])
            .arg(&file)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok);
        let m = Media::open_for_card(&file, false).unwrap_or_else(|e| panic!("{e}"));
        let started = Instant::now();
        let (mut most_ahead, mut pieces) = (0u64, 0usize);
        while started.elapsed() < Duration::from_millis(2000) {
            let _ = m.frame_nv12();
            let now = now_ns();
            for (_, at) in m.take_sound() {
                pieces += 1;
                most_ahead = most_ahead.max(at.saturating_sub(now));
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(pieces > 20, "sound came: {pieces} pieces");
        assert!(most_ahead <= 40_000_000 + 5_000_000, "sound came {} ms before it plays", most_ahead / 1_000_000);
    }

    /// Pieces that play later than the hold wait; everything due comes out in order.
    #[test]
    fn sound_is_taken_when_due_not_300_ms_ahead() {
        let ms = |m: u64| m * 1_000_000;
        let mut q: VecDeque<_> = (0..10u64).map(|i| (vec![0.0f32; 2048], ms(1_000 + i * 21), i * 21)).collect();
        let got = take_due_sound(&mut q, ms(1_000), SOUND_HOLD_NS);
        let times: Vec<u64> = got.iter().map(|p| p.1 / 1_000_000).collect();
        assert_eq!(times, vec![1_000, 1_021]);
        assert_eq!(q.len(), 8, "the rest waits");
        q.clear();
        assert!(take_due_sound(&mut q, ms(9_000), SOUND_HOLD_NS).is_empty());
    }

    /// A file is over only when decoding finished AND all queued frames and sound have been taken.
    #[test]
    fn a_file_is_over_only_when_decoded_data_has_been_taken() {
        let sh = MediaShared::new(false, false);
        assert!(!sh.finished(), "still reading");
        sh.cmd.lock().unwrap_or_else(|e| e.into_inner()).ended = true;
        assert!(sh.finished(), "ended with empty queues");
        sh.sound.lock().unwrap_or_else(|e| e.into_inner()).push_back((vec![0.0; 2048], 1, 0));
        assert!(!sh.finished(), "sound still waiting");
        sh.sound.lock().unwrap_or_else(|e| e.into_inner()).clear();
        sh.frames.lock().unwrap_or_else(|e| e.into_inner()).push_back((
            FrameData::Rgba(Picture::solid(2, 2, [0, 0, 0, 255])),
            1,
            0,
        ));
        assert!(!sh.finished(), "frame still waiting");
        sh.frames.lock().unwrap_or_else(|e| e.into_inner()).clear();
        assert!(sh.finished());
    }

    #[test]
    fn probe_detects_alpha_formats_and_vp9_alpha_mode() {
        let prores =
            "  Stream #0:0: Video: prores (4444) (ap4h / 0x68347061), yuva444p12le(tv, bt709), 1920x1080, 30 fps";
        let p = parse_probe(prores).unwrap_or_else(|| panic!("prores probe"));
        assert!(p.has_alpha && p.video_decoder.is_none());

        let vp9_alpha = "Input #0, matroska,webm, from 'stinger.webm':\n  Duration: 00:00:01.00, start: 0.000\n  \
            Stream #0:0(eng): Video: vp9 (Profile 0), yuv420p(tv), 1920x1080, 60 fps\n    Metadata:\n      \
            alpha_mode      : 1\n  Stream #0:1: Audio: opus, 48000 Hz, stereo";
        let p = parse_probe(vp9_alpha).unwrap_or_else(|| panic!("vp9 alpha probe"));
        assert!(p.has_alpha, "VP9 alpha_mode: 1 must flag has_alpha");
        assert_eq!(p.video_decoder, Some("libvpx-vp9"));
        assert!(p.audio);

        let vp8_alpha = "  Stream #0:0: Video: vp8, yuv420p, 320x180, 30 fps\n    Metadata:\n      alpha_mode : 1";
        let p = parse_probe(vp8_alpha).unwrap_or_else(|| panic!("vp8 alpha probe"));
        assert!(p.has_alpha && p.video_decoder == Some("libvpx"));

        let opaque = "  Stream #0:0: Video: h264 (High), yuv420p(tv, bt709), 1920x1080, 30 fps";
        let p = parse_probe(opaque).unwrap_or_else(|| panic!("opaque probe"));
        assert!(!p.has_alpha && p.video_decoder.is_none());
    }

    /// On Windows/FFmpeg, opening a transparent VP9 WebM stinger via `open_for_card` must keep
    /// RGBA (not NV12) and decode with `libvpx-vp9` so alpha=0 pixels stay transparent (M8-01).
    #[test]
    fn webm_vp9_with_alpha_preserves_transparency_on_card_path() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ffsrc-alpha-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("stinger.webm");
        let ok = Command::new(ffmpeg().unwrap_or_else(|e| panic!("{e}")))
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black@0.0:s=32x32:r=15:d=1,format=rgba",
                "-f",
                "lavfi",
                "-i",
                "color=c=0x00ff00@1.0:s=32x32:r=15:d=1,format=rgba",
                "-filter_complex",
                "[0:v][1:v]hstack=inputs=2,format=yuva420p",
                "-c:v",
                "libvpx-vp9",
                "-pix_fmt",
                "yuva420p",
                "-auto-alt-ref",
                "0",
                "-deadline",
                "realtime",
            ])
            .arg(&file)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let m = Media::open_for_card(&file, false).unwrap_or_else(|e| panic!("{e}"));
        let started = Instant::now();
        let mut pic = None;
        while started.elapsed() < Duration::from_secs(3) && pic.is_none() {
            assert!(m.frame_nv12().is_none(), "alpha clip must not be downgraded to NV12");
            pic = m.frame_at(0);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
        let p = pic.unwrap_or_else(|| panic!("expected RGBA frame from VP9+alpha WebM"));
        assert_eq!((p.width, p.height), (64, 32));
        // Left half (x=8, y=16): transparent (alpha == 0).
        let left = ((16 * 64 + 8) * 4) as usize;
        // Right half (x=48, y=16): opaque green (alpha == 255, g > 200).
        let right = ((16 * 64 + 48) * 4) as usize;
        assert_eq!(p.rgba[left + 3], 0, "left pixel must stay transparent: {:?}", &p.rgba[left..left + 4]);
        assert!(
            p.rgba[right + 3] > 240 && p.rgba[right + 1] > 180,
            "right pixel must stay opaque green: {:?}",
            &p.rgba[right..right + 4]
        );
    }
}
