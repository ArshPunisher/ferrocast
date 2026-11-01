//! Encoding through FFmpeg, for systems without a native encoder of ours yet (Windows, Linux)
//! and as a software backup anywhere: raw frames or sound go into an FFmpeg process, FLV comes
//! back out (our own stream format), and its tags become our packets. FFmpeg reaches the GPUs'
//! encoders (NVENC, Quick Sync, AMF, Media Foundation, VideoToolbox) and x264 behind them; the
//! first that works here is used. Ferrocast already ships FFmpeg for Go live.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
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

/// The H.264 encoders tried, best first, on this system.
fn candidates() -> &'static [&'static str] {
    if cfg!(windows) {
        &["h264_nvenc", "h264_qsv", "h264_amf", "h264_mf", "libx264"]
    } else if cfg!(target_os = "macos") {
        &["h264_videotoolbox", "libx264"]
    } else {
        &["h264_nvenc", "libx264"]
    }
}

/// What an encoder needs to be the one we mean: Media Foundation's only as a GPU's (its software
/// one keeps no keyframe interval or steady rate, and x264 beats it).
fn must_have(enc: &str) -> &'static [&'static str] {
    match enc {
        "h264_mf" => &["-hw_encoding", "1"],
        _ => &[],
    }
}

/// The first H.264 encoder that encodes a few frames here (asked once).
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
            candidates()
                .iter()
                .find(|e| {
                    Command::new(&ff)
                        .args([
                            "-hide_banner",
                            "-loglevel",
                            "error",
                            "-f",
                            "lavfi",
                            "-i",
                            "color=c=black:s=256x144:r=30",
                        ])
                        .args(["-frames:v", "3", "-c:v", e])
                        .args(must_have(e))
                        .args(["-f", "null", "-"])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success())
                })
                .map(|e| (*e).to_string())
        })
        .clone()
}

/// One FLV tag: kind (8 audio, 9 video), time in ms, body.
type Tag = (u8, u32, Vec<u8>);

/// A running FFmpeg: what goes in, the tags coming out, and its last words.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    tags: Receiver<Tag>,
    stderr: Arc<Mutex<String>>,
}

impl Proc {
    fn spawn(args: &[String]) -> Result<Self, AvError> {
        let ff =
            ffmpeg().ok_or_else(|| AvError::Encoder("FFmpeg isn't here, so this system can't encode yet".into()))?;
        let mut child = Command::new(ff)
            .args(["-hide_banner", "-loglevel", "error"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AvError::Encoder(format!("FFmpeg didn't start ({e})")))?;
        let stdin = child.stdin.take();
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
        if let Some(mut err) = child.stderr.take() {
            let keep = stderr.clone();
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = err.read_to_string(&mut s);
                if let Ok(mut k) = keep.lock() {
                    *k = s.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
                }
            });
        }
        Ok(Self { child, stdin, tags, stderr })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), AvError> {
        let w = self.stdin.as_mut().ok_or_else(|| AvError::Encoder("the encoder was already finished".into()))?;
        w.write_all(bytes).map_err(|_| self.died())
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

    /// Closes the input and waits for everything still coming.
    fn finish(&mut self) -> Vec<Tag> {
        self.stdin = None;
        let tags: Vec<Tag> = self.tags.iter().collect();
        let _ = self.child.wait();
        tags
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
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
}

impl std::fmt::Debug for Video {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ffenc::Video").field("encoder", &self.name).finish_non_exhaustive()
    }
}

impl Video {
    pub(crate) fn new(cfg: &VideoConfig) -> Result<Self, AvError> {
        if cfg.codec != VideoCodec::H264 {
            return Err(AvError::Encoder("HEVC through FFmpeg isn't done yet: H.264 is".into()));
        }
        let enc = if cfg.software {
            "libx264".to_string()
        } else {
            h264_encoder().ok_or_else(|| AvError::Encoder("no H.264 encoder works here through FFmpeg".into()))?
        };
        let kbps = cfg.bitrate_kbps;
        let gop = (u64::from(cfg.keyframe_secs.max(1)) * u64::from(cfg.fps_num) / u64::from(cfg.fps_den.max(1))).max(1);
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
        let cbr = matches!(cfg.rate, RateControl::Cbr);
        match cfg.rate {
            // A quality, not a bitrate: x264's rate factor (0 is lossless).
            RateControl::Crf(q) => a.extend(["-crf".into(), q.min(51).to_string()]),
            _ => a.extend(["-b:v".into(), format!("{kbps}k")]),
        }
        // A rate cap for steady encoders; VideoToolbox's own CBR turns off when one is given.
        if cbr && enc != "h264_videotoolbox" {
            a.extend(["-maxrate".into(), format!("{kbps}k"), "-bufsize".into(), format!("{}k", kbps * 2)]);
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
        a.extend(extra.iter().chain(must_have(&enc)).map(|s| s.to_string()));
        a.extend(["-f".into(), "flv".into(), "pipe:1".into()]);
        Ok(Self {
            proc: Proc::spawn(&a)?,
            times: VecDeque::new(),
            header: None,
            width: cfg.width,
            height: cfg.height,
            input: cfg.input,
            name: enc,
        })
    }

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
        }
        self.times.push_back(pts_us);
        self.proc.write(&raw)?;
        Ok(self.take(self.proc.tags.try_iter().collect()))
    }

    pub(crate) fn flush(&mut self) -> Result<Vec<VideoPacket>, AvError> {
        let tags = self.proc.finish();
        Ok(self.take(tags))
    }

    /// FLV video tags → packets: the codec header kept, frames given our own times.
    fn take(&mut self, tags: Vec<Tag>) -> Vec<VideoPacket> {
        let mut out = Vec::new();
        for (kind, _, body) in tags {
            if kind != 9 || body.len() < 5 || body[0] & 0x0f != 7 {
                continue;
            }
            match body[1] {
                0 => self.header = Some(body[5..].to_vec()),
                1 => {
                    let Some(pts) = self.times.pop_front() else { continue };
                    out.push(VideoPacket {
                        pts_us: pts,
                        dts_us: pts,
                        key: body[0] >> 4 == 1,
                        data: body[5..].to_vec(),
                    });
                }
                _ => {}
            }
        }
        out
    }
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
        Ok(Self { proc: Proc::spawn(&a)?, channels: cfg.channels as usize })
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
        self.proc.write(&bytes)?;
        Ok(self
            .proc
            .tags
            .try_iter()
            .filter_map(|(k, _, b)| (k == 8 && b.len() > 2 && b[1] == 1).then(|| b[2..].to_vec()))
            .collect())
    }
}
