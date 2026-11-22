//! Video encoding, the same on every system: what goes in (frames) and comes out (packets).
//! Each system's hardware encoder lives beside it (macOS: VideoToolbox, `apple/vt.rs`).

use crate::AvError;

/// The codec a stream is sent in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    /// H.265, sent over Enhanced RTMP (Twitch's and YouTube's HEVC).
    Hevc,
}

/// How frames are laid out when handed to the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8-bit B, G, R, A: what our compositor can draw to.
    Bgra,
    /// Y plane, then interleaved CbCr at half size, video range: libobs's own output.
    Nv12,
}

/// How the encoder spends its bits.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum RateControl {
    /// Steady at the bitrate, as streaming platforms ask (padded when there's little to send).
    /// Where the encoder can't (Intel Macs), an average held under the bitrate each second.
    #[default]
    Cbr,
    /// Steady at the bitrate and held strictly under `bitrate_kbps` per second (1× VBV on
    /// FFmpeg, `AverageBitRate` + 1 s `DataRateLimits` on VideoToolbox so transitions cannot burst
    /// past a strict platform ceiling like Twitch 6,000 kbps).
    CappedCbr,
    /// About the bitrate on average, less when the picture is simple (recordings).
    Average,
    /// A steady quality, 0 to 1, the bitrate following the picture (recordings).
    Quality(f32),
    /// x264's constant rate factor: 0 lossless … 51 worst (software only; recordings).
    Crf(u8),
}

/// What an encoder is asked to make.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoConfig {
    pub codec: VideoCodec,
    pub input: PixelFormat,
    pub width: u32,
    pub height: u32,
    /// Frames per second as a fraction (30/1, 60000/1001).
    pub fps_num: u32,
    pub fps_den: u32,
    pub bitrate_kbps: u32,
    pub rate: RateControl,
    /// Seconds between keyframes (platforms ask for 2); 0 lets the encoder pick (`gop_secs`).
    pub keyframe_secs: u32,
    /// x264 (through FFmpeg) instead of the system's own encoder: sharper at low bitrates, and
    /// lossless with `RateControl::Crf(0)`. H.264 only.
    pub software: bool,
    /// The H.264 profile (HEVC is always Main).
    pub profile: H264Profile,
    /// B-frames (frame reordering) in the Mac's encoder, as OBS has them by default: packets carry
    /// their own decode and presentation times. Off for Baseline (it has none), and always off for
    /// x264 and the FFmpeg hardware encoders, whose times are matched back by order.
    pub bframes: bool,
    /// x264's speed preset and tune, when the settings pick them (`X264_PRESETS`, `X264_TUNES`);
    /// other encoders ignore them.
    /// (indices into the lists, to keep this small).
    pub preset: Option<u8>,
    pub tune: Option<u8>,
}

/// x264's presets, fastest first, and its tunes (what the recording's "Your own" offers).
pub const X264_PRESETS: [&str; 9] =
    ["ultrafast", "superfast", "veryfast", "faster", "fast", "medium", "slow", "slower", "veryslow"];
pub const X264_TUNES: [&str; 6] = ["film", "animation", "grain", "stillimage", "psnr", "zerolatency"];

/// The index in `list` of a preset or tune a settings file carries, or None if it isn't one.
pub fn x264_index(list: &[&str], name: &str) -> Option<u8> {
    list.iter().position(|n| n.eq_ignore_ascii_case(name)).and_then(|i| u8::try_from(i).ok())
}

impl VideoConfig {
    /// Seconds between keyframes as the encoders use it: 0 is "let the encoder pick", here 10 s
    /// (a stream is always given 2 s by its caller: platforms ask for it).
    pub fn gop_secs(&self) -> u32 {
        if self.keyframe_secs == 0 { 10 } else { self.keyframe_secs }
    }
}

/// H.264's profiles: what the settings' "profile" picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum H264Profile {
    Baseline,
    Main,
    #[default]
    High,
}

impl H264Profile {
    /// The settings' word (`baseline`, `main`, `high`); anything else is High.
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "baseline" => Self::Baseline,
            "main" => Self::Main,
            _ => Self::High,
        }
    }
}

use std::sync::atomic::AtomicU64;

static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Frames the encoders left out since the program started: one that was behind (FFmpeg's queue
/// full) or that VideoToolbox dropped. OBS's "skipped frames due to encoding lag". The engine's
/// own frames that weren't drawn in time are counted apart (`Stats::skipped`).
pub fn dropped_frames() -> u64 {
    DROPPED.load(std::sync::atomic::Ordering::Relaxed)
}

/// What the frame path copied on the CPU, and how many frames took the zero-copy route: counters
/// for tests and the once-a-minute `vt_cost` line. Bytes are what `memcpy` moved of pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyStats {
    /// The card's readback copied into host planes (`Nv12`).
    pub readback_bytes: u64,
    /// A host frame copied into the encoder's pixel buffer.
    pub fill_bytes: u64,
    /// A pooled surface read back out for a consumer that needs host bytes (a tap, FFmpeg).
    pub materialised_bytes: u64,
    /// Frames the card drew straight into a pooled surface (no pixel copy).
    pub zero_copy_frames: u64,
    /// Frames that wanted that but took the copying path (pool full, no Metal, a size the pool
    /// wasn't made for).
    pub fallback_frames: u64,
    /// Surface pools made (one per size, again after one was let go).
    pub pools_made: u64,
}

static COPY_READBACK: AtomicU64 = AtomicU64::new(0);
static COPY_FILL: AtomicU64 = AtomicU64::new(0);
static COPY_MATERIAL: AtomicU64 = AtomicU64::new(0);
static ZERO_FRAMES: AtomicU64 = AtomicU64::new(0);
static FALLBACK_FRAMES: AtomicU64 = AtomicU64::new(0);
static POOLS_MADE: AtomicU64 = AtomicU64::new(0);

/// The copy counters since the program started.
pub fn copy_stats() -> CopyStats {
    use std::sync::atomic::Ordering::Relaxed;
    CopyStats {
        readback_bytes: COPY_READBACK.load(Relaxed),
        fill_bytes: COPY_FILL.load(Relaxed),
        materialised_bytes: COPY_MATERIAL.load(Relaxed),
        zero_copy_frames: ZERO_FRAMES.load(Relaxed),
        fallback_frames: FALLBACK_FRAMES.load(Relaxed),
        pools_made: POOLS_MADE.load(Relaxed),
    }
}

pub(crate) fn note_readback_bytes(n: u64) {
    COPY_READBACK.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn note_fill_bytes(n: u64) {
    COPY_FILL.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn note_materialised_bytes(n: u64) {
    COPY_MATERIAL.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}
#[allow(dead_code)]
pub(crate) fn note_zero_copy_frame() {
    ZERO_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn note_pool_made() {
    POOLS_MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[allow(dead_code)]
pub(crate) fn note_fallback_frame() {
    FALLBACK_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// An encoder left `n` frames out.
pub(crate) fn note_dropped(n: u64) {
    DROPPED.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

/// One frame's pixels, borrowed. Strides are bytes per row.
#[derive(Debug, Clone, Copy)]
pub enum Frame<'a> {
    Bgra {
        data: &'a [u8],
        stride: usize,
    },
    Nv12 {
        y: &'a [u8],
        y_stride: usize,
        uv: &'a [u8],
        uv_stride: usize,
    },
    /// An NV12 frame in a pooled IOSurface (Mac): the Mac's encoder takes it as it is; anything
    /// else reads it where it sits.
    #[cfg(target_os = "macos")]
    Surface(&'a crate::apple::surface::PixelFrame),
}

/// One encoded frame: NAL units, each with a 4-byte big-endian length before it (AVCC / HVCC,
/// as FLV and MP4 carry them). Times in microseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPacket {
    pub pts_us: i64,
    pub dts_us: i64,
    pub key: bool,
    pub data: Vec<u8>,
}

/// The encoder doing the work.
enum Inner {
    /// The Mac's own media engine.
    #[cfg(target_os = "macos")]
    Native(crate::apple::vt::Encoder),
    /// FFmpeg, with the best H.264 encoder it has here.
    Ffmpeg(Box<crate::ffenc::Video>),
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(target_os = "macos")]
            Self::Native(_) => f.write_str("VideoToolbox"),
            Self::Ffmpeg(v) => v.fmt(f),
        }
    }
}

/// A video encoder: the system's own (VideoToolbox on a Mac), else FFmpeg's best here.
#[derive(Debug)]
pub struct VideoEncoder {
    inner: Inner,
    config: VideoConfig,
}

/// `FERROCAST_AV_ENCODER=ffmpeg` uses FFmpeg even where a native encoder exists (to compare).
fn ffmpeg_asked() -> bool {
    std::env::var("FERROCAST_AV_ENCODER").is_ok_and(|v| v == "ffmpeg")
}

impl VideoEncoder {
    pub fn new(config: VideoConfig) -> Result<Self, AvError> {
        Self::check(&config)?;
        #[cfg(target_os = "macos")]
        if !ffmpeg_asked() && !config.software {
            return Ok(Self { inner: Inner::Native(crate::apple::vt::Encoder::new(&config)?), config });
        }
        let _ = ffmpeg_asked();
        Self::ffmpeg(config)
    }

    /// Through FFmpeg, whatever the system.
    pub fn ffmpeg(config: VideoConfig) -> Result<Self, AvError> {
        Self::check(&config)?;
        Ok(Self { inner: Inner::Ffmpeg(Box::new(crate::ffenc::Video::new(&config)?)), config })
    }

    /// Opens a specific FFmpeg H.264 encoder (or native VideoToolbox when `ffmpeg_encoder` is `None`
    /// on macOS), with optional bounded blocking queue capacity and cancellation signal for
    /// synchronous benchmarking.
    pub fn for_benchmark(
        config: VideoConfig,
        ffmpeg_encoder: Option<&str>,
        blocking_cap: Option<usize>,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Self, AvError> {
        Self::check(&config)?;
        if let Some(enc) = ffmpeg_encoder {
            return Ok(Self {
                inner: Inner::Ffmpeg(Box::new(crate::ffenc::Video::new_named(&config, enc, blocking_cap, cancel)?)),
                config,
            });
        }
        Self::new(config)
    }

    /// Frames skipped because this FFmpeg encoder's input queue stayed full.
    pub fn skipped(&self) -> u64 {
        match &self.inner {
            #[cfg(target_os = "macos")]
            Inner::Native(_) => 0,
            Inner::Ffmpeg(v) => v.skipped,
        }
    }

    fn check(config: &VideoConfig) -> Result<(), AvError> {
        if config.width == 0
            || config.height == 0
            || !config.width.is_multiple_of(2)
            || !config.height.is_multiple_of(2)
        {
            return Err(AvError::Encoder(format!(
                "the picture is {}x{}: both sides must be even and above 0",
                config.width, config.height
            )));
        }
        if config.fps_num == 0 || config.fps_den == 0 || config.bitrate_kbps == 0 {
            return Err(AvError::Encoder("the frame rate and bitrate must be above 0".into()));
        }
        if matches!(config.rate, RateControl::Crf(_)) && !config.software {
            return Err(AvError::Encoder("a constant rate factor is x264's: ask for the software encoder".into()));
        }
        if config.software && config.codec != VideoCodec::H264 {
            return Err(AvError::Encoder("x264 makes H.264 only".into()));
        }
        Ok(())
    }

    pub fn config(&self) -> &VideoConfig {
        &self.config
    }

    /// Which encoder does the work ("VideoToolbox", or FFmpeg's: "h264_nvenc", "libx264"…).
    pub fn name(&self) -> String {
        match &self.inner {
            #[cfg(target_os = "macos")]
            Inner::Native(_) => "VideoToolbox".into(),
            Inner::Ffmpeg(v) => v.name.clone(),
        }
    }

    /// The decoder configuration record (avcC for H.264, hvcC for HEVC): the codec header FLV
    /// and MP4 send first. Known once the first frame is encoded.
    pub fn header(&self) -> Option<Vec<u8>> {
        match &self.inner {
            #[cfg(target_os = "macos")]
            Inner::Native(e) => e.header(),
            Inner::Ffmpeg(e) => e.header(),
        }
    }

    pub(crate) fn dts_shift(&self) -> i64 {
        match &self.inner {
            #[cfg(target_os = "macos")]
            Inner::Native(e) => e.dts_shift(),
            Inner::Ffmpeg(_) => 0,
        }
    }

    pub(crate) fn set_min_dts(&mut self, #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] last_dts: i64) {
        match &mut self.inner {
            #[cfg(target_os = "macos")]
            Inner::Native(e) => e.set_min_dts(last_dts),
            Inner::Ffmpeg(_) => {}
        }
    }

    /// True once when a rebuilt hardware session made a codec header unlike the lost one's:
    /// a stream sends the new sequence header before the next keyframe.
    pub fn header_changed(&mut self) -> bool {
        match &mut self.inner {
            #[cfg(target_os = "macos")]
            Inner::Native(e) => e.header_changed(),
            Inner::Ffmpeg(_) => false,
        }
    }

    /// Tests: the hardware session is taken away, and (with `differ`) its header made unlike
    /// what a new one will have.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn lose_session_for_test(&mut self, differ: bool) {
        if let Inner::Native(e) = &mut self.inner {
            if differ {
                e.perturb_header_for_test();
            }
            e.invalidate_for_test();
        }
    }

    /// Hands a frame in (`key` forces a keyframe where the encoder can); returns what's ready.
    pub fn encode(&mut self, frame: Frame<'_>, pts_us: i64, key: bool) -> Result<Vec<VideoPacket>, AvError> {
        let ok = match (frame, self.config.input) {
            (Frame::Bgra { data, stride }, PixelFormat::Bgra) => {
                stride >= self.config.width as usize * 4 && data.len() >= stride * self.config.height as usize
            }
            (Frame::Nv12 { y, y_stride, uv, uv_stride }, PixelFormat::Nv12) => {
                let h = self.config.height as usize;
                y_stride >= self.config.width as usize
                    && uv_stride >= self.config.width as usize
                    && y.len() >= y_stride * h
                    && uv.len() >= uv_stride * h / 2
            }
            #[cfg(target_os = "macos")]
            (Frame::Surface(px), PixelFormat::Nv12) => px.size() == (self.config.width, self.config.height),
            _ => false,
        };
        if !ok {
            return Err(AvError::Encoder("a frame doesn't match the encoder's size or pixel format".into()));
        }
        match &mut self.inner {
            #[cfg(target_os = "macos")]
            Inner::Native(e) => e.encode(frame, pts_us, key),
            // FFmpeg starts on a keyframe and keeps the interval itself.
            Inner::Ffmpeg(e) => {
                let _ = key;
                e.encode(frame, pts_us)
            }
        }
    }

    /// A new bitrate for what's encoded from now on, on the stream as it runs: the Mac's
    /// encoder takes it; FFmpeg's hands over to another at the new rate at the next frame.
    pub fn set_kbps(&mut self, kbps: u32) -> Result<bool, AvError> {
        if kbps == 0 {
            return Err(AvError::Encoder("the bitrate must be above 0".into()));
        }
        match &mut self.inner {
            #[cfg(target_os = "macos")]
            Inner::Native(e) => {
                e.set_rate(kbps, self.config.rate)?;
                self.config.bitrate_kbps = kbps;
                Ok(true)
            }
            // Another encoder takes over at the next frame (a keyframe), seamlessly.
            // (CRF has no bitrate to change.)
            Inner::Ffmpeg(_) if matches!(self.config.rate, RateControl::Crf(_)) => Ok(false),
            Inner::Ffmpeg(e) => {
                e.set_kbps(kbps)?;
                self.config.bitrate_kbps = kbps;
                Ok(true)
            }
        }
    }

    /// Everything still inside the encoder.
    pub fn flush(&mut self) -> Result<Vec<VideoPacket>, AvError> {
        match &mut self.inner {
            #[cfg(target_os = "macos")]
            Inner::Native(e) => e.flush(),
            Inner::Ffmpeg(e) => e.flush(),
        }
    }
}
