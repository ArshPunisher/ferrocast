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

/// A hardware video encoder.
#[derive(Debug)]
pub struct VideoEncoder {
    #[cfg(target_os = "macos")]
    inner: crate::apple::vt::Encoder,
    config: VideoConfig,
}

impl VideoEncoder {
    pub fn new(config: VideoConfig) -> Result<Self, AvError> {
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
        #[cfg(target_os = "macos")]
        {
            Ok(Self { inner: crate::apple::vt::Encoder::new(&config)?, config })
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(AvError::Encoder("this system's hardware encoder isn't built yet".into()))
        }
    }

    pub fn config(&self) -> &VideoConfig {
        &self.config
    }

    /// The decoder configuration record (avcC for H.264, hvcC for HEVC): the codec header FLV
    /// and MP4 send first. Known once the first frame is encoded.
    pub fn header(&self) -> Option<Vec<u8>> {
        #[cfg(target_os = "macos")]
        {
            self.inner.header()
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    /// Hands a frame in (`key` forces a keyframe); returns what's ready.
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
        #[cfg(target_os = "macos")]
        {
            self.inner.encode(frame, pts_us, key)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (pts_us, key);
            Ok(Vec::new())
        }
    }

    /// Everything still inside the encoder.
    pub fn flush(&mut self) -> Result<Vec<VideoPacket>, AvError> {
        #[cfg(target_os = "macos")]
        {
            self.inner.flush()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(Vec::new())
        }
    }
}
