//! The Image source: PNG, JPEG, GIF (animated, with its own timing and loops), WebP and BMP,
//! premultiplied as libobs's image loader stores them.

use std::path::Path;
use std::time::Duration;

use image::AnimationDecoder;

use crate::{AvError, Picture};

/// The largest picture an image may be (libobs's limit for textures is the GPU's; 16384 is
/// what every GPU here takes).
const MAX_SIDE: u32 = 16_384;
/// An animation's frames, all together, may take this much memory (500 frames of 800×600 is
/// about 960 MB; more than this is refused rather than taking the Studio down).
const MAX_ANIMATION_BYTES: usize = 512 * 1024 * 1024;
/// A GIF frame shorter than this is shown this long, as browsers and libobs do.
const MIN_DELAY: Duration = Duration::from_millis(20);

/// A loaded image: one picture, or an animation's frames with how long each shows.
#[derive(Debug, Clone)]
pub struct ImageSource {
    frames: Vec<(Picture, Duration)>,
    total: Duration,
}

impl ImageSource {
    /// Loads a file. Errors say what's wrong: missing, unreadable, too big, not an image.
    pub fn open(path: &Path) -> Result<Self, AvError> {
        let bytes =
            std::fs::read(path).map_err(|e| AvError::Source(format!("{} can't be read ({e})", path.display())))?;
        Self::from_bytes(&bytes).map_err(|e| match e {
            AvError::Source(m) => AvError::Source(format!("{}: {m}", path.display())),
            other => other,
        })
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AvError> {
        let format = image::guess_format(bytes)
            .map_err(|_| AvError::Source("not a picture this source opens (PNG, JPEG, GIF, WebP, BMP)".into()))?;
        if format == image::ImageFormat::Gif {
            let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).map_err(bad)?;
            let mut frames = Vec::new();
            let mut bytes_kept = 0usize;
            for f in decoder.into_frames() {
                let f = f.map_err(bad)?;
                let (fw, fh) = f.buffer().dimensions();
                bytes_kept = bytes_kept.saturating_add(fw as usize * fh as usize * 4);
                if bytes_kept > MAX_ANIMATION_BYTES {
                    return Err(AvError::Source(format!(
                        "the GIF is too long or too big to play here (over {} MB once opened); a shorter or smaller one, or a video, plays",
                        MAX_ANIMATION_BYTES / 1024 / 1024
                    )));
                }
                let (n, d) = f.delay().numer_denom_ms();
                let delay = Duration::from_millis(u64::from(n) / u64::from(d.max(1))).max(MIN_DELAY);
                let buf = f.into_buffer();
                frames.push((picture(buf.width(), buf.height(), buf.into_raw())?, delay));
            }
            if frames.is_empty() {
                return Err(AvError::Source("the GIF has no frames".into()));
            }
            let total = frames.iter().map(|(_, d)| *d).sum();
            return Ok(Self { frames, total });
        }
        let img = image::load_from_memory_with_format(bytes, format).map_err(bad)?.into_rgba8();
        let (w, h) = img.dimensions();
        Ok(Self { frames: vec![(picture(w, h, img.into_raw())?, Duration::ZERO)], total: Duration::ZERO })
    }

    pub fn size(&self) -> (u32, u32) {
        self.frames.first().map_or((0, 0), |(p, _)| (p.width, p.height))
    }

    pub fn animated(&self) -> bool {
        self.frames.len() > 1
    }

    /// The picture to show `elapsed` after it started (an animation loops).
    pub fn frame_at(&self, elapsed: Duration) -> &Picture {
        if self.frames.len() <= 1 || self.total.is_zero() {
            return &self.frames[0].0;
        }
        let mut t = Duration::from_nanos((elapsed.as_nanos() % self.total.as_nanos()) as u64);
        for (p, d) in &self.frames {
            if t < *d {
                return p;
            }
            t -= *d;
        }
        &self.frames[self.frames.len() - 1].0
    }
}

fn bad(e: image::ImageError) -> AvError {
    AvError::Source(format!("the picture couldn't be read ({e})"))
}

fn picture(w: u32, h: u32, rgba: Vec<u8>) -> Result<Picture, AvError> {
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(AvError::Source(format!("the picture is {w}x{h}: at most {MAX_SIDE} pixels each way")));
    }
    Ok(Picture::image(w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, px: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut out),
            px,
            w,
            h,
            image::ExtendedColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        out
    }

    #[test]
    fn pngs_load_premultiplied() {
        let src = ImageSource::from_bytes(&png(1, 1, &[200, 100, 50, 128])).unwrap_or_else(|e| panic!("{e}"));
        let p = src.frame_at(Duration::ZERO);
        assert!(p.premultiplied);
        assert_eq!(p.rgba, vec![100, 50, 25, 128]);
        assert!(!src.animated());
    }

    #[test]
    fn gifs_animate_on_their_timing_and_loop() {
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame, RgbaImage};
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut out);
            enc.set_repeat(Repeat::Infinite).unwrap_or_else(|e| panic!("{e}"));
            for (c, ms) in [([255, 0, 0, 255], 100), ([0, 0, 255, 255], 300)] {
                let img = RgbaImage::from_pixel(2, 2, image::Rgba(c));
                enc.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1)))
                    .unwrap_or_else(|e| panic!("{e}"));
            }
        }
        let src = ImageSource::from_bytes(&out).unwrap_or_else(|e| panic!("{e}"));
        assert!(src.animated());
        let red = |ms| src.frame_at(Duration::from_millis(ms)).rgba[0] > 200;
        assert!(red(50));
        assert!(!red(150));
        // 400 ms a loop.
        assert!(red(420));
    }

    #[test]
    fn junk_is_refused_with_a_reason() {
        match ImageSource::from_bytes(b"not a picture") {
            Err(AvError::Source(m)) => assert!(m.contains("PNG")),
            other => panic!("{other:?}"),
        }
    }
}
