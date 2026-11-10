//! Sources read through FFmpeg, where the system has no capture API of ours yet (Windows:
//! screens with `ddagrab`/`gdigrab`, cameras with `dshow`, and video files): FFmpeg reads the
//! input and hands over RGBA pictures (fitted into the canvas) and 48 kHz stereo sound on its
//! output, which a thread keeps for the engine, stamped with the engine's clock as they arrive.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::engine::now_ns;
use crate::{AvError, Picture};

/// Sound kept before it's taken (about 5 s of 1024-frame pieces).
const MAX_SOUND: usize = 240;
/// Frames of sound per piece.
const PIECE: usize = 1024;
/// Pictures a file shows before its sound is placed on their clock: the first few come in a
/// burst, so the clock (the earliest start they give) settles by then.
const SETTLE_FRAMES: u64 = 8;

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

/// The probe's facts from FFmpeg's text (the first video and audio stream).
pub fn parse_probe(text: &str) -> Option<Probe> {
    let mut p = Probe::default();
    let mut any = false;
    for line in text.lines() {
        let l = line.trim();
        if let Some(d) = l.strip_prefix("Duration: ").and_then(|r| r.split(',').next()) {
            let parts: Vec<f64> = d.split(':').filter_map(|x| x.trim().parse().ok()).collect();
            if parts.len() == 3 {
                p.duration_ns = ((parts[0] * 3600.0 + parts[1] * 60.0 + parts[2]) * 1e9) as u64;
            }
        }
        if !l.starts_with("Stream #") {
            continue;
        }
        if l.contains("Video:") && p.size.is_none() {
            any = true;
            // The first "WxH" token after "Video:" (FFmpeg writes it as one).
            p.size = l.split("Video:").nth(1).and_then(|rest| {
                rest.split([' ', ',']).find_map(|tok| {
                    let (w, h) = tok.split_once('x')?;
                    Some((w.parse().ok()?, h.trim_end_matches(']').parse().ok()?))
                })
            });
            // "…, 29.97 fps, 30 tbr, …": its own rate (else the guessed one, tbr).
            let rate =
                |unit: &str| l.split(", ").find_map(|part| part.trim().strip_suffix(unit)?.trim().parse::<f64>().ok());
            p.fps = rate(" fps").or_else(|| rate(" tbr")).map_or(0, |f| f.ceil().clamp(0.0, 240.0) as u32);
            // "yuv420p(tv, bt709, progressive)", "yuvj420p(pc, bt470bg/bt709/unknown)": the range,
            // then the matrix (the first of matrix/primaries/transfer when they differ).
            let after = l.split("Video:").nth(1).unwrap_or_default();
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
    /// The newest NV12 picture (`Out::Nv12`), for the card.
    nv12: Mutex<Option<Nv12Picture>>,
    /// Sound as it came: its samples, when it came (ns), and how many samples came before it.
    sound: Mutex<std::collections::VecDeque<(Vec<f32>, u64, u64)>>,
    /// Pictures: when the input's start would have shown, by the earliest picture so far (each
    /// picture's arrival less its place, `-r` making it frame n at n/fps). A file's sound is
    /// placed on this clock (`Media::take_sound`).
    start_ns: Mutex<Option<u64>>,
    frames: AtomicU64,
    stop: AtomicBool,
    ended: AtomicBool,
    error: Mutex<Option<String>>,
}

/// One FFmpeg reading an input; dropping it stops FFmpeg.
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

/// What a reader hands over.
#[derive(Debug, Clone, Copy)]
enum Out {
    /// RGBA pictures this size.
    Pictures(u32, u32),
    /// NV12 pictures this size (even), and their colours' matrix and range, for the card.
    Nv12(u32, u32, bool, bool),
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
            // As decoded (the GPU decoder's frames come down as NV12 already): sized, no
            // colour change; the card turns it into RGB.
            Out::Nv12(w, h, ..) => {
                let vf = format!("scale={w}:{h}:flags=bilinear,format=nv12");
                cmd.args(["-an", "-vf", &vf, "-r", &fps.max(1).to_string(), "-f", "rawvideo", "-pix_fmt", "nv12", "-"]);
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
                    Out::Pictures(w, h) | Out::Nv12(w, h, ..) => {
                        let nv12 = match out {
                            Out::Nv12(_, _, bt709, full_range) => Some((bt709, full_range)),
                            _ => None,
                        };
                        let size = if nv12.is_some() { w * h * 3 / 2 } else { w * h * 4 };
                        let mut buf = vec![0u8; size as usize];
                        let frame_ns = 1_000_000_000 / u64::from(fps.max(1));
                        while !s.stop.load(Ordering::Acquire) && stdout.read_exact(&mut buf).is_ok() {
                            let at = now_ns();
                            match nv12 {
                                Some((bt709, full_range)) => {
                                    let p = Nv12Picture { width: w, height: h, data: buf.clone(), bt709, full_range };
                                    if let Ok(mut l) = s.nv12.lock() {
                                        *l = Some(p);
                                    }
                                }
                                None => {
                                    let p = Picture { width: w, height: h, rgba: buf.clone(), premultiplied: false };
                                    if let Ok(mut l) = s.picture.lock() {
                                        *l = Some((p, at));
                                    }
                                }
                            }
                            let n = s.frames.fetch_add(1, Ordering::Relaxed);
                            let start = at.saturating_sub(n * frame_ns);
                            if let Ok(mut st) = s.start_ns.lock() {
                                *st = Some(st.map_or(start, |was| was.min(start)));
                            }
                        }
                    }
                    Out::Sound => {
                        let mut buf = vec![0u8; PIECE * 2 * 4];
                        let mut before = 0u64;
                        while !s.stop.load(Ordering::Acquire) && stdout.read_exact(&mut buf).is_ok() {
                            let pcm: Vec<f32> = buf.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
                            let pos = before;
                            before += PIECE as u64;
                            if let Ok(mut q) = s.sound.lock() {
                                q.push_back((pcm, now_ns(), pos));
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
                for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
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
        self.0.shared.sound.lock().map(|mut q| q.drain(..).map(|(pcm, at, _)| (pcm, at)).collect()).unwrap_or_default()
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
            .and_then(|s| s.shared.sound.lock().ok().map(|mut q| q.drain(..).map(|(pcm, at, _)| (pcm, at)).collect()))
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

/// A file's readers now: pictures (or the sound alone), sound, since when and from where (ns).
type Run = (Reader, Option<Reader>, Instant, u64);

/// A file playing through FFmpeg (in real time, its sound beside it).
#[derive(Debug)]
pub struct Media {
    path: PathBuf,
    looping: bool,
    max: (u32, u32),
    /// Its pictures are decoded on the GPU.
    pub hw: bool,
    run: Mutex<Option<Run>>,
    state: Mutex<(MediaState, u64)>,
    pub has_video: bool,
    pub has_audio: bool,
    pub duration_ns: u64,
    /// Pictures a second FFmpeg hands over: the file's own, at most 60.
    fps: u32,
    /// Pictures as NV12 for the card (`frame_nv12`) with this matrix (BT.709?) and range (full?),
    /// else RGBA (`frame_at`).
    nv12: Option<(bool, bool)>,
}

impl Media {
    /// Opens `path` and starts playing it.
    pub fn open(path: &Path, looping: bool) -> Result<Self, AvError> {
        Self::open_fit(path, looping, (1920, 1080))
    }

    /// The same, pictures fitted into `max`.
    pub fn open_fit(path: &Path, looping: bool, max: (u32, u32)) -> Result<Self, AvError> {
        Self::open_as(path, looping, max, false)
    }

    /// Opens `path` for the card: its pictures come as NV12 (`frame_nv12`), turned into RGB there,
    /// not on the CPU. FFmpeg making RGBA (and 4 bytes a pixel through the pipe) was most of a
    /// video file's CPU on Windows (10 Nov: ~10% of the machine for one 1080p30 clip).
    pub fn open_for_card(path: &Path, looping: bool) -> Result<Self, AvError> {
        Self::open_as(path, looping, (1920, 1080), true)
    }

    fn open_as(path: &Path, looping: bool, max: (u32, u32), for_card: bool) -> Result<Self, AvError> {
        if !path.is_file() {
            return Err(AvError::Source(format!("{} isn't there", path.display())));
        }
        let ff = ffmpeg()?;
        let p = probe(&ff, &["-i".into(), path.display().to_string()], Duration::from_secs(10))
            .ok_or_else(|| AvError::Source(format!("{} couldn't be opened", path.display())))?;
        let file = ["-i".to_string(), path.display().to_string()];
        let hw = p.size.is_some() && hw_decode_allowed() && hw_decodes(&ff, &file);
        if p.size.is_some() && !hw {
            tracing::info!(file = %path.display(), "the video is decoded on the CPU");
        }
        let m = Media {
            path: path.to_path_buf(),
            looping,
            max: p.size.map_or(max, |s| fit(s, max)),
            hw,
            run: Mutex::new(None),
            state: Mutex::new((MediaState::Playing, 0)),
            has_video: p.size.is_some(),
            has_audio: p.audio,
            duration_ns: p.duration_ns,
            // Its own rate, not 60 for every file: a 30 fps 1080p clip sent 60 RGBA pictures a
            // second (500 MB/s through the pipe), FFmpeg fell behind and the clip ended half a
            // minute late, with the CPU busy all the while (10 Nov, Windows).
            fps: if p.fps == 0 { 30 } else { p.fps.min(60) },
            // A file that doesn't name its matrix: BT.709 from 720 lines up, BT.601 below, as
            // players read one.
            nv12: for_card.then(|| (p.bt709.unwrap_or(p.size.is_some_and(|(_, h)| h >= 720)), p.full_range)),
        };
        m.play_from(0)?;
        Ok(m)
    }

    fn input(&self, from_ns: u64) -> Vec<String> {
        let mut a: Vec<String> = vec!["-re".into()];
        if self.hw {
            a.extend(["-hwaccel".into(), "auto".into()]);
        }
        if self.looping {
            a.extend(["-stream_loop".into(), "-1".into()]);
        }
        if from_ns > 0 {
            a.extend(["-ss".into(), format!("{:.3}", from_ns as f64 / 1e9)]);
        }
        a.extend(["-i".into(), self.path.display().to_string()]);
        a
    }

    fn play_from(&self, from_ns: u64) -> Result<(), AvError> {
        let ff = ffmpeg()?;
        let input = self.input(from_ns);
        let pictures = if self.has_video {
            let out = match self.nv12 {
                Some((bt709, full)) => Out::Nv12(self.max.0, self.max.1, bt709, full),
                None => Out::Pictures(self.max.0, self.max.1),
            };
            Reader::start(&ff, &input, out, self.fps, Arc::new(Shared::default()))?
        } else {
            // No picture: the sound's reader stands in (it ends when the file does).
            Reader::start(&ff, &input, Out::Sound, 60, Arc::new(Shared::default()))?
        };
        let sound = if self.has_audio && self.has_video {
            Some(Reader::start(&ff, &input, Out::Sound, 60, Arc::new(Shared::default()))?)
        } else {
            None
        };
        if let Ok(mut r) = self.run.lock() {
            *r = Some((pictures, sound, Instant::now(), from_ns));
        }
        if let Ok(mut s) = self.state.lock() {
            *s = (MediaState::Playing, from_ns);
        }
        Ok(())
    }

    fn halt(&self, state: MediaState, at: u64) {
        if let Ok(mut r) = self.run.lock() {
            r.take();
        }
        if let Ok(mut s) = self.state.lock() {
            *s = (state, at);
        }
    }

    fn position(&self) -> u64 {
        let r = self.run.lock();
        match r.as_ref().ok().and_then(|r| r.as_ref()) {
            Some((_, _, since, from)) => {
                let p = from + since.elapsed().as_nanos() as u64;
                match (self.duration_ns, self.looping) {
                    (0, _) => p,
                    (d, true) => p % d,
                    (d, false) => p.min(d),
                }
            }
            None => self.state.lock().map(|s| s.1).unwrap_or(0),
        }
    }

    /// The newest picture (the engine's clock isn't needed: FFmpeg plays in real time).
    pub fn frame_at(&self, _now_ns: u64) -> Option<Picture> {
        let r = self.run.lock().ok()?;
        let (p, _, _, _) = r.as_ref()?;
        if !self.has_video {
            return None;
        }
        p.shared.picture.lock().ok()?.take().map(|(pic, _)| pic)
    }

    /// The newest picture as NV12 (taken), when opened for the card (`open_for_card`).
    pub fn frame_nv12(&self) -> Option<Nv12Picture> {
        let r = self.run.lock().ok()?;
        let (p, _, _, _) = r.as_ref()?;
        if !self.has_video {
            return None;
        }
        p.shared.nv12.lock().ok()?.take()
    }

    pub fn control(&self, c: Control) {
        match c {
            Control::Play => {
                let (s, at) = self.state();
                if s != MediaState::Playing {
                    let from = if s == MediaState::Ended { 0 } else { at };
                    let _ = self.play_from(from);
                }
            }
            Control::Pause => {
                let at = self.position();
                self.halt(MediaState::Paused, at);
            }
            Control::Restart => {
                let _ = self.play_from(0);
            }
            Control::Stop => self.halt(MediaState::Stopped, 0),
            Control::Seek(ns) => {
                let ns = if self.duration_ns > 0 { ns.min(self.duration_ns) } else { ns };
                if self.state().0 == MediaState::Playing {
                    let _ = self.play_from(ns);
                } else if let Ok(mut s) = self.state.lock() {
                    s.1 = ns;
                }
            }
        }
    }

    pub fn state(&self) -> (MediaState, u64) {
        let ended =
            self.run.lock().ok().and_then(|r| r.as_ref().map(|(p, _, _, _)| p.shared.ended.load(Ordering::Acquire)));
        if ended == Some(true) && !self.looping {
            self.halt(MediaState::Ended, self.duration_ns);
        }
        let s = self.state.lock().map(|s| s.0).unwrap_or(MediaState::Stopped);
        (s, self.position())
    }

    /// The file's sound since the last call, each piece placed where it is in the file on the
    /// pictures' clock. Stamped when it came, it played 0.35-0.45 s before its picture (10 Nov,
    /// Windows: FFmpeg hands sound over that far ahead of time, pictures about on time). Held
    /// until a few pictures have set the clock; a file with no picture keeps when it came.
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        let r = self.run.lock();
        let Some((p, s, _, _)) = r.as_ref().ok().and_then(|r| r.as_ref()) else { return Vec::new() };
        let Some(sound) = s.as_ref() else {
            return p
                .shared
                .sound
                .lock()
                .map(|mut q| q.drain(..).map(|(pcm, at, _)| (pcm, at)).collect())
                .unwrap_or_default();
        };
        if p.shared.frames.load(Ordering::Relaxed) < SETTLE_FRAMES {
            return Vec::new();
        }
        let Some(start) = p.shared.start_ns.lock().ok().and_then(|s| *s) else { return Vec::new() };
        // Sound for a moment already shown is let go (the first pieces, held while the clock
        // settled): handed to the mixer late, it would hold every sound back that much for good.
        let floor = now_ns().saturating_sub(20_000_000);
        sound
            .shared
            .sound
            .lock()
            .map(|mut q| {
                q.drain(..)
                    .map(|(pcm, _, pos)| (pcm, start + pos * 1_000_000_000 / 48_000))
                    .filter(|(_, ts)| *ts >= floor)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn ended(&self) -> bool {
        self.state().0 == MediaState::Ended
    }

    pub fn error(&self) -> Option<String> {
        let r = self.run.lock().ok()?;
        let (p, _, _, _) = r.as_ref()?;
        p.shared.error.lock().ok()?.clone()
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
        let p = parse_probe(text).unwrap();
        assert_eq!(p.size, Some((2560, 1440)));
        assert!(p.audio);
        assert_eq!(p.duration_ns, 62_500_000_000);
        assert_eq!(p.fps, 30);
        let file = "  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(tv, bt709, progressive), \
            1920x1080 [SAR 1:1 DAR 16:9], 5967 kb/s, 29.97 fps, 29.97 tbr, 30k tbn (default)";
        assert_eq!(parse_probe(file).unwrap().fps, 30, "rounded up");
        // Its colours: the pixel format's brackets, not the codec's.
        let tagged = parse_probe(file).unwrap();
        assert_eq!((tagged.bt709, tagged.full_range), (Some(true), false));
        let jpeg = parse_probe(
            "  Stream #0:0: Video: mjpeg (Baseline), yuvj420p(pc, bt470bg/unknown/unknown), 1280x720, 30 fps",
        )
        .unwrap();
        assert_eq!((jpeg.bt709, jpeg.full_range), (Some(false), true));
        assert_eq!(
            parse_probe("  Stream #0:0: Video: h264 (High), yuv420p(progressive), 1920x1080, 30 fps").unwrap().bt709,
            None
        );
        assert_eq!(parse_probe("  Stream #0:0: Video: vp9, yuv420p, 640x360, 25 tbr").unwrap().fps, 25);
        // dshow's line has a pixel format with brackets before the size.
        let cam =
            "  Stream #0:0: Video: rawvideo (YUY2 / 0x32595559), yuyv422(tv, bt470bg/bt709/unknown), 1280x720, 30 fps";
        assert_eq!(parse_probe(cam).unwrap().size, Some((1280, 720)));
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
        // Windows (and anywhere else): one on the PATH. These never ran on Windows before.
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
        let live = Live::start(&input, (640, 640), 30, Some(&sound)).unwrap();
        assert_eq!(live.size, (640, 360));
        std::thread::sleep(Duration::from_millis(1500));
        let (p, t) = live.take().expect("a picture");
        assert_eq!((p.width, p.height, p.rgba.len()), (640, 360, 640 * 360 * 4));
        assert!(t > 0 && live.frames() >= 10, "{}", live.frames());
        let s = live.take_sound();
        assert!(!s.is_empty() && s[0].0.len() == PIECE * 2);
        assert!(s.iter().flat_map(|(p, _)| p).any(|v| v.abs() > 0.05), "the tone is heard");
        assert!(live.ended().is_none());
    }

    /// A file: plays, pauses where it was, seeks, and ends when it isn't looping.
    #[test]
    fn a_file_plays_pauses_seeks_and_ends() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ffsrc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mp4");
        let ok = Command::new(ffmpeg().unwrap())
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=30:duration=2"])
            .args(["-f", "lavfi", "-i", "sine=duration=2", "-c:v", "libx264", "-c:a", "aac", "-shortest"])
            .arg(&file)
            .status()
            .unwrap();
        assert!(ok.success());
        let m = Media::open(&file, false).unwrap();
        assert!(m.has_video && m.has_audio && (1_900_000_000..2_200_000_000).contains(&m.duration_ns));
        // The GPU decodes it where it can; a file it can't read never goes that way.
        eprintln!("decoded on the GPU: {}", m.hw);
        assert!(!hw_decodes(&ffmpeg().unwrap(), &["-i".into(), dir.join("missing.mp4").display().to_string()]));
        std::thread::sleep(Duration::from_millis(600));
        assert!(m.frame_at(0).is_some(), "pictures come");
        assert!(!m.take_sound().is_empty(), "sound comes");
        m.control(Control::Pause);
        let (s, at) = m.state();
        assert_eq!(s, MediaState::Paused);
        assert!(at > 300_000_000, "paused where it was: {at}");
        m.control(Control::Seek(1_500_000_000));
        assert_eq!(m.state().1, 1_500_000_000);
        m.control(Control::Play);
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(m.state().0, MediaState::Ended, "played to the end");
        assert!(m.ended());

        // For the card: NV12 pictures (no RGBA), the file's own matrix, sound placed on their clock.
        let card = Media::open_for_card(&file, true).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let p = card.frame_nv12().unwrap_or_else(|| panic!("NV12 pictures come"));
        assert_eq!((p.width, p.height, p.data.len()), (320, 180, 320 * 180 * 3 / 2));
        assert!(!p.bt709, "an untagged 180-line picture is read as BT.601");
        assert!(card.frame_at(0).is_none(), "no RGBA made");
        let sound = card.take_sound();
        assert!(!sound.is_empty(), "sound comes once the pictures set the clock");
        assert!(sound.windows(2).all(|w| w[1].1 > w[0].1), "in order on the pictures' clock");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
