//! Takes a device's steady offset out of its sound. A capture card with no signal, or a
//! microphone input with a bias, can send the same small number for every sample: that is not
//! sound, but it reads as one on a meter (-50 dB, flat forever), goes into the stream and the
//! recording, and holds a noise gate open. A one-pole high-pass, `y[n] = x[n] - x[n-1] +
//! R * y[n-1]`, lets everything a person hears through (its corner is a few Hz, under the
//! lowest note a microphone is used for) and takes the offset away within a fraction of a
//! second.
//!
//! No allocation, no lock: it runs on the sound's way into the mixer, a few multiplies per
//! sample.

/// Where the high-pass starts to cut, in Hz. 7 Hz takes a 30 Hz tone down by about 0.2 dB and
/// settles an offset to silence in about a fifth of a second.
const CORNER_HZ: f32 = 7.0;

/// A remainder this small (-200 dB) is made zero, so a settled offset reaches true silence
/// and the filter never runs on denormal numbers (slow on some processors).
const FLUSH: f32 = 1e-10;

/// Most channels it keeps state for; a sound with more passes the extra channels through.
const MAX_CHANNELS: usize = 8;

/// The high-pass for one source's sound: its memory is per channel and starts over when the
/// sample rate or the number of channels changes (a new device, or the same one opened again).
#[derive(Debug, Clone, Copy)]
pub struct DcBlock {
    rate: u32,
    channels: usize,
    /// `R`: `exp(-2 pi corner / rate)`.
    r: f32,
    /// The last input and output of each channel.
    x1: [f32; MAX_CHANNELS],
    y1: [f32; MAX_CHANNELS],
    /// False until the first sample after a start: that sample sets the input memory, so
    /// sound that begins on an offset starts silent, with no step.
    primed: bool,
}

impl Default for DcBlock {
    fn default() -> Self {
        Self::new()
    }
}

impl DcBlock {
    pub const fn new() -> Self {
        DcBlock { rate: 0, channels: 0, r: 0.0, x1: [0.0; MAX_CHANNELS], y1: [0.0; MAX_CHANNELS], primed: false }
    }

    /// Forgets everything: the next sound starts the filter again.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Takes the offset out of `pcm` (interleaved, `channels` channels at `rate` Hz) in place.
    /// A sample that isn't a number counts as silence (the mixer would make it so anyway, and
    /// one NaN must not stay in the filter's memory for good).
    pub fn process(&mut self, pcm: &mut [f32], channels: usize, rate: u32) {
        if channels == 0 || rate == 0 {
            return;
        }
        if rate != self.rate || channels != self.channels {
            self.reset();
            self.rate = rate;
            self.channels = channels;
            self.r = (-std::f32::consts::TAU * CORNER_HZ / rate as f32).exp();
        }
        let r = self.r;
        for frame in pcm.chunks_exact_mut(channels) {
            for (c, v) in frame.iter_mut().take(MAX_CHANNELS).enumerate() {
                let x = if v.is_finite() { *v } else { 0.0 };
                if !self.primed {
                    self.x1[c] = x;
                }
                let mut y = x - self.x1[c] + r * self.y1[c];
                if y.abs() < FLUSH {
                    y = 0.0;
                }
                self.x1[c] = x;
                self.y1[c] = y;
                *v = y;
            }
            self.primed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn tone(hz: f32, secs: f32, rate: u32, amp: f32) -> Vec<f32> {
        let n = (secs * rate as f32) as usize;
        (0..n)
            .flat_map(|i| {
                let v = amp * (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin();
                [v, v]
            })
            .collect()
    }

    fn rms(pcm: &[f32]) -> f32 {
        (pcm.iter().map(|v| v * v).sum::<f32>() / pcm.len() as f32).sqrt()
    }

    /// How much a tone is changed once the filter has settled, in dB.
    fn gain_db(hz: f32, rate: u32) -> f32 {
        let mut dc = DcBlock::new();
        let mut x = tone(hz, 2.0, rate, 0.5);
        let before = rms(&x[x.len() / 2..]);
        dc.process(&mut x, 2, rate);
        20.0 * (rms(&x[x.len() / 2..]) / before).log10()
    }

    #[test]
    fn a_steady_offset_settles_to_true_silence() {
        let mut dc = DcBlock::new();
        // A device that starts at nothing and then sits on 0.0032 (-50 dB) on both channels.
        let mut pcm = vec![0.0f32; 4800 * 2];
        pcm.extend(std::iter::repeat_n(0.0032f32, 48_000 * 2 * 2));
        let mut out = pcm.clone();
        for block in out.chunks_mut(480 * 2) {
            dc.process(block, 2, RATE);
        }
        let at = |s: f32| (s * RATE as f32) as usize * 2;
        // The jump itself is at 0.1 s: a fifth of a second later it is under -100 dB.
        let tail = |from: f32, to: f32| out[at(from)..at(to)].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(tail(0.1, 0.12) > 0.002, "the step itself is still there at first");
        assert!(tail(0.4, 0.5) < 1e-5, "under -100 dB after 0.3 s: {}", tail(0.4, 0.5));
        assert_eq!(tail(1.0, 2.0), 0.0, "exactly silent in the end");
    }

    #[test]
    fn an_offset_that_was_there_from_the_start_is_silent_at_once() {
        let mut dc = DcBlock::new();
        let mut pcm = vec![0.0032f32; 480 * 2];
        dc.process(&mut pcm, 2, RATE);
        assert!(pcm.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn the_blocks_it_is_given_in_make_no_difference() {
        let src: Vec<f32> = tone(100.0, 0.5, RATE, 0.4).iter().map(|v| v + 0.05).collect();
        let mut whole = src.clone();
        DcBlock::new().process(&mut whole, 2, RATE);
        let mut dc = DcBlock::new();
        let mut parts = src;
        for block in parts.chunks_mut(2 * 37) {
            dc.process(block, 2, RATE);
        }
        assert_eq!(whole, parts);
    }

    #[test]
    fn tones_pass_through_essentially_unchanged() {
        for rate in [44_100, 48_000] {
            for (hz, most) in [(1000.0, 0.01), (440.0, 0.02), (100.0, 0.1), (30.0, 0.5)] {
                let loss = gain_db(hz, rate).abs();
                assert!(loss < most, "{hz} Hz at {rate}: {loss} dB");
            }
        }
    }

    #[test]
    fn a_tone_on_top_of_an_offset_keeps_the_tone_and_loses_the_offset() {
        let mut dc = DcBlock::new();
        let mut x: Vec<f32> = tone(440.0, 2.0, RATE, 0.25).iter().map(|v| v + 0.01).collect();
        dc.process(&mut x, 2, RATE);
        let settled = &x[x.len() / 2..];
        let mean = settled.iter().sum::<f32>() / settled.len() as f32;
        assert!(mean.abs() < 1e-4, "mean {mean}");
        let want = 0.25 / 2f32.sqrt();
        assert!((rms(settled) - want).abs() < 0.005, "rms {}", rms(settled));
    }

    #[test]
    fn each_channel_has_its_own_memory() {
        let mut dc = DcBlock::new();
        // Left sits on an offset, right is a tone: the right one mustn't be touched by the left.
        let mut pcm: Vec<f32> = (0..96_000)
            .flat_map(|i| [0.2, 0.5 * (std::f32::consts::TAU * 1000.0 * i as f32 / RATE as f32).sin()])
            .collect();
        dc.process(&mut pcm, 2, RATE);
        let tail = &pcm[pcm.len() / 2..];
        assert!(tail.iter().step_by(2).all(|v| *v == 0.0 || v.abs() < 1e-6));
        let right: Vec<f32> = tail.iter().skip(1).step_by(2).copied().collect();
        assert!((rms(&right) - 0.5 / 2f32.sqrt()).abs() < 0.01);
    }

    #[test]
    fn the_memory_starts_over_when_the_rate_or_channels_change() {
        let mut dc = DcBlock::new();
        let mut a = vec![0.5f32; 480 * 2];
        dc.process(&mut a, 2, RATE);
        // Another device (44.1 kHz) sitting on 0.1: without a reset its first sample would be
        // 0.1 - 0.5 = -0.4, a click as loud as speech.
        let mut b = vec![0.1f32; 441 * 2];
        dc.process(&mut b, 2, 44_100);
        assert!(b.iter().all(|v| *v == 0.0), "first {}", b[0]);
        // And the same for the channels changing.
        let mut c = vec![0.3f32; 480];
        dc.process(&mut c, 1, 44_100);
        assert!(c.iter().all(|v| *v == 0.0), "first {}", c[0]);
        // An explicit reset does it too.
        let mut d = vec![0.7f32; 20];
        dc.process(&mut d, 1, 44_100);
        dc.reset();
        let mut e = vec![0.2f32; 20];
        dc.process(&mut e, 1, 44_100);
        assert!(e.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn a_sample_that_is_not_a_number_does_not_stay_in_the_memory() {
        let mut dc = DcBlock::new();
        let mut pcm = vec![0.1f32, 0.1, f32::NAN, f32::INFINITY, 0.1, 0.1];
        dc.process(&mut pcm, 2, RATE);
        assert!(pcm.iter().all(|v| v.is_finite()));
        let mut more = vec![0.1f32; 48_000 * 2];
        dc.process(&mut more, 2, RATE);
        assert!(more.iter().all(|v| v.is_finite()));
        assert_eq!(*more.last().unwrap_or(&1.0), 0.0);
    }

    #[test]
    fn the_filter_never_leaves_denormal_numbers() {
        let mut dc = DcBlock::new();
        let mut pcm = vec![0.0f32; 4800 * 2];
        pcm.extend(std::iter::repeat_n(0.5f32, 48_000 * 4 * 2));
        for block in pcm.chunks_mut(960) {
            dc.process(block, 2, RATE);
            assert!(block.iter().all(|v| *v == 0.0 || v.abs() >= FLUSH), "a tiny number is left");
        }
    }

    #[test]
    fn nothing_to_do_for_an_empty_sound_or_zero_channels() {
        let mut dc = DcBlock::new();
        let mut none: [f32; 0] = [];
        dc.process(&mut none, 2, RATE);
        let mut x = [0.5f32, 0.5];
        dc.process(&mut x, 0, RATE);
        dc.process(&mut x, 2, 0);
        assert_eq!(x, [0.5, 0.5]);
    }
}
