//! What a camera reader shows each time its app wants a frame: Ferrocast's latest picture in
//! the size and format the app agreed to, or the placeholder while Ferrocast isn't sending.
//! It finds the shared memory again by itself when Ferrocast starts after the app (or again).

use crate::{FrameInfo, Read, Reader, Region, nv12_len, pic};

/// The pixel formats apps ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Nv12,
    Yuy2,
    I420,
}

impl Format {
    /// Bytes in one frame of `w`×`h`.
    pub fn frame_len(self, w: u32, h: u32) -> usize {
        match self {
            Format::Nv12 | Format::I420 => nv12_len(w, h),
            Format::Yuy2 => w as usize * h as usize * 2,
        }
    }

    /// Bits a pixel takes (`biBitCount`).
    pub fn bits(self) -> u16 {
        match self {
            Format::Nv12 | Format::I420 => 12,
            Format::Yuy2 => 16,
        }
    }

    /// Its FourCC (`biCompression`, and the subtype GUID's first field).
    pub fn fourcc(self) -> u32 {
        u32::from_le_bytes(*match self {
            Format::Nv12 => b"NV12",
            Format::Yuy2 => b"YUY2",
            Format::I420 => b"I420",
        })
    }
}

/// What a frame filled in was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// Ferrocast's picture (frame number, size it came in).
    Picture { seq: u64, width: u32, height: u32 },
    /// The placeholder (Ferrocast isn't sending).
    Placeholder,
}

/// How often to look for the shared memory while it isn't there.
const RETRY_MS: u64 = 1000;

/// One reader's state.
pub struct Feed {
    open: Box<dyn FnMut() -> Option<Region> + Send>,
    reader: Option<Reader>,
    tried_ms: Option<u64>,
    last: u64,
    raw: Vec<u8>,
    raw_size: (u32, u32),
    fitted: Vec<u8>,
    placeholder: Option<((u32, u32), Vec<u8>)>,
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feed").field("open", &self.reader.is_some()).field("last", &self.last).finish()
    }
}

impl Feed {
    /// `open` finds the shared memory (None while it isn't there): asked at most once a second.
    pub fn new(open: impl FnMut() -> Option<Region> + Send + 'static) -> Self {
        Feed {
            open: Box::new(open),
            reader: None,
            tried_ms: None,
            last: 0,
            raw: Vec::new(),
            raw_size: (0, 0),
            fitted: Vec::new(),
            placeholder: None,
        }
    }

    /// A feed on a region it already has (the Media Foundation reader made it).
    pub fn with_reader(reader: Reader) -> Self {
        let mut f = Feed::new(|| None);
        f.reader = Some(reader);
        f
    }

    /// The latest frame's size and rate, when Ferrocast is sending (an app picking a format).
    pub fn peek(&mut self, now_ms: u64) -> Option<FrameInfo> {
        self.find(now_ms);
        self.reader.as_ref().and_then(|r| r.peek(now_ms))
    }

    fn find(&mut self, now_ms: u64) {
        if self.reader.is_some() || self.tried_ms.is_some_and(|t| now_ms.saturating_sub(t) < RETRY_MS) {
            return;
        }
        self.tried_ms = Some(now_ms);
        self.reader = (self.open)().map(Reader::new);
    }

    /// Fills `dst` (a `w`×`h` frame in `format`) with what to show now.
    pub fn fill(&mut self, now_ms: u64, (w, h): (u32, u32), format: Format, dst: &mut [u8]) -> Shown {
        if w < 2 || h < 2 || dst.len() < format.frame_len(w, h) {
            return Shown::Placeholder;
        }
        self.find(now_ms);
        let read = match &self.reader {
            Some(r) => r.read(now_ms, self.last, &mut self.raw),
            None => Read::Absent,
        };
        match read {
            Read::Frame(info) => {
                self.last = info.seq;
                self.raw_size = (info.width, info.height);
            }
            Read::Same if self.last != 0 => {}
            Read::Same | Read::Absent => {
                // Gone (stopped, quit or crashed): forget the frame, and look for the memory
                // again later (a new Ferrocast makes it anew).
                if matches!(read, Read::Absent) {
                    self.last = 0;
                    self.raw_size = (0, 0);
                    if self.tried_ms.is_some_and(|t| now_ms.saturating_sub(t) >= RETRY_MS) {
                        self.reader = None;
                    }
                }
                if self.placeholder.as_ref().is_none_or(|(size, _)| *size != (w, h)) {
                    self.placeholder = Some(((w, h), pic::placeholder(w, h)));
                }
                if let Some((_, ph)) = &self.placeholder {
                    convert(ph, w, h, format, dst);
                }
                return Shown::Placeholder;
            }
        }
        let (rw, rh) = self.raw_size;
        let src: &[u8] = if (rw, rh) == (w, h) {
            &self.raw
        } else {
            self.fitted.resize(nv12_len(w, h), 0);
            pic::fit_nv12(&self.raw, (rw, rh), &mut self.fitted, (w, h));
            &self.fitted
        };
        convert(src, w, h, format, dst);
        Shown::Picture { seq: self.last, width: rw, height: rh }
    }
}

/// An NV12 frame into `format`.
fn convert(nv12: &[u8], w: u32, h: u32, format: Format, dst: &mut [u8]) {
    match format {
        Format::Nv12 => {
            let n = nv12_len(w, h);
            dst[..n].copy_from_slice(&nv12[..n]);
        }
        Format::Yuy2 => pic::nv12_to_yuy2(nv12, w, h, dst),
        Format::I420 => pic::nv12_to_i420(nv12, w, h, dst),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Writer;
    use std::sync::{Arc, Mutex};

    fn view(r: &Region) -> Region {
        // SAFETY: the tests keep the owning region alive past every view.
        unsafe { Region::from_raw(r.base, r.len) }
    }

    fn write(w: &mut Writer, size: (u32, u32), v: u8, now: u64) {
        let n = (size.0 * size.1) as usize;
        assert!(w.write(size, (30, 1), 0, &vec![v; n], &vec![128; n / 2], now).is_ok());
    }

    #[test]
    fn formats_know_their_sizes() {
        assert_eq!(Format::Nv12.frame_len(1920, 1080), 3_110_400);
        assert_eq!(Format::Yuy2.frame_len(1920, 1080), 4_147_200);
        assert_eq!(Format::Nv12.fourcc(), 0x3231_564E);
        assert_eq!((Format::Yuy2.bits(), Format::I420.bits()), (16, 12));
    }

    #[test]
    fn the_placeholder_until_ferrocast_sends_then_its_picture() {
        let mem = Region::heap();
        let there = Arc::new(Mutex::new(false));
        let (m, t) = (view(&mem), there.clone());
        let mut slot = Some(m);
        let mut feed = Feed::new(move || if *t.lock().unwrap_or_else(|p| p.into_inner()) { slot.take() } else { None });
        let mut dst = vec![0; Format::Nv12.frame_len(64, 36)];
        assert_eq!(feed.fill(1_000, (64, 36), Format::Nv12, &mut dst), Shown::Placeholder, "no memory yet");
        // Ferrocast starts.
        *there.lock().unwrap_or_else(|p| p.into_inner()) = true;
        let mut w = Writer::new(view(&mem));
        write(&mut w, (64, 36), 200, 1_500);
        assert_eq!(feed.fill(1_500, (64, 36), Format::Nv12, &mut dst), Shown::Placeholder, "looked a moment ago");
        let got = feed.fill(2_100, (64, 36), Format::Nv12, &mut dst);
        assert_eq!(got, Shown::Picture { seq: 1, width: 64, height: 36 }, "found it a second later");
        assert!(dst[..64 * 36].iter().all(|&v| v == 200));
        // The same frame again while nothing new comes (the app asks faster than Ferrocast draws).
        assert_eq!(
            feed.fill(2_101, (64, 36), Format::Nv12, &mut dst),
            Shown::Picture { seq: 1, width: 64, height: 36 }
        );
        assert_eq!(feed.peek(2_101).map(|i| (i.width, i.height)), Some((64, 36)));
        // Ferrocast stops: the placeholder at once, not a frozen frame.
        w.stop();
        assert_eq!(feed.fill(2_200, (64, 36), Format::Nv12, &mut dst), Shown::Placeholder);
        assert!(feed.peek(2_200).is_none());
    }

    #[test]
    fn another_size_and_format_are_made_from_the_picture() {
        let mem = Region::heap();
        let mut slot = Some(view(&mem));
        let mut feed = Feed::new(move || slot.take());
        let mut w = Writer::new(view(&mem));
        write(&mut w, (128, 72), 180, 10);
        let mut yuy2 = vec![0; Format::Yuy2.frame_len(64, 36)];
        let got = feed.fill(10, (64, 36), Format::Yuy2, &mut yuy2);
        assert_eq!(got, Shown::Picture { seq: 1, width: 128, height: 72 });
        assert!(yuy2.as_chunks::<2>().0.iter().all(|p| *p == [180, 128]), "scaled down, packed as YUY2");
        let mut i420 = vec![0; Format::I420.frame_len(64, 36)];
        feed.fill(10, (64, 36), Format::I420, &mut i420);
        assert!(i420[..64 * 36].iter().all(|&v| v == 180));
        // Its size changes while running (the canvas changed): still fitted, no reconnect.
        write(&mut w, (64, 64), 90, 11);
        let mut nv12 = vec![0; Format::Nv12.frame_len(64, 36)];
        assert_eq!(feed.fill(11, (64, 36), Format::Nv12, &mut nv12), Shown::Picture { seq: 2, width: 64, height: 64 });
        assert_eq!(nv12[18 * 64 + 32], 90, "the picture in the middle");
        assert_eq!(nv12[18 * 64], pic::BLACK_Y, "bars at the sides");
    }

    #[test]
    fn a_crashed_ferrocast_is_found_again_when_it_comes_back() {
        let mem = Region::heap();
        let opens = Arc::new(Mutex::new(0));
        let (o, m) = (opens.clone(), view(&mem));
        let base = m.base as usize;
        let mut feed = Feed::new(move || {
            *o.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            // SAFETY: the test keeps `mem` alive.
            Some(unsafe { Region::from_raw(base as *mut u8, crate::SIZE) })
        });
        let mut w = Writer::new(view(&mem));
        write(&mut w, (64, 36), 50, 100);
        let mut dst = vec![0; Format::Nv12.frame_len(64, 36)];
        assert!(matches!(feed.fill(100, (64, 36), Format::Nv12, &mut dst), Shown::Picture { .. }));
        // It stops beating without saying so (a crash): the placeholder after a second…
        assert_eq!(feed.fill(100 + crate::STALE_MS + 1, (64, 36), Format::Nv12, &mut dst), Shown::Placeholder);
        // …and the memory is looked for anew, so a new Ferrocast's is used.
        assert_eq!(feed.fill(3_000, (64, 36), Format::Nv12, &mut dst), Shown::Placeholder);
        let mut again = Writer::new(view(&mem));
        write(&mut again, (64, 36), 60, 4_100);
        assert!(matches!(feed.fill(4_100, (64, 36), Format::Nv12, &mut dst), Shown::Picture { .. }));
        assert_eq!(dst[0], 60);
        assert!(*opens.lock().unwrap_or_else(|p| p.into_inner()) >= 2, "opened again");
    }

    #[test]
    fn a_frame_too_small_for_the_app_is_left_alone() {
        let mut feed = Feed::new(|| None);
        let mut dst = vec![3; 10];
        assert_eq!(feed.fill(1, (64, 36), Format::Nv12, &mut dst), Shown::Placeholder);
        assert!(dst.iter().all(|&v| v == 3));
    }
}
