//! Building blocks for sound that runs on a thread of its own (D1, `the D1 design note`):
//! a wait-free ring for one producer and one consumer, meters that are read without a lock, and a
//! clock that tests can drive. Nothing here is used by the engine yet.
//!
//! No `unsafe` in this file: the ring stores every sample as the bits of an `f32` in an atomic
//! word, so the compiler's own types do the synchronising and a reader can never see a torn value.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
#[cfg(not(target_os = "macos"))]
use std::time::Duration;

use crate::mix::Levels;

// ----------------------------------------------------------------------------------------
// The ring

/// What came with one push: how long it is, how it is laid out and when it was captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    /// Frames (one sample for each channel) in the piece.
    pub frames: u32,
    pub rate: u32,
    pub channels: u8,
    /// When the first frame was captured (the clock the capture uses), nanoseconds.
    pub ts_ns: u64,
}

impl Piece {
    /// How many samples (frames times channels) the piece holds.
    #[must_use]
    pub fn samples(&self) -> usize {
        self.frames as usize * usize::from(self.channels)
    }
}

/// Why a push did nothing. The piece is dropped, never waited for: the one pushing is a capture
/// callback that must not block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// No room: the reader is behind. The newest is the one dropped, and counted.
    Full,
    /// The piece itself is wrong: no channels, a length that isn't whole frames, a rate or length
    /// that doesn't fit, or more than the ring could ever hold.
    Invalid,
}

/// Why a pop gave nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopError {
    Empty,
    /// The buffer is too small for the next piece (`needed` samples); it is left where it was.
    TooSmall {
        needed: usize,
    },
}

/// One piece's header, in atomics: written by the producer before it publishes the piece and
/// read by the consumer after it sees it.
#[derive(Debug, Default)]
struct MetaSlot {
    ts_ns: AtomicU64,
    /// `frames` in the low 32 bits, `rate` in the next 24 and `channels` in the top 8.
    layout: AtomicU64,
}

const RATE_BITS: u32 = 24;
const MAX_RATE: u32 = (1 << RATE_BITS) - 1;

fn pack(frames: u32, rate: u32, channels: u8) -> u64 {
    u64::from(frames) | (u64::from(rate) << 32) | (u64::from(channels) << (32 + RATE_BITS))
}

fn unpack(bits: u64, ts_ns: u64) -> Piece {
    Piece {
        frames: (bits & 0xFFFF_FFFF) as u32,
        rate: ((bits >> 32) & u64::from(MAX_RATE)) as u32,
        channels: (bits >> (32 + RATE_BITS)) as u8,
        ts_ns,
    }
}

/// What the two ends share.
#[derive(Debug)]
struct Shared {
    samples: Box<[AtomicU32]>,
    meta: Box<[MetaSlot]>,
    /// Samples ever written, and ever read (they only grow; the index is the count masked).
    head: AtomicUsize,
    tail: AtomicUsize,
    /// Pieces ever published and ever taken.
    meta_head: AtomicUsize,
    meta_tail: AtomicUsize,
    /// Pushes that were dropped, and the frames in them.
    overruns: AtomicU64,
    dropped_frames: AtomicU64,
}

/// A wait-free ring of audio pieces for one producer and one consumer.
///
/// `new` makes both ends; each is `Send` and not `Clone`, so there can be only one of each. A
/// push and a pop never allocate and never wait: when the ring is full the piece pushed is
/// dropped (and counted), so a capture callback is never held up by a slow reader.
#[derive(Debug)]
pub struct SpscAudioRing;

impl SpscAudioRing {
    /// A ring holding up to `sample_capacity` samples (rounded up to a power of two) in up to
    /// `piece_capacity` pieces (likewise). This is the only place that allocates.
    #[must_use]
    #[allow(clippy::new_ret_no_self)] // it makes the two ends, not a ring to hold
    pub fn new(sample_capacity: usize, piece_capacity: usize) -> (AudioProducer, AudioConsumer) {
        let samples = sample_capacity.max(2).next_power_of_two();
        let pieces = piece_capacity.max(2).next_power_of_two();
        let shared = Arc::new(Shared {
            samples: (0..samples).map(|_| AtomicU32::new(0)).collect(),
            meta: (0..pieces).map(|_| MetaSlot::default()).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            meta_head: AtomicUsize::new(0),
            meta_tail: AtomicUsize::new(0),
            overruns: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
        });
        (AudioProducer { shared: shared.clone() }, AudioConsumer { shared })
    }

    /// Room for `seconds` of `channels`-channel sound at `rate`, in pieces of about `piece_frames`.
    #[must_use]
    pub fn for_seconds(
        seconds: usize,
        rate: u32,
        channels: usize,
        piece_frames: usize,
    ) -> (AudioProducer, AudioConsumer) {
        let frames = seconds.max(1) * rate as usize;
        Self::new(frames * channels.max(1), frames / piece_frames.max(1) + 1)
    }
}

/// The end that pushes (a capture callback).
#[derive(Debug)]
pub struct AudioProducer {
    shared: Arc<Shared>,
}

impl AudioProducer {
    /// Puts `pcm` (interleaved, `channels` of them) in the ring, stamped `ts_ns`. Never blocks,
    /// never allocates.
    ///
    /// # Errors
    /// `Full` when the reader is behind (the piece is dropped and counted); `Invalid` for a piece
    /// that can't be one.
    pub fn push(&mut self, pcm: &[f32], channels: u8, rate: u32, ts_ns: u64) -> Result<(), PushError> {
        let s = &*self.shared;
        let ch = usize::from(channels);
        let capacity = s.samples.len();
        if ch == 0
            || pcm.is_empty()
            || !pcm.len().is_multiple_of(ch)
            || rate == 0
            || rate > MAX_RATE
            || pcm.len() > capacity
        {
            return Err(PushError::Invalid);
        }
        let Ok(frames) = u32::try_from(pcm.len() / ch) else { return Err(PushError::Invalid) };
        // Only this end moves `head`, so a relaxed read of it is exact; the other end's `tail` is
        // acquired, so what it read is done with before the room is reused.
        let head = s.head.load(Ordering::Relaxed);
        let meta_head = s.meta_head.load(Ordering::Relaxed);
        let room = capacity - head.wrapping_sub(s.tail.load(Ordering::Acquire));
        let slots = s.meta.len() - meta_head.wrapping_sub(s.meta_tail.load(Ordering::Acquire));
        if pcm.len() > room || slots == 0 {
            s.overruns.fetch_add(1, Ordering::Relaxed);
            s.dropped_frames.fetch_add(u64::from(frames), Ordering::Relaxed);
            return Err(PushError::Full);
        }
        let mask = capacity - 1;
        for (i, v) in pcm.iter().enumerate() {
            s.samples[head.wrapping_add(i) & mask].store(v.to_bits(), Ordering::Relaxed);
        }
        let slot = &s.meta[meta_head & (s.meta.len() - 1)];
        slot.ts_ns.store(ts_ns, Ordering::Relaxed);
        slot.layout.store(pack(frames, rate, channels), Ordering::Relaxed);
        // Published in this order, the second `Release` carrying the first and the slot with it: a
        // consumer that sees the piece sees all of its samples.
        s.head.store(head.wrapping_add(pcm.len()), Ordering::Release);
        s.meta_head.store(meta_head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Pushes dropped for lack of room, and the frames in them, so far.
    #[must_use]
    pub fn dropped(&self) -> (u64, u64) {
        (self.shared.overruns.load(Ordering::Relaxed), self.shared.dropped_frames.load(Ordering::Relaxed))
    }

    /// How many samples there is room for now.
    #[must_use]
    pub fn room(&self) -> usize {
        let s = &*self.shared;
        s.samples.len() - s.head.load(Ordering::Relaxed).wrapping_sub(s.tail.load(Ordering::Acquire))
    }
}

/// The end that pops (the mixer).
#[derive(Debug)]
pub struct AudioConsumer {
    shared: Arc<Shared>,
}

impl AudioConsumer {
    /// What the next piece is, without taking it.
    #[must_use]
    pub fn peek(&self) -> Option<Piece> {
        let s = &*self.shared;
        let tail = s.meta_tail.load(Ordering::Relaxed);
        if tail == s.meta_head.load(Ordering::Acquire) {
            return None;
        }
        let slot = &s.meta[tail & (s.meta.len() - 1)];
        Some(unpack(slot.layout.load(Ordering::Relaxed), slot.ts_ns.load(Ordering::Relaxed)))
    }

    /// Takes the next piece into the front of `out`. Never blocks, never allocates.
    ///
    /// # Errors
    /// `Empty` when there is none; `TooSmall` when `out` can't hold it (look with [`Self::peek`]).
    pub fn pop_into(&mut self, out: &mut [f32]) -> Result<Piece, PopError> {
        let piece = self.peek().ok_or(PopError::Empty)?;
        let n = piece.samples();
        if out.len() < n {
            return Err(PopError::TooSmall { needed: n });
        }
        let s = &*self.shared;
        let tail = s.tail.load(Ordering::Relaxed);
        let mask = s.samples.len() - 1;
        for (i, o) in out[..n].iter_mut().enumerate() {
            *o = f32::from_bits(s.samples[tail.wrapping_add(i) & mask].load(Ordering::Relaxed));
        }
        // Given back only now that they are read.
        s.tail.store(tail.wrapping_add(n), Ordering::Release);
        s.meta_tail.store(s.meta_tail.load(Ordering::Relaxed).wrapping_add(1), Ordering::Release);
        Ok(piece)
    }

    /// Throws away everything waiting (a source that was paused or seeked), and says how many
    /// samples that was. A piece that arrives meanwhile is kept or dropped whole, never cut.
    pub fn clear(&mut self) -> usize {
        let mut dropped = 0;
        while let Some(piece) = self.peek() {
            let s = &*self.shared;
            let n = piece.samples();
            s.tail.store(s.tail.load(Ordering::Relaxed).wrapping_add(n), Ordering::Release);
            s.meta_tail.store(s.meta_tail.load(Ordering::Relaxed).wrapping_add(1), Ordering::Release);
            dropped += n;
        }
        dropped
    }

    /// Samples waiting.
    #[must_use]
    pub fn queued(&self) -> usize {
        let s = &*self.shared;
        s.head.load(Ordering::Acquire).wrapping_sub(s.tail.load(Ordering::Relaxed))
    }

    /// Pushes dropped for lack of room, and the frames in them, so far.
    #[must_use]
    pub fn dropped(&self) -> (u64, u64) {
        (self.shared.overruns.load(Ordering::Relaxed), self.shared.dropped_frames.load(Ordering::Relaxed))
    }
}

// ----------------------------------------------------------------------------------------
// Meters

/// A source's meters, written by the thread that mixes and read by anyone, without a lock: each
/// pair (left and right) is one atomic word, so a reader never sees one channel from one block and
/// the other from the next.
#[derive(Debug)]
pub struct AtomicLevels {
    peak: AtomicU64,
    rms: AtomicU64,
    input_peak: AtomicU64,
    /// The same three over a few blocks only (the dashboard's meters).
    fast_peak: AtomicU64,
    fast_rms: AtomicU64,
    fast_input_peak: AtomicU64,
    gated: std::sync::atomic::AtomicBool,
    speech: std::sync::atomic::AtomicBool,
    fast_speech: std::sync::atomic::AtomicBool,
    /// When the mixing thread last looked at this source (host clock, ns; 0: never).
    seen_ns: AtomicU64,
}

/// How long a source's meters may go unwritten before a reader takes them as silence (ns).
pub const METER_FRESH_NS: u64 = 500_000_000;

fn pair(v: [f32; 2]) -> u64 {
    u64::from(v[0].to_bits()) | (u64::from(v[1].to_bits()) << 32)
}

fn unpair(bits: u64) -> [f32; 2] {
    [f32::from_bits((bits & 0xFFFF_FFFF) as u32), f32::from_bits((bits >> 32) as u32)]
}

impl Default for AtomicLevels {
    fn default() -> Self {
        let silent = Levels::default();
        Self {
            peak: AtomicU64::new(pair(silent.peak)),
            rms: AtomicU64::new(pair(silent.rms)),
            input_peak: AtomicU64::new(pair(silent.input_peak)),
            fast_peak: AtomicU64::new(pair(silent.peak)),
            fast_rms: AtomicU64::new(pair(silent.rms)),
            fast_input_peak: AtomicU64::new(pair(silent.input_peak)),
            gated: std::sync::atomic::AtomicBool::new(false),
            speech: std::sync::atomic::AtomicBool::new(false),
            fast_speech: std::sync::atomic::AtomicBool::new(false),
            seen_ns: AtomicU64::new(0),
        }
    }
}

impl AtomicLevels {
    /// Meters at silence, to share (`Arc`) between the writer and the readers.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Publishes the levels of the block just mixed.
    pub fn store(&self, l: &Levels) {
        self.peak.store(pair(l.peak), Ordering::Relaxed);
        self.rms.store(pair(l.rms), Ordering::Relaxed);
        self.input_peak.store(pair(l.input_peak), Ordering::Relaxed);
        self.gated.store(l.gated, Ordering::Relaxed);
        self.speech.store(l.speech, Ordering::Relaxed);
    }

    /// Publishes the fast (few-block) levels.
    pub fn store_fast(&self, l: &Levels) {
        self.fast_peak.store(pair(l.peak), Ordering::Relaxed);
        self.fast_rms.store(pair(l.rms), Ordering::Relaxed);
        self.fast_input_peak.store(pair(l.input_peak), Ordering::Relaxed);
        self.fast_speech.store(l.speech, Ordering::Relaxed);
    }

    /// The fast levels last published: three atomic loads.
    #[must_use]
    pub fn load_fast(&self) -> Levels {
        Levels {
            peak: unpair(self.fast_peak.load(Ordering::Relaxed)),
            rms: unpair(self.fast_rms.load(Ordering::Relaxed)),
            input_peak: unpair(self.fast_input_peak.load(Ordering::Relaxed)),
            gated: self.gated.load(Ordering::Relaxed),
            speech: self.fast_speech.load(Ordering::Relaxed),
        }
    }

    /// Records that the mixing thread looked at this source at `now_ns`, whether or not a block
    /// was mixed for it. A reader that finds this old knows the meters are no longer being kept.
    pub fn touch(&self, now_ns: u64) {
        self.seen_ns.store(now_ns, Ordering::Relaxed);
    }

    fn is_fresh(&self, now_ns: u64) -> bool {
        let seen = self.seen_ns.load(Ordering::Relaxed);
        seen != 0 && now_ns.saturating_sub(seen) <= METER_FRESH_NS
    }

    /// [`AtomicLevels::load`], or silence when the mixing thread has not looked at this source
    /// for [`METER_FRESH_NS`] (its sound stopped or the source is no longer mixed: the last loud
    /// block must not stay on a meter). Live levels are back with the next write.
    #[must_use]
    pub fn load_fresh(&self, now_ns: u64) -> Levels {
        if self.is_fresh(now_ns) { self.load() } else { Levels::default() }
    }

    /// [`AtomicLevels::load_fast`] with the same freshness rule as [`AtomicLevels::load_fresh`].
    #[must_use]
    pub fn load_fast_fresh(&self, now_ns: u64) -> Levels {
        if self.is_fresh(now_ns) { self.load_fast() } else { Levels::default() }
    }

    /// The levels last published: three atomic loads.
    #[must_use]
    pub fn load(&self) -> Levels {
        Levels {
            peak: unpair(self.peak.load(Ordering::Relaxed)),
            rms: unpair(self.rms.load(Ordering::Relaxed)),
            input_peak: unpair(self.input_peak.load(Ordering::Relaxed)),
            gated: self.gated.load(Ordering::Relaxed),
            speech: self.speech.load(Ordering::Relaxed),
        }
    }
}

// ----------------------------------------------------------------------------------------
// Clocks

/// The clock a loop that ticks at a steady rate asks the time of, and sleeps by: the real one in
/// the app, a [`SimClock`] in tests (hours of ticks in a moment, stalls made on purpose).
pub trait AudioClock: Send + Sync {
    /// Now, in nanoseconds: the same clock capture times use.
    fn now_ns(&self) -> u64;

    /// Returns once `now_ns` has reached `target_ns` (at once if it already has).
    fn sleep_until_ns(&self, target_ns: u64);
}

/// The host clock ([`crate::engine::now_ns`]), slept by `std`: coarse until the last millisecond,
/// then yielding, so a wake is not up to a scheduler tick late.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl AudioClock for SystemClock {
    fn now_ns(&self) -> u64 {
        crate::engine::now_ns()
    }

    fn sleep_until_ns(&self, target_ns: u64) {
        // The last stretch is yielded, then spun: the OS wakes a coarse sleep a little late.
        #[cfg(target_os = "windows")]
        const SPIN_NS: u64 = 1_500_000;
        #[cfg(target_os = "macos")]
        const SPIN_NS: u64 = 60_000;
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        const SPIN_NS: u64 = 1_000_000;
        const BUSY_SPIN_NS: u64 = 30_000;
        loop {
            let now = self.now_ns();
            if now >= target_ns {
                return;
            }
            let left = target_ns - now;
            if left > SPIN_NS + SPIN_NS / 2 {
                // macOS: `mach_wait_until`, precise and not a busy wait (a plain sleep's timer slack
                // grows with its length: 20 ms woke 7-10 ms late, 4.5 ms at the median for a block).
                #[cfg(target_os = "macos")]
                crate::apple::wait_until_ns(target_ns - SPIN_NS);
                // Windows: the high-resolution timer wakes within ~0.5 ms: sleep to the margin.
                #[cfg(target_os = "windows")]
                crate::sleep_precise(Duration::from_nanos(left - SPIN_NS));
                // Elsewhere: half of what is left never passes the target.
                #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                crate::sleep_precise(Duration::from_nanos(left / 2));
            } else if left > BUSY_SPIN_NS {
                std::thread::yield_now();
            } else {
                std::hint::spin_loop();
            }
        }
    }
}

/// A clock that moves only when it is told to (or slept past): shared by every clone, never goes
/// back.
#[derive(Debug, Clone, Default)]
pub struct SimClock {
    now: Arc<AtomicU64>,
}

impl SimClock {
    /// At `start_ns`.
    #[must_use]
    pub fn at(start_ns: u64) -> Self {
        Self { now: Arc::new(AtomicU64::new(start_ns)) }
    }

    /// Moves it on by `ns` (a stall made on purpose).
    pub fn advance(&self, ns: u64) {
        self.now.fetch_add(ns, Ordering::AcqRel);
    }
}

impl AudioClock for SimClock {
    fn now_ns(&self) -> u64 {
        self.now.load(Ordering::Acquire)
    }

    /// Sleeping is the time going by: the clock is brought to `target_ns` (never back).
    fn sleep_until_ns(&self, target_ns: u64) {
        self.now.fetch_max(target_ns, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A piece of `frames` stereo frames whose samples are `base`, `base + 1`, …
    fn counting(base: u32, frames: usize) -> Vec<f32> {
        (0..frames * 2).map(|i| (base + i as u32) as f32).collect()
    }

    #[test]
    fn a_piece_comes_out_as_it_went_in_with_its_header() {
        let (mut p, mut c) = SpscAudioRing::new(64, 4);
        assert_eq!(c.peek(), None);
        let pcm = counting(0, 5);
        p.push(&pcm, 2, 48_000, 123_456_789).unwrap();
        let want = Piece { frames: 5, rate: 48_000, channels: 2, ts_ns: 123_456_789 };
        assert_eq!(c.peek(), Some(want));
        let mut out = [0.0; 16];
        assert_eq!(c.pop_into(&mut out), Ok(want));
        assert_eq!(&out[..10], &pcm[..]);
        assert_eq!(c.pop_into(&mut out), Err(PopError::Empty));
        // Mono, an odd rate and the biggest header values survive the packing.
        let big = Piece { frames: 3, rate: MAX_RATE, channels: 255, ts_ns: u64::MAX };
        let (mut p, c) = SpscAudioRing::new(1024, 4);
        p.push(&vec![0.5; 3 * 255], 255, MAX_RATE, u64::MAX).unwrap();
        assert_eq!(c.peek(), Some(big));
    }

    #[test]
    fn a_buffer_too_small_leaves_the_piece_where_it_is() {
        let (mut p, mut c) = SpscAudioRing::new(64, 4);
        p.push(&counting(0, 4), 2, 44_100, 1).unwrap();
        let mut small = [0.0; 3];
        assert_eq!(c.pop_into(&mut small), Err(PopError::TooSmall { needed: 8 }));
        assert_eq!(c.queued(), 8);
        let mut out = [0.0; 8];
        assert!(c.pop_into(&mut out).is_ok());
        assert_eq!(out[7], 7.0);
    }

    #[test]
    fn pieces_that_cannot_be_are_refused() {
        let (mut p, _c) = SpscAudioRing::new(8, 4);
        assert_eq!(p.push(&[0.0; 4], 0, 48_000, 0), Err(PushError::Invalid), "no channels");
        assert_eq!(p.push(&[], 2, 48_000, 0), Err(PushError::Invalid), "nothing");
        assert_eq!(p.push(&[0.0; 3], 2, 48_000, 0), Err(PushError::Invalid), "not whole frames");
        assert_eq!(p.push(&[0.0; 4], 2, 0, 0), Err(PushError::Invalid), "no rate");
        assert_eq!(p.push(&[0.0; 4], 2, MAX_RATE + 1, 0), Err(PushError::Invalid), "a rate too big");
        assert_eq!(p.push(&[0.0; 10], 2, 48_000, 0), Err(PushError::Invalid), "more than it could ever hold");
        assert_eq!(p.dropped(), (0, 0), "a wrong piece isn't a full ring");
    }

    /// A full ring drops the newest, counts it, keeps what it has, and takes pieces again once
    /// the reader has made room.
    #[test]
    fn a_full_ring_drops_the_newest_and_counts_it() {
        let (mut p, mut c) = SpscAudioRing::new(16, 8);
        // 16 samples of room: two pieces of 4 stereo frames fill it.
        p.push(&counting(0, 4), 2, 48_000, 10).unwrap();
        p.push(&counting(100, 4), 2, 48_000, 20).unwrap();
        assert_eq!(p.room(), 0);
        assert_eq!(p.push(&counting(200, 4), 2, 48_000, 30), Err(PushError::Full));
        assert_eq!(p.push(&counting(300, 1), 2, 48_000, 40), Err(PushError::Full));
        assert_eq!(p.dropped(), (2, 5), "two pushes, 4 + 1 frames");
        assert_eq!(c.dropped(), (2, 5));
        let mut out = [0.0; 8];
        assert_eq!(c.pop_into(&mut out).unwrap().ts_ns, 10);
        assert_eq!(out[0], 0.0);
        assert_eq!(c.pop_into(&mut out).unwrap().ts_ns, 20);
        assert_eq!(out[0], 100.0, "the oldest are the ones kept");
        assert_eq!(c.pop_into(&mut out), Err(PopError::Empty));
        p.push(&counting(400, 4), 2, 48_000, 50).unwrap();
        assert_eq!(c.pop_into(&mut out).unwrap().ts_ns, 50);
        // Pieces, not samples, can run out too.
        let (mut p, _c) = SpscAudioRing::new(1024, 2);
        p.push(&[0.0; 2], 2, 48_000, 0).unwrap();
        p.push(&[0.0; 2], 2, 48_000, 0).unwrap();
        assert_eq!(p.push(&[0.0; 2], 2, 48_000, 0), Err(PushError::Full));
    }

    #[test]
    fn clear_drops_what_is_waiting_and_keeps_the_ring_working() {
        let (mut p, mut c) = SpscAudioRing::new(32, 4);
        p.push(&counting(0, 3), 2, 48_000, 1).unwrap();
        p.push(&counting(50, 3), 2, 48_000, 2).unwrap();
        assert_eq!(c.clear(), 12);
        assert_eq!((c.queued(), c.peek()), (0, None));
        p.push(&counting(90, 2), 2, 48_000, 3).unwrap();
        let mut out = [0.0; 4];
        assert_eq!(c.pop_into(&mut out).unwrap().ts_ns, 3);
        assert_eq!(out, [90.0, 91.0, 92.0, 93.0]);
    }

    /// A producer thread and a consumer thread, a ring so small both indices wrap thousands of
    /// times and pieces straddle the end: every piece arrives whole, in order, none lost or torn.
    #[test]
    fn a_producer_and_a_consumer_wrap_around_without_losing_or_tearing_anything() {
        const PIECES: u32 = 100_000;
        let (mut p, mut c) = SpscAudioRing::new(64, 8);
        let producer = std::thread::spawn(move || {
            let mut x = 12345u32;
            for n in 0..PIECES {
                // Lengths of 1 to 12 frames, so the pieces never line up with the ring.
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let frames = 1 + (x >> 24) as usize % 12;
                let pcm: Vec<f32> = (0..frames * 2).map(|i| (n * 100 + i as u32) as f32).collect();
                // The ring drops when full; this test wants every piece, so it tries again.
                while p.push(&pcm, 2, 48_000, u64::from(n)).is_err() {
                    std::thread::yield_now();
                }
            }
        });
        let mut out = [0.0f32; 24];
        let mut next = 0u32;
        while next < PIECES {
            match c.pop_into(&mut out) {
                Ok(piece) => {
                    assert_eq!(piece.ts_ns, u64::from(next), "in order");
                    for (i, v) in out[..piece.samples()].iter().enumerate() {
                        assert_eq!(*v, (next * 100 + i as u32) as f32, "piece {next} sample {i}");
                    }
                    next += 1;
                }
                Err(PopError::Empty) => std::thread::yield_now(),
                Err(e) => panic!("{e:?}"),
            }
        }
        producer.join().unwrap();
        assert_eq!(c.queued(), 0);
    }

    /// Pushing and popping don't grow anything: the storage is what `new` made it, whatever
    /// goes through (the allocator is counted in `tests/rt_alloc.rs`).
    #[test]
    fn pushing_and_popping_use_no_more_room_than_the_ring_was_made_with() {
        let (mut p, mut c) = SpscAudioRing::for_seconds(1, 48_000, 2, 1024);
        let (samples, meta) = (p.shared.samples.len(), p.shared.meta.len());
        assert!(samples >= 96_000 && samples.is_power_of_two());
        let pcm = vec![0.25f32; 2048];
        let mut out = vec![0.0f32; 2048];
        for n in 0..10_000u64 {
            p.push(&pcm, 2, 48_000, n).unwrap();
            c.pop_into(&mut out).unwrap();
        }
        assert_eq!((p.shared.samples.len(), p.shared.meta.len()), (samples, meta));
        assert_eq!(p.dropped(), (0, 0));
    }

    #[test]
    fn levels_come_back_as_they_went_in_silence_included() {
        let m = AtomicLevels::shared();
        assert_eq!(m.load(), Levels::default(), "silent to start");
        let l = Levels {
            peak: [-3.5, -6.25],
            rms: [-20.0, f32::NEG_INFINITY],
            input_peak: [-0.1, -12.0],
            gated: true,
            speech: true,
        };
        m.store(&l);
        assert_eq!(m.load(), l);
        m.store_fast(&Levels { speech: false, ..l });
        assert!(!m.load_fast().speech);
        m.store_fast(&l);
        assert!(m.load_fast().speech);
        // Written on one thread and read on another: always a whole pair, never a mix of two writes.
        let (a, b) = (Levels { peak: [-1.0, -1.0], ..l }, Levels { peak: [-9.0, -9.0], ..l });
        let writer = {
            let m = m.clone();
            std::thread::spawn(move || {
                for i in 0..50_000 {
                    m.store(if i % 2 == 0 { &a } else { &b });
                }
            })
        };
        for _ in 0..50_000 {
            let got = m.load().peak;
            assert!(got == l.peak || got == a.peak || got == b.peak, "a pair that was never written: {got:?}");
        }
        writer.join().unwrap();
    }

    #[test]
    fn meters_nobody_writes_read_as_silence_until_they_are_written_again() {
        let m = AtomicLevels::shared();
        let loud = Levels { peak: [-3.0; 2], rms: [-9.0; 2], input_peak: [-3.0; 2], gated: true, speech: true };
        m.store(&loud);
        m.store_fast(&loud);
        let t = 5_000_000_000;
        assert_eq!(m.load_fresh(t), Levels::default(), "never looked at: silence");
        m.touch(t);
        assert_eq!(m.load_fresh(t + METER_FRESH_NS), loud, "inside the timeout");
        assert_eq!(m.load_fast_fresh(t + METER_FRESH_NS), loud);
        assert_eq!(
            m.load_fresh(t + METER_FRESH_NS + 1),
            Levels::default(),
            "past it: silence, whatever was last written"
        );
        assert_eq!(m.load_fast_fresh(t + METER_FRESH_NS + 1), Levels::default());
        assert_eq!(m.load(), loud, "the raw read is untouched");
        m.touch(t + 2 * METER_FRESH_NS);
        assert_eq!(m.load_fresh(t + 2 * METER_FRESH_NS), loud, "live again as soon as it is written");
        assert_eq!(m.load_fresh(t), loud, "a read from before the last write is never stale");
    }

    #[test]
    fn the_sim_clock_only_goes_forward_and_sleeping_is_time_passing() {
        let c = SimClock::at(1_000);
        assert_eq!(c.now_ns(), 1_000);
        c.sleep_until_ns(5_000);
        assert_eq!(c.now_ns(), 5_000);
        c.sleep_until_ns(2_000);
        assert_eq!(c.now_ns(), 5_000, "never back");
        c.advance(500);
        assert_eq!(c.now_ns(), 5_500);
        // Shared by every clone, from any thread, and monotonic as seen by each.
        let d = c.clone();
        let t = std::thread::spawn(move || {
            for i in 0..10_000 {
                d.advance(1);
                d.sleep_until_ns(i);
            }
        });
        let mut last = 0;
        for _ in 0..10_000 {
            let now = c.now_ns();
            assert!(now >= last);
            last = now;
        }
        t.join().unwrap();
        assert_eq!(c.now_ns(), 5_500 + 10_000);
    }

    #[test]
    fn the_system_clock_is_monotonic_and_sleeps_to_its_target() {
        let c = SystemClock;
        let a = c.now_ns();
        let target = a + 5_000_000;
        c.sleep_until_ns(target);
        let b = c.now_ns();
        assert!(b >= target, "{b} < {target}");
        assert!(b - target < 50_000_000, "woke {} ms late", (b - target) / 1_000_000);
        c.sleep_until_ns(a);
        assert!(c.now_ns() >= b, "a target in the past returns at once");
    }

    /// On Windows, `SystemClock::sleep_until_ns` must hold a 21.333 ms audio cadence via high-res
    /// waitable timers instead of slipping onto the 15.625 ms default `Sleep` tick (~10 ms late
    /// every block).
    #[cfg(windows)]
    #[test]
    fn system_clock_holds_audio_block_cadence_on_windows() {
        let c = SystemClock;
        let step_ns = 21_333_333u64;
        let mut target = c.now_ns() + step_ns;
        let mut overshoots_ns = Vec::with_capacity(15);
        for _ in 0..15 {
            c.sleep_until_ns(target);
            let now = c.now_ns();
            assert!(now >= target, "woke early: {now} < {target}");
            overshoots_ns.push(now - target);
            target = now.max(target) + step_ns;
        }
        overshoots_ns.sort_unstable();
        let median = overshoots_ns[overshoots_ns.len() / 2];
        assert!(
            median < 5_000_000,
            "median audio wakeup overshoot was {} ms (stuck on 15.6 ms timer tick?): {overshoots_ns:?}",
            median / 1_000_000
        );
    }
}

/// A map from source id to `V` that adding or removing an entry doesn't allocate in, up to the
/// room made when it was created: two sorted vectors, not tree nodes (a `BTreeMap` insert
/// allocates a node). The audio thread keeps its sources, rings and levels in these, so a source
/// added while live costs no allocation there. Past its room it grows like a `Vec` (rare: the
/// room is [`PreMap::ROOM`] entries).
pub struct PreMap<V> {
    keys: Vec<u64>,
    vals: Vec<V>,
}

/// Shown as a map (`{1: .., 2: ..}`), as the `BTreeMap` it replaces was (tests read it).
impl<V: std::fmt::Debug> std::fmt::Debug for PreMap<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<V> Default for PreMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> PreMap<V> {
    /// Entries there is room for without growing.
    pub const ROOM: usize = 256;

    pub fn new() -> Self {
        Self { keys: Vec::with_capacity(Self::ROOM), vals: Vec::with_capacity(Self::ROOM) }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn contains_key(&self, k: &u64) -> bool {
        self.keys.binary_search(k).is_ok()
    }

    pub fn get(&self, k: &u64) -> Option<&V> {
        self.keys.binary_search(k).ok().map(|i| &self.vals[i])
    }

    pub fn get_mut(&mut self, k: &u64) -> Option<&mut V> {
        self.keys.binary_search(k).ok().map(|i| &mut self.vals[i])
    }

    /// Puts `v` at `k`; what was there is handed back (drop it off the audio thread).
    pub fn insert(&mut self, k: u64, v: V) -> Option<V> {
        match self.keys.binary_search(&k) {
            Ok(i) => Some(std::mem::replace(&mut self.vals[i], v)),
            Err(i) => {
                self.keys.insert(i, k);
                self.vals.insert(i, v);
                None
            }
        }
    }

    pub fn remove(&mut self, k: &u64) -> Option<V> {
        let i = self.keys.binary_search(k).ok()?;
        self.keys.remove(i);
        Some(self.vals.remove(i))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&u64, &V)> {
        self.keys.iter().zip(self.vals.iter())
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&u64, &mut V)> {
        self.keys.iter().zip(self.vals.iter_mut())
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.vals.iter()
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.vals.iter_mut()
    }

    pub fn keys(&self) -> impl Iterator<Item = &u64> {
        self.keys.iter()
    }
}

impl<'a, V> IntoIterator for &'a PreMap<V> {
    type Item = (&'a u64, &'a V);
    type IntoIter = std::iter::Zip<std::slice::Iter<'a, u64>, std::slice::Iter<'a, V>>;
    fn into_iter(self) -> Self::IntoIter {
        self.keys.iter().zip(self.vals.iter())
    }
}

impl<'a, V> IntoIterator for &'a mut PreMap<V> {
    type Item = (&'a u64, &'a mut V);
    type IntoIter = std::iter::Zip<std::slice::Iter<'a, u64>, std::slice::IterMut<'a, V>>;
    fn into_iter(self) -> Self::IntoIter {
        self.keys.iter().zip(self.vals.iter_mut())
    }
}

impl<V> std::ops::Index<&u64> for PreMap<V> {
    type Output = V;
    fn index(&self, k: &u64) -> &V {
        &self.vals[self.keys.binary_search(k).unwrap_or_else(|_| panic!("no entry for source {k}"))]
    }
}

#[cfg(test)]
mod premap_tests {
    use super::PreMap;

    #[test]
    fn behaves_like_a_sorted_map_and_adds_without_growing() {
        let mut m = PreMap::<u32>::new();
        let (kc, vc) = (m.keys.capacity(), m.vals.capacity());
        for k in [5u64, 1, 9, 3] {
            assert!(m.insert(k, k as u32).is_none());
        }
        assert_eq!(m.insert(3, 33), Some(3));
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), vec![1, 3, 5, 9]);
        assert_eq!(m[&3], 33);
        assert_eq!(m.remove(&5), Some(5));
        assert!(!m.contains_key(&5) && m.len() == 3);
        for (_, v) in &mut m {
            *v += 1;
        }
        assert_eq!(m.get(&9), Some(&10));
        assert_eq!((m.keys.capacity(), m.vals.capacity()), (kc, vc), "no growth");
    }
}
