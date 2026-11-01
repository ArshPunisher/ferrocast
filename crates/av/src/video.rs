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
    /// About the bitrate on average, less when the picture is simple (recordings).
    Average,
    /// A steady quality, 0 to 1, the bitrate following the picture (recordings).
    Quality(f32),
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
    /// Seconds between keyframes (platforms ask for 2).
    pub keyframe_secs: u32,
}

/// One frame's pixels, borrowed. Strides are bytes per row.
#[derive(Debug, Clone, Copy)]
pub enum Frame<'a> {
    Bgra { data: &'a [u8], stride: usize },
    Nv12 { y: &'a [u8], y_stride: usize, uv: &'a [u8], uv_stride: usize },
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
    Ffmpeg(crate::ffenc::Video),
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
        if !ffmpeg_asked() {
            return Ok(Self { inner: Inner::Native(crate::apple::vt::Encoder::new(&config)?), config });
        }
        let _ = ffmpeg_asked();
        Self::ffmpeg(config)
    }

    /// Through FFmpeg, whatever the system.
    pub fn ffmpeg(config: VideoConfig) -> Result<Self, AvError> {
        Self::check(&config)?;
        Ok(Self { inner: Inner::Ffmpeg(crate::ffenc::Video::new(&config)?), config })
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

    /// A new bitrate for what's encoded from now on. The Mac's encoder takes it while running;
    /// FFmpeg's can't (false: it applies from the next start).
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
            Inner::Ffmpeg(_) => Ok(false),
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
