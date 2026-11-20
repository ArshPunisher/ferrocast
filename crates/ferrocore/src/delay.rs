//! The Video Delay filter's buffers (OBS's `gpu_delay` and `async_delay_filter`): a source's
//! pictures held back by a set time, and (for the async one) its sound too.
//!
//! Memory is bounded on purpose: pictures are held on the CPU, so a long delay on a big picture
//! would eat the machine. [`FrameDelay`] keeps at most [`MAX_VIDEO_BYTES`] and lets the oldest
//! picture out early beyond that (the delay shortens; it never grows memory). The sound delay
//! line is sized once when the filter is set and never allocates after.

use std::collections::VecDeque;

/// OBS's Video Delay (Async) goes to 20 seconds.
pub const MAX_DELAY_MS: u32 = 20_000;

/// The most picture data one filter holds: 512 MiB.
pub const MAX_VIDEO_BYTES: usize = 512 * 1024 * 1024;

/// The most entries kept room for up front (a longer queue still works; it just grows).
const RESERVE_FRAMES: usize = 1024;

/// Pictures held back by `delay`, released in order once they have waited it out.
pub struct FrameDelay<T> {
    delay_ns: u64,
    held: VecDeque<(u64, usize, T)>,
    bytes: usize,
    max_bytes: usize,
    /// A picture let out early because the memory cap was reached (the newest such).
    early: Option<T>,
    capped: bool,
}

impl<T> std::fmt::Debug for FrameDelay<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameDelay").field("delay_ms", &self.delay_ms()).field("held", &self.held.len()).finish()
    }
}

impl<T> FrameDelay<T> {
    /// `fps` sizes the queue up front so holding a picture doesn't allocate.
    pub fn new(delay_ms: u32, fps: u32) -> Self {
        Self::with_cap(delay_ms, fps, MAX_VIDEO_BYTES)
    }

    pub fn with_cap(delay_ms: u32, fps: u32, max_bytes: usize) -> Self {
        let ms = delay_ms.min(MAX_DELAY_MS);
        let frames = (u64::from(ms) * u64::from(fps.max(1))).div_ceil(1000) as usize + 2;
        Self {
            delay_ns: u64::from(ms) * 1_000_000,
            held: VecDeque::with_capacity(frames.min(RESERVE_FRAMES)),
            bytes: 0,
            max_bytes,
            early: None,
            capped: false,
        }
    }

    pub fn delay_ms(&self) -> u32 {
        (self.delay_ns / 1_000_000) as u32
    }

    /// Whether the memory cap has cut the delay short at any time.
    pub fn capped(&self) -> bool {
        self.capped
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty() && self.early.is_none()
    }

    /// Takes a picture of `bytes` size, seen at `now` (ns).
    /// True the first time the memory cap cuts the delay short.
    pub fn push(&mut self, now: u64, frame: T, bytes: usize) -> bool {
        let was = self.capped;
        self.bytes += bytes;
        self.held.push_back((now, bytes, frame));
        // Past the cap the oldest goes out early: the delay shortens, memory doesn't grow.
        while self.bytes > self.max_bytes && self.held.len() > 1 {
            if let Some((_, b, f)) = self.held.pop_front() {
                self.bytes -= b;
                self.early = Some(f);
                self.capped = true;
            }
        }
        self.capped && !was
    }

    /// The newest picture that has waited out the delay by `now` (older ones are dropped: only
    /// the latest is shown). `slack` (ns) lets one out a little early, half a frame, so a tick
    /// that comes a hair before its time isn't a whole frame late.
    pub fn take_due(&mut self, now: u64, slack: u64) -> Option<T> {
        let mut out = self.early.take();
        while let Some((at, ..)) = self.held.front() {
            if now.saturating_add(slack) < at.saturating_add(self.delay_ns) {
                break;
            }
            if let Some((_, b, f)) = self.held.pop_front() {
                self.bytes -= b;
                out = Some(f);
            }
        }
        out
    }
}

/// The most sound one delay line holds, in samples across all channels: 20 s of stereo at 192 kHz.
const MAX_AUDIO_SAMPLES: usize = 20 * 192_000 * 2;

/// Sound held back by a whole number of frames: what goes in comes out that many frames later
/// (silence at first), so its timestamps stay the same and the stream stays continuous.
#[derive(Debug)]
pub struct AudioDelay {
    ring: Vec<f32>,
    pos: usize,
    rate: u32,
    channels: usize,
    delay_ms: u32,
    scratch: Vec<f32>,
    dirty: bool,
}

impl AudioDelay {
    pub fn new(delay_ms: u32) -> Self {
        Self {
            ring: Vec::new(),
            pos: 0,
            rate: 0,
            channels: 0,
            delay_ms: delay_ms.min(MAX_DELAY_MS),
            // Room for the usual piece, so the first ones don't grow it.
            scratch: Vec::with_capacity(16_384),
            dirty: false,
        }
    }

    pub fn delay_ms(&self) -> u32 {
        self.delay_ms
    }

    /// Sound the delay line holds right now (false once it's been cleared or never fed).
    pub fn holding(&self) -> bool {
        self.dirty
    }

    /// Forgets what's held (the source went off air: its old sound mustn't come back later).
    pub fn clear(&mut self) {
        if self.dirty {
            self.ring.fill(0.0);
            self.pos = 0;
            self.dirty = false;
        }
    }

    /// Delays `pcm` (interleaved, `channels` wide, at `rate`) and returns the delayed piece, the
    /// same length. The line is sized on the first piece (or when the format changes); after
    /// that nothing is allocated.
    pub fn process(&mut self, pcm: &[f32], channels: usize, rate: u32) -> &[f32] {
        if channels == 0 || rate == 0 {
            self.scratch.clear();
            return &self.scratch;
        }
        if self.rate != rate || self.channels != channels {
            let frames = (u64::from(self.delay_ms) * u64::from(rate)).div_ceil(1000) as usize;
            let len = (frames * channels).min(MAX_AUDIO_SAMPLES);
            self.ring.clear();
            self.ring.resize(len, 0.0);
            self.pos = 0;
            self.rate = rate;
            self.channels = channels;
        }
        self.scratch.clear();
        if self.ring.is_empty() {
            self.scratch.extend_from_slice(pcm);
            return &self.scratch;
        }
        self.dirty = true;
        let len = self.ring.len();
        for &s in pcm {
            self.scratch.push(std::mem::replace(&mut self.ring[self.pos], s));
            self.pos += 1;
            if self.pos == len {
                self.pos = 0;
            }
        }
        &self.scratch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: u64 = 1_000_000_000 / 60;

    /// Runs frames through at 60 fps; returns how many ticks each came out after it went in.
    fn lag_in_frames(delay_ms: u32) -> Vec<u64> {
        let mut d = FrameDelay::<u64>::new(delay_ms, 60);
        let mut lags = Vec::new();
        for tick in 0..120u64 {
            let now = tick * PERIOD;
            d.push(now, tick, 1000);
            if let Some(shown) = d.take_due(now, PERIOD / 2) {
                lags.push(tick - shown);
            }
        }
        lags
    }

    #[test]
    fn the_delay_is_exact_to_a_frame() {
        // 200 ms at 60 fps is 12 frames; 1000 ms is 60.
        for (ms, frames) in [(200u32, 12u64), (100, 6), (50, 3), (1000, 60), (17, 1)] {
            let lags = lag_in_frames(ms);
            assert!(!lags.is_empty(), "{ms} ms shows something");
            assert!(lags.iter().all(|l| *l == frames), "{ms} ms is {frames} frames late, got {lags:?}");
        }
    }

    #[test]
    fn jittery_ticks_stay_within_a_frame() {
        let mut d = FrameDelay::<u64>::new(200, 60);
        let mut worst = 0i64;
        for tick in 0..200u64 {
            // Each tick is up to a quarter frame early or late.
            let jitter = [0i64, 3, -4, 2, -3][tick as usize % 5] * (PERIOD as i64 / 16);
            let now = (tick * PERIOD) as i64 + jitter;
            d.push(now as u64, tick, 1000);
            if let Some(shown) = d.take_due(now as u64, PERIOD / 2) {
                worst = worst.max((tick as i64 - shown as i64 - 12).abs());
            }
        }
        assert!(worst <= 1, "within a frame of 12, was {worst}");
    }

    #[test]
    fn memory_is_capped_and_the_delay_gives_way() {
        // 100 frames of 1000 bytes under a 10 000 byte cap: never more than 10 held.
        let mut d = FrameDelay::<u64>::with_cap(20_000, 60, 10_000);
        let mut shown = 0;
        for tick in 0..100u64 {
            d.push(tick * PERIOD, tick, 1000);
            assert!(d.len() <= 10, "held {}", d.len());
            if d.take_due(tick * PERIOD, PERIOD / 2).is_some() {
                shown += 1;
            }
        }
        assert!(d.capped(), "the cap was hit and noted");
        assert!(shown > 0, "a capped delay still shows pictures");
    }

    #[test]
    fn nothing_is_shown_before_the_delay_is_up() {
        let mut d = FrameDelay::<u64>::new(500, 30);
        d.push(0, 1, 10);
        assert!(d.take_due(100_000_000, 0).is_none());
        assert_eq!(d.take_due(500_000_000, 0), Some(1));
        assert!(d.is_empty());
    }

    #[test]
    fn the_delay_is_limited_to_twenty_seconds() {
        assert_eq!(FrameDelay::<u8>::new(99_999, 30).delay_ms(), MAX_DELAY_MS);
        assert_eq!(AudioDelay::new(99_999).delay_ms(), MAX_DELAY_MS);
    }

    #[test]
    fn sound_comes_out_exactly_the_delay_later() {
        // 10 ms at 48 kHz stereo is 480 frames.
        let mut a = AudioDelay::new(10);
        let input: Vec<f32> = (1..=2000).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let mut out = Vec::new();
        for piece in input.chunks(300) {
            out.extend_from_slice(a.process(piece, 2, 48_000));
        }
        assert_eq!(out.len(), input.len(), "same length");
        assert!(out[..480 * 2].iter().all(|s| *s == 0.0), "silence for the delay");
        assert_eq!(&out[480 * 2..], &input[..input.len() - 480 * 2], "then the sound, intact");
    }

    #[test]
    fn a_zero_delay_passes_sound_through() {
        let mut a = AudioDelay::new(0);
        let pcm = [0.1f32, 0.2, 0.3, 0.4];
        assert_eq!(a.process(&pcm, 2, 48_000), &pcm);
    }

    #[test]
    fn a_cleared_line_gives_no_old_sound() {
        let mut a = AudioDelay::new(10);
        a.process(&vec![0.5f32; 960], 2, 48_000);
        assert!(a.holding());
        a.clear();
        let out = a.process(&vec![0.0f32; 960], 2, 48_000).to_vec();
        assert!(out.iter().all(|s| *s == 0.0), "what was held is gone");
    }

    #[test]
    fn the_sound_line_does_not_allocate_after_it_is_sized() {
        let mut a = AudioDelay::new(200);
        let pcm = vec![0.25f32; 1024];
        a.process(&pcm, 2, 48_000);
        let (ring, scratch) = (a.ring.as_ptr(), a.scratch.as_ptr());
        for _ in 0..500 {
            a.process(&pcm, 2, 48_000);
        }
        assert_eq!((a.ring.as_ptr(), a.scratch.as_ptr()), (ring, scratch), "the same buffers throughout");
    }
}
