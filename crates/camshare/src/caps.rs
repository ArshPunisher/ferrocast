//! What the camera offers apps: sizes, rates and formats, best first, and how an app's choice
//! maps onto them.

use crate::FrameInfo;
use crate::feed::Format;

/// One way the camera can send: a size, a format and a frame time (100 ns units, as DirectShow
/// and Media Foundation count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub interval: i64,
}

impl Mode {
    pub fn frame_len(&self) -> usize {
        self.format.frame_len(self.width, self.height)
    }

    /// Frames a second, rounded.
    pub fn fps(&self) -> u32 {
        (10_000_000 / self.interval.max(1)) as u32
    }
}

/// 100 ns units between frames at `num`/`den` frames a second (30 when unknown).
pub fn interval(num: u32, den: u32) -> i64 {
    if num == 0 || den == 0 {
        return 333_333;
    }
    (10_000_000i64 * i64::from(den)) / i64::from(num)
}

/// Common sizes apps look for, after Ferrocast's own.
const SIZES: [(u32, u32); 3] = [(1920, 1080), (1280, 720), (640, 360)];
/// The formats, the one most apps take first.
pub const FORMATS: [Format; 3] = [Format::Nv12, Format::Yuy2, Format::I420];

/// Everything offered, best first: Ferrocast's own size and rate (when it's sending), then
/// the common sizes; each at Ferrocast's rate and at 30 fps; each in every format.
pub fn offered(native: Option<FrameInfo>) -> Vec<Mode> {
    let own = native.map(|i| ((i.width, i.height), interval(i.fps.0, i.fps.1)));
    let mut sizes: Vec<(u32, u32)> = own.map(|(s, _)| s).into_iter().collect();
    for s in SIZES {
        if !sizes.contains(&s) {
            sizes.push(s);
        }
    }
    let thirty = interval(30, 1);
    let mut rates: Vec<i64> = own.map(|(_, r)| r).into_iter().collect();
    if !rates.contains(&thirty) {
        rates.push(thirty);
    }
    let mut out = Vec::new();
    for &(width, height) in &sizes {
        for &interval in &rates {
            for format in FORMATS {
                out.push(Mode { width, height, format, interval });
            }
        }
    }
    out
}

/// Whether the camera can send `m`: one of its formats, even sides up to 4K, 1 to 120 fps.
/// (Any size: it's fitted from Ferrocast's picture.)
pub fn acceptable(m: &Mode) -> bool {
    m.width >= 2
        && m.height >= 2
        && m.width.is_multiple_of(2)
        && m.height.is_multiple_of(2)
        && m.width <= 3840
        && m.height <= 2160
        && (interval(120, 1)..=interval(1, 1)).contains(&m.interval)
}

/// The format a FourCC names, if the camera sends it.
pub fn format_of(fourcc: u32) -> Option<Format> {
    FORMATS.into_iter().find(|f| f.fourcc() == fourcc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(w: u32, h: u32, fps: u32) -> FrameInfo {
        FrameInfo { width: w, height: h, fps: (fps, 1), seq: 1, ts_ns: 0 }
    }

    #[test]
    fn ferrocasts_own_size_and_rate_come_first() {
        let m = offered(Some(info(1726, 1080, 60)));
        assert_eq!(m[0], Mode { width: 1726, height: 1080, format: Format::Nv12, interval: 166_666 });
        assert_eq!((m[1].format, m[2].format), (Format::Yuy2, Format::I420));
        assert_eq!(m[3].interval, 333_333, "then the same at 30 fps");
        assert_eq!(m.len(), 4 * 2 * 3, "own + three common sizes, two rates, three formats");
        assert!(m.iter().all(acceptable));
    }

    #[test]
    fn without_ferrocast_the_common_sizes_at_30() {
        let m = offered(None);
        assert_eq!((m[0].width, m[0].height, m[0].fps()), (1920, 1080, 30));
        assert_eq!(m.len(), 3 * 3);
        // Ferrocast at 1080p30 already: nothing twice.
        let m = offered(Some(info(1920, 1080, 30)));
        assert_eq!(m.len(), 3 * 3);
    }

    #[test]
    fn what_apps_may_ask_for() {
        let ok = Mode { width: 1280, height: 720, format: Format::Yuy2, interval: 333_333 };
        assert!(acceptable(&ok));
        assert!(!acceptable(&Mode { width: 1279, ..ok }), "odd");
        assert!(!acceptable(&Mode { width: 7680, height: 4320, ..ok }), "too big");
        assert!(!acceptable(&Mode { interval: 0, ..ok }), "no rate");
        assert!(acceptable(&Mode { interval: interval(120, 1), ..ok }));
        assert_eq!(interval(30000, 1001), 333_666);
        assert_eq!(interval(0, 1), 333_333);
        assert_eq!(format_of(u32::from_le_bytes(*b"YUY2")), Some(Format::Yuy2));
        assert_eq!(format_of(u32::from_le_bytes(*b"MJPG")), None);
    }
}
