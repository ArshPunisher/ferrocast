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

    /// Changes the delay with what is held kept: longer, the held pictures simply wait more;
    /// shorter, those past the new time come out at the next call (the newest shows).
    pub fn set_delay_ms(&mut self, delay_ms: u32) {
        let ms = delay_ms.min(MAX_DELAY_MS);
        if ms != self.delay_ms() {
            self.delay_ns = u64::from(ms) * 1_000_000;
            self.capped = false;
        }
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

/// How long sound fades out and in around a change of the delay.
const FADE_MS: u64 = 5;

/// Sound held back by a whole number of frames: what goes in comes out that many frames later
/// (silence at first), so its timestamps stay the same and the stream stays continuous.
///
/// Changing the delay while it runs doesn't cut the held sound off: the old line's sound fades
/// out over 5 ms, the line is made again at the new length, and what goes in fades in over 5 ms.
#[derive(Debug)]
pub struct AudioDelay {
    ring: Vec<f32>,
    pos: usize,
    rate: u32,
    channels: usize,
    delay_ms: u32,
    /// Frames of fade-out left before the line is rebuilt at `delay_ms` (0: no change waiting).
    fade_out: usize,
    /// Frames of fade-in left on what goes into the new line.
    fade_in: usize,
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
            fade_out: 0,
            fade_in: 0,
            // Room for the usual piece, so the first ones don't grow it.
            scratch: Vec::with_capacity(16_384),
            dirty: false,
        }
    }

    /// The delay asked for (a change still fading in counts).
    pub fn delay_ms(&self) -> u32 {
        self.delay_ms
    }

    /// Changes the delay: with sound already running, the old sound fades out and the new line's
    /// fades in (a few ms each) instead of the held sound being cut.
    pub fn set_delay(&mut self, delay_ms: u32) {
        let ms = delay_ms.min(MAX_DELAY_MS);
        if ms == self.delay_ms {
            return;
        }
        self.delay_ms = ms;
        if self.rate == 0 {
            return;
        }
        if self.fade_out == 0 {
            self.fade_out = self.fade_frames();
        }
    }

    fn fade_frames(&self) -> usize {
        ((u64::from(self.rate) * FADE_MS).div_ceil(1000) as usize).max(1)
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
        self.fade_out = 0;
        self.fade_in = 0;
    }

    /// The line made again at the asked length, empty.
    fn rebuild(&mut self) {
        let frames = (u64::from(self.delay_ms) * u64::from(self.rate)).div_ceil(1000) as usize;
        // A whole number of frames, whatever the channel count.
        let len = (frames * self.channels).min(MAX_AUDIO_SAMPLES / self.channels * self.channels);
        self.ring.clear();
        self.ring.resize(len, 0.0);
        self.pos = 0;
    }

    /// Delays `pcm` (interleaved, `channels` wide, at `rate`) and returns the delayed piece, the
    /// same length. The line is sized on the first piece (or when the format changes); after
    /// that nothing is allocated until the delay is changed.
    pub fn process(&mut self, pcm: &[f32], channels: usize, rate: u32) -> &[f32] {
        self.scratch.clear();
        if channels == 0 || rate == 0 {
            return &self.scratch;
        }
        if self.rate != rate || self.channels != channels {
            self.rate = rate;
            self.channels = channels;
            self.fade_out = 0;
            self.fade_in = 0;
            self.rebuild();
        }
        self.dirty = true;
        let fade = self.fade_frames();
        for frame in pcm.chunks_exact(channels) {
            // The old line's sound goes down; at the bottom the line is made again.
            let mut out_gain = 1.0;
            if self.fade_out > 0 {
                out_gain = (self.fade_out - 1) as f32 / fade as f32;
                self.fade_out -= 1;
            }
            let mut in_gain = 1.0;
            if self.fade_out == 0 && self.fade_in > 0 {
                in_gain = 1.0 - self.fade_in as f32 / fade as f32;
                self.fade_in -= 1;
            }
            let was_fading = out_gain < 1.0;
            for &s in frame {
                let put = s * in_gain;
                let out = if self.ring.is_empty() {
                    put
                } else {
                    let o = std::mem::replace(&mut self.ring[self.pos], put);
                    self.pos += 1;
                    if self.pos == self.ring.len() {
                        self.pos = 0;
                    }
                    o
                };
                self.scratch.push(out * out_gain);
            }
            if was_fading && self.fade_out == 0 {
                // Silent now: the new line starts empty, and its input fades in.
                self.rebuild();
                self.fade_in = fade;
            }
        }
        &self.scratch
    }
}

/// Converts opaque RGBA to NV12 (BT.709, limited range: what the card reads back with
/// `Nv12Picture { bt709: true, full_range: false }`), chroma from each 2x2 block. None when the
/// picture has any transparency or an odd size: those stay RGBA (NV12 has no alpha).
/// Holding a picture this way takes 1.5 bytes a pixel instead of 4.
pub fn rgba_to_nv12(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 || rgba.len() != w * h * 4 {
        return None;
    }
    if rgba.as_chunks::<4>().0.iter().any(|p| p[3] != 255) {
        return None;
    }
    let mut out = vec![0u8; w * h * 3 / 2];
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    for row in 0..h {
        let src = &rgba[row * w * 4..(row + 1) * w * 4];
        let dst = &mut y_plane[row * w..(row + 1) * w];
        for (p, y) in src.as_chunks::<4>().0.iter().zip(dst.iter_mut()) {
            let v = 47 * i32::from(p[0]) + 157 * i32::from(p[1]) + 16 * i32::from(p[2]) + 128;
            *y = ((v >> 8) + 16).clamp(16, 235) as u8;
        }
    }
    for row in 0..h / 2 {
        let top = &rgba[(row * 2) * w * 4..(row * 2 + 1) * w * 4];
        let bottom = &rgba[(row * 2 + 1) * w * 4..(row * 2 + 2) * w * 4];
        let dst = &mut uv_plane[row * w..(row + 1) * w];
        for (col, uv) in dst.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let at = col * 8;
            let mut sum = [0i32; 3];
            for px in [&top[at..at + 4], &top[at + 4..at + 8], &bottom[at..at + 4], &bottom[at + 4..at + 8]] {
                for k in 0..3 {
                    sum[k] += i32::from(px[k]);
                }
            }
            let (r, g, b) = ((sum[0] + 2) / 4, (sum[1] + 2) / 4, (sum[2] + 2) / 4);
            uv[0] = (((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
            uv[1] = (((112 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
        }
    }
    Some(out)
}

/// The longest delay, in ms, that `max_bytes` holds of pictures of `frame_bytes` at `fps`.
pub fn longest_ms(frame_bytes: usize, fps: u32, max_bytes: usize) -> u32 {
    if frame_bytes == 0 || fps == 0 {
        return MAX_DELAY_MS;
    }
    let frames = (max_bytes / frame_bytes) as u64;
    ((frames * 1000 / u64::from(fps)).min(u64::from(MAX_DELAY_MS))) as u32
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
    fn a_changed_delay_keeps_what_is_held() {
        let mut d = FrameDelay::<u64>::new(100, 60);
        for tick in 0..3u64 {
            d.push(tick * PERIOD, tick, 1000);
        }
        d.set_delay_ms(500);
        assert_eq!(d.len(), 3, "nothing thrown away");
        assert_eq!(d.take_due(3 * PERIOD, 0), None, "they wait the longer time");
        assert_eq!(d.take_due(500_000_000 + 2 * PERIOD, 0), Some(2), "then the newest shows");
    }

    #[test]
    fn the_sound_line_is_a_whole_number_of_frames_for_any_channel_count() {
        let mut a = AudioDelay::new(MAX_DELAY_MS);
        let pcm = vec![0.0f32; 3 * 64];
        a.process(&pcm, 3, 192_000);
        assert_eq!(a.ring.len() % 3, 0);
        assert!(a.ring.len() <= MAX_AUDIO_SAMPLES);
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

    fn sine(from: usize, n: usize) -> Vec<f32> {
        (from..from + n)
            .flat_map(|i| {
                let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin();
                [v, v]
            })
            .collect()
    }

    /// The biggest step between neighbouring samples of one channel.
    fn biggest_step(out: &[f32]) -> f32 {
        out.as_chunks::<2>()
            .0
            .iter()
            .map(|f| f[0])
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn changing_the_delay_while_it_runs_does_not_click() {
        // A steady tone: its own biggest step is 0.029. A cut would be a jump of up to 0.5.
        for (from, to) in [(100u32, 300u32), (300, 100), (100, 0), (0, 200)] {
            let mut a = AudioDelay::new(from);
            let mut out = Vec::new();
            let mut at = 0;
            for round in 0..120 {
                if round == 20 {
                    a.set_delay(to);
                }
                out.extend_from_slice(a.process(&sine(at, 480), 2, 48_000));
                at += 480;
            }
            assert_eq!(a.delay_ms(), to);
            let step = biggest_step(&out);
            assert!(step < 0.06, "{from} -> {to} ms: a step of {step}");
            // It settles at the new delay: the tone comes out `to` ms late, once it has faded in.
            let late = (u64::from(to) * 48) as usize;
            let tail = out.len() / 2 - 480;
            let i = tail;
            let want = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * (i - late) as f32 / 48_000.0).sin();
            assert!((out[i * 2] - want).abs() < 1e-3, "{from} -> {to}: {} vs {want}", out[i * 2]);
        }
    }

    #[test]
    fn the_old_sound_fades_out_in_five_ms() {
        let mut a = AudioDelay::new(50);
        for round in 0..10 {
            a.process(&sine(round * 480, 480), 2, 48_000);
        }
        a.set_delay(100);
        let out = a.process(&vec![0.5f32; 2 * 480], 2, 48_000).to_vec();
        let first = out[0].abs();
        let at_five = out[2 * 239].abs();
        assert!(first > at_five, "going down: {first} then {at_five}");
        assert!(out[2 * 240..].iter().all(|s| s.abs() < 1e-6), "silent once faded, until the new delay is up");
    }

    #[test]
    fn a_picture_is_held_as_video_in_a_third_less_than_half_the_memory() {
        let (w, h) = (64u32, 36u32);
        let rgba: Vec<u8> = (0..w * h).flat_map(|i| [(i % 251) as u8, (i % 113) as u8 * 2, 200, 255]).collect();
        let nv = rgba_to_nv12(w, h, &rgba).expect("opaque and even");
        assert_eq!(nv.len(), (w * h * 3 / 2) as usize);
        assert!(rgba.len() as f64 / nv.len() as f64 > 2.6, "2.67x as many pictures in the same memory");
    }

    #[test]
    fn nv12_holds_the_colours_closely_and_leaves_transparent_pictures_alone() {
        use crate::ffsrc::Nv12Picture;
        let (w, h) = (32u32, 16u32);
        for rgb in [[255u8, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255], [0, 0, 0], [90, 60, 30], [200, 120, 40]] {
            let rgba: Vec<u8> = (0..w * h).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect();
            let data = rgba_to_nv12(w, h, &rgba).expect("opaque");
            let back = Nv12Picture { width: w, height: h, data, bt709: true, full_range: false }
                .to_picture()
                .expect("converts back");
            for k in 0..3 {
                let d = (i32::from(back.rgba[k]) - i32::from(rgb[k])).abs();
                assert!(d <= 3, "{rgb:?} came back {:?}", &back.rgba[..4]);
            }
        }
        // Any transparency keeps it RGBA, and so does an odd size.
        let mut see_through = vec![255u8; (w * h * 4) as usize];
        see_through[3] = 254;
        assert!(rgba_to_nv12(w, h, &see_through).is_none());
        assert!(rgba_to_nv12(31, 16, &vec![255u8; 31 * 16 * 4]).is_none());
    }

    #[test]
    fn the_longest_delay_that_fits_is_worked_out_from_the_picture() {
        let nv12_1080 = 1920 * 1080 * 3 / 2;
        assert_eq!(longest_ms(nv12_1080, 30, MAX_VIDEO_BYTES), 5733);
        assert_eq!(longest_ms(nv12_1080, 60, MAX_VIDEO_BYTES), 2866);
        assert!(longest_ms(1920 * 1080 * 4, 60, MAX_VIDEO_BYTES) < 1100, "RGBA fits 2.7x less");
        assert_eq!(longest_ms(100, 30, MAX_VIDEO_BYTES), MAX_DELAY_MS);
    }
}
