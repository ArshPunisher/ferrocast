//! The virtual camera's frames in shared memory, between Ferrocast (the one writer) and the
//! camera DLL in other apps or in Windows' camera frame server (the readers).
//!
//! The layout: a 4 KiB header, then [`SLOTS`] slots of one NV12 frame each, at most
//! [`MAX_W`]×[`MAX_H`]. Each slot has a sequence number used as a seqlock: odd while the writer
//! fills it, even when it's done. A reader copies a slot and checks the number didn't move, so
//! it never shows a torn frame, and the writer never waits for a reader. With three slots the
//! writer only reuses the slot a reader is copying when that reader is two frames late; the
//! reader then tries again.
//!
//! A heartbeat (the writer's wall clock, in ms) tells readers the writer is alive: older than
//! [`STALE_MS`], or a writer that said it stopped, and readers show [`pic::placeholder`]
//! instead of a frozen frame.

#![allow(unsafe_code)]

pub mod caps;
pub mod feed;
pub mod pic;
pub mod regdata;
#[cfg(windows)]
pub mod win;

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// "FCAM".
pub const MAGIC: u32 = u32::from_le_bytes(*b"FCAM");
/// The layout's version: a reader of another one shows the placeholder.
pub const VERSION: u32 = 1;
/// The header's size: the frames start here.
pub const HEADER: usize = 4096;
/// Frames kept.
pub const SLOTS: usize = 3;
/// The biggest frame (the canvas is fitted inside it).
pub const MAX_W: u32 = 1920;
pub const MAX_H: u32 = 1080;
/// One slot: an NV12 frame of the biggest size, rounded up to 4 KiB.
pub const SLOT_BYTES: usize = (MAX_W as usize * MAX_H as usize * 3 / 2).div_ceil(4096) * 4096;
/// The whole region.
pub const SIZE: usize = HEADER + SLOTS * SLOT_BYTES;
/// A writer silent this long is gone.
pub const STALE_MS: u64 = 1000;
/// Where DirectShow readers (in the user's own apps) find the frames.
pub const NAME_LOCAL: &str = "Local\\FerrocastCamera";
/// Where the Media Foundation reader (in the frame server, session 0) finds them: it makes this
/// one itself, as the app can't make `Global\` objects.
pub const NAME_GLOBAL: &str = "Global\\FerrocastCameraMF";

// Header offsets.
const O_MAGIC: usize = 0;
const O_VERSION: usize = 4;
const O_HEADER: usize = 8;
const O_SLOTS: usize = 12;
const O_SLOT_BYTES: usize = 16;
const O_MAX_W: usize = 20;
const O_MAX_H: usize = 24;
const O_LATEST: usize = 32;
const O_HEARTBEAT: usize = 40;
const O_STOPPED: usize = 48;
const O_PID: usize = 52;
const O_SLOT0: usize = 64;
const SLOT_DESC: usize = 64;
// In a slot's descriptor.
const S_SEQ: usize = 0;
const S_W: usize = 8;
const S_H: usize = 12;
const S_FPS_NUM: usize = 16;
const S_FPS_DEN: usize = 20;
const S_TS: usize = 24;

/// The camera's name in every app.
pub const FRIENDLY_NAME: &str = "Ferrocast Camera";
/// The DirectShow source filter's class.
pub const CLSID_DSHOW: u128 = 0x9E5C_C468_3A73_45C7_AE12_0E58_4A9A_4E36;
/// The Media Foundation media source's class (made a camera by `MFCreateVirtualCamera`).
pub const CLSID_MF: u128 = 0x42F4_0093_E449_4B73_9730_7D6C_B577_3F8A;

/// A GUID as the registry writes it: `{9E5CC468-3A73-45C7-AE12-0E584A9A4E36}`.
pub fn guid_text(g: u128) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:04X}-{:012X}}}",
        (g >> 96) as u32,
        (g >> 80) as u16,
        (g >> 64) as u16,
        (g >> 48) as u16,
        g & 0xFFFF_FFFF_FFFF
    )
}

/// A GUID's 16 bytes as Windows lays it out in memory (the first three fields little-endian).
pub fn guid_bytes(g: u128) -> [u8; 16] {
    let b = g.to_be_bytes();
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&[b[3], b[2], b[1], b[0]]);
    out[4..6].copy_from_slice(&[b[5], b[4]]);
    out[6..8].copy_from_slice(&[b[7], b[6]]);
    out[8..].copy_from_slice(&b[8..]);
    out
}

/// The writer's clock: milliseconds since 1970 (both sides read the same wall clock).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The camera's size for a canvas: fitted inside [`MAX_W`]×[`MAX_H`], its shape kept, even
/// sides (NV12), never smaller than 2×2.
pub fn camera_size(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (MAX_W, MAX_H);
    }
    let f = (f64::from(MAX_W) / f64::from(width)).min(f64::from(MAX_H) / f64::from(height)).min(1.0);
    let side = |v: u32| (((f64::from(v) * f).round() as u32) & !1).max(2);
    (side(width), side(height))
}

/// Bytes in an NV12 frame of `w`×`h`.
pub fn nv12_len(w: u32, h: u32) -> usize {
    w as usize * h as usize * 3 / 2
}

enum Backing {
    /// Owned memory (tests, and a reader with nothing shared yet).
    Heap(#[allow(dead_code)] Box<[u64]>),
    /// Someone else's; they keep it alive.
    Raw,
    /// A file mapping of ours, let go with the region.
    #[cfg(windows)]
    Mapped(#[allow(dead_code)] win::Mapped),
}

/// The shared memory: every access to it goes through here, checked against its length.
pub struct Region {
    base: *mut u8,
    len: usize,
    _backing: Backing,
}

impl std::fmt::Debug for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Region").field("len", &self.len).finish()
    }
}

// SAFETY: the memory is only touched through atomics and seqlocked copies, which is what shared
// memory between processes needs anyway; the pointer stays valid for the Region's life.
unsafe impl Send for Region {}
// SAFETY: as above.
unsafe impl Sync for Region {}

impl Region {
    /// Zeroed memory of its own, [`SIZE`] long (8-byte aligned).
    pub fn heap() -> Self {
        Self::heap_of(SIZE)
    }

    fn heap_of(len: usize) -> Self {
        let mut words = vec![0u64; len.div_ceil(8)].into_boxed_slice();
        let base = words.as_mut_ptr().cast::<u8>();
        Region { base, len, _backing: Backing::Heap(words) }
    }

    /// Memory someone else owns (a mapped view).
    ///
    /// # Safety
    /// `base` must be valid for reads and writes of `len` bytes, 8-byte aligned, for as long as
    /// the Region lives.
    pub unsafe fn from_raw(base: *mut u8, len: usize) -> Self {
        Region { base, len, _backing: Backing::Raw }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn u32_at(&self, at: usize) -> &AtomicU32 {
        assert!(at.is_multiple_of(4) && at + 4 <= self.len.min(HEADER), "header u32 at {at}");
        // SAFETY: in bounds and aligned (checked); the memory lives as long as self.
        unsafe { &*self.base.add(at).cast::<AtomicU32>() }
    }

    fn u64_at(&self, at: usize) -> &AtomicU64 {
        assert!(at.is_multiple_of(8) && at + 8 <= self.len.min(HEADER), "header u64 at {at}");
        // SAFETY: in bounds and aligned (checked); the memory lives as long as self.
        unsafe { &*self.base.add(at).cast::<AtomicU64>() }
    }

    fn slot_data(&self, slot: usize) -> usize {
        HEADER + slot * SLOT_BYTES
    }

    /// Copies `src` into the region at `at`.
    fn put(&self, at: usize, src: &[u8]) {
        assert!(at.checked_add(src.len()).is_some_and(|end| end <= self.len), "write in bounds");
        // SAFETY: in bounds (checked); a reader racing this copy is caught by the seqlock.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.base.add(at), src.len()) };
    }

    /// Copies `dst.len()` bytes from the region at `at`.
    fn get(&self, at: usize, dst: &mut [u8]) {
        assert!(at.checked_add(dst.len()).is_some_and(|end| end <= self.len), "read in bounds");
        // SAFETY: in bounds (checked); a writer racing this copy is caught by the seqlock.
        unsafe { std::ptr::copy_nonoverlapping(self.base.add(at), dst.as_mut_ptr(), dst.len()) };
    }

    fn desc(&self, slot: usize) -> usize {
        O_SLOT0 + slot * SLOT_DESC
    }

    /// The header says this is our layout, whole.
    pub fn valid(&self) -> bool {
        self.len >= SIZE
            && self.u32_at(O_MAGIC).load(Ordering::Acquire) == MAGIC
            && self.u32_at(O_VERSION).load(Ordering::Relaxed) == VERSION
            && self.u32_at(O_HEADER).load(Ordering::Relaxed) as usize == HEADER
            && self.u32_at(O_SLOTS).load(Ordering::Relaxed) as usize == SLOTS
            && self.u32_at(O_SLOT_BYTES).load(Ordering::Relaxed) as usize == SLOT_BYTES
            && self.u32_at(O_MAX_W).load(Ordering::Relaxed) == MAX_W
            && self.u32_at(O_MAX_H).load(Ordering::Relaxed) == MAX_H
    }

    /// Writes our layout's header if it isn't there (a region made by a reader starts zeroed).
    fn init(&self) {
        if self.valid() {
            return;
        }
        self.u32_at(O_VERSION).store(VERSION, Ordering::Relaxed);
        self.u32_at(O_HEADER).store(HEADER as u32, Ordering::Relaxed);
        self.u32_at(O_SLOTS).store(SLOTS as u32, Ordering::Relaxed);
        self.u32_at(O_SLOT_BYTES).store(SLOT_BYTES as u32, Ordering::Relaxed);
        self.u32_at(O_MAX_W).store(MAX_W, Ordering::Relaxed);
        self.u32_at(O_MAX_H).store(MAX_H, Ordering::Relaxed);
        self.u64_at(O_LATEST).store(0, Ordering::Relaxed);
        for s in 0..SLOTS {
            self.u64_at(self.desc(s) + S_SEQ).store(0, Ordering::Relaxed);
        }
        // Last: a reader seeing the magic sees the rest.
        self.u32_at(O_MAGIC).store(MAGIC, Ordering::Release);
    }
}

/// Why a frame wasn't written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// Odd sides, none, or bigger than [`MAX_W`]×[`MAX_H`].
    BadSize(u32, u32),
    /// The planes don't hold a `w`×`h` NV12 frame.
    BadPlanes,
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadSize(w, h) => write!(f, "a camera frame of {w}x{h} (even sides, at most {MAX_W}x{MAX_H})"),
            Self::BadPlanes => f.write_str("the frame's planes don't match its size"),
        }
    }
}

impl std::error::Error for WriteError {}

/// The one writer (Ferrocast).
#[derive(Debug)]
pub struct Writer {
    r: Region,
    /// Frames written (the next one's number).
    n: u64,
}

impl Writer {
    /// Takes the region, writing the header if it isn't there yet. One made by a reader (the
    /// Media Foundation one) carries on from its frame count.
    pub fn new(r: Region) -> Self {
        r.init();
        let n = r.u64_at(O_LATEST).load(Ordering::Acquire);
        r.u32_at(O_PID).store(std::process::id(), Ordering::Relaxed);
        r.u32_at(O_STOPPED).store(0, Ordering::Release);
        Writer { r, n }
    }

    /// Writes one NV12 frame: `y` is `w`×`h`, `uv` is `w`×`h/2` (rows packed).
    pub fn write(
        &mut self,
        (w, h): (u32, u32),
        fps: (u32, u32),
        ts_ns: u64,
        y: &[u8],
        uv: &[u8],
        now_ms: u64,
    ) -> Result<(), WriteError> {
        if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 || w > MAX_W || h > MAX_H {
            return Err(WriteError::BadSize(w, h));
        }
        let ylen = w as usize * h as usize;
        if y.len() != ylen || uv.len() != ylen / 2 {
            return Err(WriteError::BadPlanes);
        }
        let slot = (self.n % SLOTS as u64) as usize;
        let d = self.r.desc(slot);
        let seq = self.r.u64_at(d + S_SEQ);
        // Odd: being written. Readers that copy now see it move and try again.
        seq.store(2 * self.n + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        self.r.u32_at(d + S_W).store(w, Ordering::Relaxed);
        self.r.u32_at(d + S_H).store(h, Ordering::Relaxed);
        self.r.u32_at(d + S_FPS_NUM).store(fps.0.max(1), Ordering::Relaxed);
        self.r.u32_at(d + S_FPS_DEN).store(fps.1.max(1), Ordering::Relaxed);
        self.r.u64_at(d + S_TS).store(ts_ns, Ordering::Relaxed);
        let at = self.r.slot_data(slot);
        self.r.put(at, y);
        self.r.put(at + ylen, uv);
        // Even: done.
        seq.store(2 * self.n + 2, Ordering::Release);
        self.n += 1;
        self.r.u64_at(O_LATEST).store(self.n, Ordering::Release);
        self.beat(now_ms);
        Ok(())
    }

    /// Says the writer is alive (between frames: a still canvas still beats).
    pub fn beat(&self, now_ms: u64) {
        self.r.u32_at(O_STOPPED).store(0, Ordering::Relaxed);
        self.r.u64_at(O_HEARTBEAT).store(now_ms.max(1), Ordering::Release);
    }

    /// Says the camera is off: readers show the placeholder at once.
    pub fn stop(&self) {
        self.r.u32_at(O_STOPPED).store(1, Ordering::Release);
        self.r.u64_at(O_HEARTBEAT).store(0, Ordering::Release);
    }

    pub fn frames(&self) -> u64 {
        self.n
    }

    /// Stops, and clears every frame from the region: for a region other accounts can open
    /// (the frame server's), so no picture is left in it after the camera is off.
    pub fn wipe(&self) {
        self.stop();
        for slot in 0..SLOTS {
            // Odd while cleared, so a reader racing this sees nothing whole.
            let seq = self.r.u64_at(self.r.desc(slot) + S_SEQ);
            seq.store(1, Ordering::Release);
            let zero = vec![0u8; SLOT_BYTES];
            self.r.put(self.r.slot_data(slot), &zero);
            seq.store(0, Ordering::Release);
        }
        self.r.u64_at(O_LATEST).store(0, Ordering::Release);
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A frame read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    pub fps: (u32, u32),
    /// The frame's number (1 is the first).
    pub seq: u64,
    pub ts_ns: u64,
}

/// What a read found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    /// A new frame, copied out.
    Frame(FrameInfo),
    /// The writer is alive and the frame asked about is still the latest.
    Same,
    /// No writer (never started, stopped, gone quiet, or another layout): show the placeholder.
    Absent,
}

/// A reader (the camera DLL, in any number of apps).
#[derive(Debug)]
pub struct Reader {
    r: Region,
}

impl Reader {
    pub fn new(r: Region) -> Self {
        Reader { r }
    }

    /// For the side that makes the region (Media Foundation's): writes the header so the
    /// writer finds the layout, and so a reader before any writer sees "absent".
    pub fn new_made(r: Region) -> Self {
        r.init();
        Reader { r }
    }

    /// The writer is there and beat within [`STALE_MS`] of `now_ms`.
    pub fn alive(&self, now_ms: u64) -> bool {
        if !self.r.valid() || self.r.u32_at(O_STOPPED).load(Ordering::Acquire) != 0 {
            return false;
        }
        let beat = self.r.u64_at(O_HEARTBEAT).load(Ordering::Acquire);
        // A beat a little ahead (clocks moved) still counts.
        beat != 0 && now_ms.saturating_sub(beat) <= STALE_MS && beat <= now_ms.saturating_add(STALE_MS)
    }

    /// The latest frame's size and rate without copying it (an app picking its format).
    pub fn peek(&self, now_ms: u64) -> Option<FrameInfo> {
        if !self.alive(now_ms) {
            return None;
        }
        let latest = self.r.u64_at(O_LATEST).load(Ordering::Acquire);
        if latest == 0 {
            return None;
        }
        let d = self.r.desc(((latest - 1) % SLOTS as u64) as usize);
        let info = FrameInfo {
            width: self.r.u32_at(d + S_W).load(Ordering::Relaxed),
            height: self.r.u32_at(d + S_H).load(Ordering::Relaxed),
            fps: (
                self.r.u32_at(d + S_FPS_NUM).load(Ordering::Relaxed),
                self.r.u32_at(d + S_FPS_DEN).load(Ordering::Relaxed),
            ),
            seq: latest,
            ts_ns: self.r.u64_at(d + S_TS).load(Ordering::Relaxed),
        };
        good_size(info.width, info.height).then_some(info)
    }

    /// The latest frame into `out` (resized to fit it), unless it's frame `last` already.
    pub fn read(&self, now_ms: u64, last: u64, out: &mut Vec<u8>) -> Read {
        if !self.alive(now_ms) {
            return Read::Absent;
        }
        for _ in 0..4 {
            let latest = self.r.u64_at(O_LATEST).load(Ordering::Acquire);
            if latest == 0 {
                return Read::Absent;
            }
            if latest == last {
                return Read::Same;
            }
            let n = latest - 1;
            let slot = (n % SLOTS as u64) as usize;
            let d = self.r.desc(slot);
            let seq = self.r.u64_at(d + S_SEQ);
            let before = seq.load(Ordering::Acquire);
            if before != 2 * n + 2 {
                // Already being written again (we're two frames late): the next latest.
                continue;
            }
            let (w, h) =
                (self.r.u32_at(d + S_W).load(Ordering::Relaxed), self.r.u32_at(d + S_H).load(Ordering::Relaxed));
            if !good_size(w, h) {
                return Read::Absent;
            }
            let info = FrameInfo {
                width: w,
                height: h,
                fps: (
                    self.r.u32_at(d + S_FPS_NUM).load(Ordering::Relaxed),
                    self.r.u32_at(d + S_FPS_DEN).load(Ordering::Relaxed),
                ),
                seq: latest,
                ts_ns: self.r.u64_at(d + S_TS).load(Ordering::Relaxed),
            };
            out.resize(nv12_len(w, h), 0);
            self.r.get(self.r.slot_data(slot), out);
            fence(Ordering::Acquire);
            if seq.load(Ordering::Relaxed) == before {
                return Read::Frame(info);
            }
        }
        // The writer kept lapping us: say nothing new this time.
        Read::Same
    }
}

fn good_size(w: u32, h: u32) -> bool {
    w >= 2 && h >= 2 && w.is_multiple_of(2) && h.is_multiple_of(2) && w <= MAX_W && h <= MAX_H
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    /// Another view of `r`'s memory, as a second process maps it (`r` must outlive it).
    fn view(r: &Region) -> Region {
        // SAFETY: the tests keep the owning region alive past every view.
        unsafe { Region::from_raw(r.base, r.len) }
    }

    fn frame(w: u32, h: u32, v: u8) -> (Vec<u8>, Vec<u8>) {
        (vec![v; (w * h) as usize], vec![v; (w * h / 2) as usize])
    }

    #[test]
    fn guids_are_written_as_windows_does() {
        assert_eq!(guid_text(CLSID_DSHOW), "{9E5CC468-3A73-45C7-AE12-0E584A9A4E36}");
        assert_eq!(guid_text(CLSID_MF), "{42F40093-E449-4B73-9730-7D6CB5773F8A}");
        // MEDIATYPE_Video, {73646976-0000-0010-8000-00AA00389B71}: "vids" first.
        assert_eq!(
            guid_bytes(0x7364_6976_0000_0010_8000_00AA_0038_9B71),
            [0x76, 0x69, 0x64, 0x73, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]
        );
    }

    #[test]
    fn a_camera_fits_the_canvas_inside_1080p() {
        assert_eq!(camera_size(2560, 1440), (1920, 1080));
        assert_eq!(camera_size(1920, 1080), (1920, 1080));
        assert_eq!(camera_size(1280, 720), (1280, 720), "never bigger than the canvas");
        assert_eq!(camera_size(1080, 1920), (608, 1080), "a vertical canvas keeps its shape");
        assert_eq!(camera_size(1917, 1199), (1726, 1080), "even sides");
        assert_eq!(camera_size(0, 0), (1920, 1080));
        assert_eq!(camera_size(1, 1), (2, 2));
    }

    #[test]
    fn a_frame_written_is_read_back_whole() {
        let mem = Region::heap();
        let mut w = Writer::new(view(&mem));
        let rd = Reader::new(view(&mem));
        let mut out = Vec::new();
        assert_eq!(rd.read(1000, 0, &mut out), Read::Absent, "nothing written yet");
        let (y, uv) = frame(640, 360, 7);
        assert!(w.write((640, 360), (30, 1), 99, &y, &uv, 1000).is_ok());
        let Read::Frame(info) = rd.read(1000, 0, &mut out) else { panic!("a frame") };
        assert_eq!((info.width, info.height, info.fps, info.seq, info.ts_ns), (640, 360, (30, 1), 1, 99));
        assert_eq!(out.len(), nv12_len(640, 360));
        assert!(out.iter().all(|&b| b == 7));
        assert_eq!(rd.read(1000, 1, &mut out), Read::Same, "already seen");
        assert_eq!(rd.peek(1000).map(|i| (i.width, i.seq)), Some((640, 1)));
        // The size can change while running.
        let (y, uv) = frame(1280, 720, 9);
        assert!(w.write((1280, 720), (60, 1), 100, &y, &uv, 1001).is_ok());
        let Read::Frame(info) = rd.read(1001, 1, &mut out) else { panic!("the next frame") };
        assert_eq!((info.width, info.height, info.seq), (1280, 720, 2));
        assert!(out.iter().all(|&b| b == 9));
    }

    #[test]
    fn a_writer_gone_quiet_or_stopped_shows_the_placeholder() {
        let mem = Region::heap();
        let mut w = Writer::new(view(&mem));
        let rd = Reader::new(view(&mem));
        let (y, uv) = frame(64, 36, 1);
        assert!(w.write((64, 36), (30, 1), 0, &y, &uv, 10_000).is_ok());
        let mut out = Vec::new();
        assert!(matches!(rd.read(10_500, 0, &mut out), Read::Frame(_)));
        assert_eq!(rd.read(10_000 + STALE_MS + 1, 0, &mut out), Read::Absent, "quiet a second: gone");
        w.beat(20_000);
        assert!(rd.alive(20_000), "a still canvas still beats");
        w.stop();
        assert_eq!(rd.read(20_000, 0, &mut out), Read::Absent, "stopped: at once");
        drop(w);
        assert!(!rd.alive(20_000));
    }

    #[test]
    fn a_wiped_region_holds_no_picture() {
        let mem = Region::heap();
        let mut w = Writer::new(view(&mem));
        let (y, uv) = frame(64, 36, 9);
        assert!(w.write((64, 36), (30, 1), 0, &y, &uv, 50).is_ok());
        w.wipe();
        let rd = Reader::new(view(&mem));
        assert_eq!(rd.read(50, 0, &mut Vec::new()), Read::Absent);
        let mut all = vec![1u8; SLOT_BYTES];
        mem.get(HEADER, &mut all);
        assert!(all.iter().all(|&b| b == 0), "the frame's bytes are gone");
    }

    #[test]
    fn bad_frames_are_refused() {
        let mut w = Writer::new(Region::heap());
        let (y, uv) = frame(64, 36, 1);
        assert_eq!(w.write((63, 36), (30, 1), 0, &y, &uv, 1), Err(WriteError::BadSize(63, 36)));
        assert_eq!(w.write((3840, 2160), (30, 1), 0, &y, &uv, 1), Err(WriteError::BadSize(3840, 2160)));
        assert_eq!(w.write((0, 36), (30, 1), 0, &y, &uv, 1), Err(WriteError::BadSize(0, 36)));
        assert_eq!(w.write((64, 36), (30, 1), 0, &y[1..], &uv, 1), Err(WriteError::BadPlanes));
        assert_eq!(w.frames(), 0);
    }

    #[test]
    fn another_layout_or_a_short_region_reads_as_absent() {
        let mem = Region::heap();
        let mut w = Writer::new(view(&mem));
        let (y, uv) = frame(64, 36, 1);
        assert!(w.write((64, 36), (30, 1), 0, &y, &uv, 5).is_ok());
        mem.u32_at(O_VERSION).store(VERSION + 1, Ordering::Relaxed);
        assert_eq!(Reader::new(view(&mem)).read(5, 0, &mut Vec::new()), Read::Absent);
        let short = Region::heap_of(HEADER + 10);
        assert_eq!(Reader::new(short).read(5, 0, &mut Vec::new()), Read::Absent);
    }

    #[test]
    fn a_region_made_by_the_reader_is_taken_over_by_the_writer() {
        // Media Foundation's way: the frame server makes the region, the app writes into it.
        let mem = Region::heap();
        let rd = Reader::new_made(view(&mem));
        assert!(mem.valid() && !rd.alive(1));
        let mut w = Writer::new(view(&mem));
        let (y, uv) = frame(64, 36, 4);
        assert!(w.write((64, 36), (30, 1), 0, &y, &uv, 50).is_ok());
        assert!(matches!(rd.read(50, 0, &mut Vec::new()), Read::Frame(FrameInfo { seq: 1, .. })));
    }

    #[test]
    fn a_writer_that_comes_back_carries_on_counting() {
        let mem = Region::heap();
        let mut w = Writer::new(view(&mem));
        let (y, uv) = frame(64, 36, 4);
        for _ in 0..5 {
            assert!(w.write((64, 36), (30, 1), 0, &y, &uv, 50).is_ok());
        }
        drop(w);
        // A reader that last saw frame 5 must not think frame "1" of a new writer is old.
        let mut again = Writer::new(view(&mem));
        assert_eq!(again.frames(), 5);
        assert!(again.write((64, 36), (30, 1), 0, &y, &uv, 60).is_ok());
        assert_eq!(again.frames(), 6);
    }

    /// A writer flat out and readers copying all the while: every frame read is one whole
    /// frame (all its bytes the same), never two mixed.
    #[test]
    fn readers_racing_the_writer_never_see_a_torn_frame() {
        let mem = Region::heap();
        let views: Vec<Region> = (0..3).map(|_| view(&mem)).collect();
        let done = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = views
            .into_iter()
            .map(|v| {
                let done = done.clone();
                std::thread::spawn(move || {
                    let rd = Reader::new(v);
                    let (mut out, mut last, mut whole) = (Vec::new(), 0, 0u32);
                    while !done.load(Ordering::Acquire) {
                        if let Read::Frame(info) = rd.read(now_ms(), last, &mut out) {
                            assert!(info.seq > last, "frames only go forward");
                            last = info.seq;
                            let first = out[0];
                            assert!(out.iter().all(|&b| b == first), "a torn frame at {}", info.seq);
                            assert_eq!(first, (info.seq % 251) as u8, "the frame its number says");
                            whole += 1;
                        }
                    }
                    whole
                })
            })
            .collect();
        let mut w = Writer::new(view(&mem));
        for n in 1..=600u64 {
            let (y, uv) = frame(320, 180, (n % 251) as u8);
            assert!(w.write((320, 180), (60, 1), n, &y, &uv, now_ms()).is_ok());
        }
        done.store(true, Ordering::Release);
        for t in readers {
            let whole = t.join().unwrap_or_else(|_| panic!("a reader failed"));
            assert!(whole > 0, "each reader got frames");
        }
    }
}
