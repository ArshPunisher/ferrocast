//! "Remove background noise": RNNoise (the `nnnoiseless` port) run inline on the audio thread,
//! as OBS's noise-suppress-filter.c does. One model state per channel, 480-sample frames at
//! 48 kHz, exactly one frame (10 ms) of delay, no queue and no allocation while it runs: the
//! states are built off the audio thread, when the filter is added.

use crate::afx::{AudioEffect, SoundPlugin};
use nnnoiseless::{DenoiseState, FRAME_SIZE};

/// RNNoise takes 16-bit-range floats.
const I16_SCALE: f32 = 32768.0;
/// Where the soft limit on the model's input starts and tops out (the same knee as the strong
/// noise removal's: a loud source before it must not overload the model).
const KNEE: f32 = 0.8;
const CEILING: f32 = 0.99;

fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE {
        return x;
    }
    (KNEE + (CEILING - KNEE) * ((a - KNEE) / (CEILING - KNEE)).tanh()).copysign(x)
}

/// One channel: the frame being collected, the last frame's result (played out a frame late),
/// and the model.
struct Chan {
    state: Box<DenoiseState<'static>>,
    collect: [f32; FRAME_SIZE],
    out: [f32; FRAME_SIZE],
}

impl Chan {
    fn new() -> Self {
        Self { state: DenoiseState::new(), collect: [0.0; FRAME_SIZE], out: [0.0; FRAME_SIZE] }
    }

    /// The collected frame is full: clean it, mixed with itself by `a0` to `a1` (wet, 0 to 1).
    fn run_frame(&mut self, a0: f32, a1: f32) {
        let mut inp = [0.0f32; FRAME_SIZE];
        let mut wet = [0.0f32; FRAME_SIZE];
        for (d, s) in inp.iter_mut().zip(&self.collect) {
            *d = if s.is_finite() { soft_limit(*s) * I16_SCALE } else { 0.0 };
        }
        self.state.process_frame(&mut wet, &inp);
        for (k, ((o, w), d)) in self.out.iter_mut().zip(&wet).zip(&self.collect).enumerate() {
            let a = a0 + (a1 - a0) * ((k + 1) as f32 / FRAME_SIZE as f32);
            let w = w / I16_SCALE;
            let w = if w.is_finite() { w } else { 0.0 };
            let dry = if d.is_finite() { *d } else { 0.0 };
            *o = dry + (w - dry) * a;
        }
    }
}

/// The noise removal: a [`SoundPlugin`] whose `process` runs on the audio thread.
pub struct Rnnoise {
    ch: [Chan; 2],
    pos: usize,
    /// Wet share now and wanted (0 to 1); moves over one frame so a slider doesn't click.
    amount: f32,
    target: f32,
    /// Frames still to play after a reset (3, 2, 1): the sound as it comes in (as it was while
    /// bypassed, with nothing cleaned yet), then from it to the same sound a frame late, then from
    /// that to the cleaned one (the model's first frame back is let go: its old history makes it
    /// unlike the next). Never a gap and never a step.
    fresh: u8,
}

impl std::fmt::Debug for Rnnoise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rnnoise").field("pos", &self.pos).field("amount", &self.amount).finish_non_exhaustive()
    }
}

/// Frames of delay (10 ms at 48 kHz).
pub const LATENCY_FRAMES: usize = FRAME_SIZE;

fn wet_share(amount: u32) -> f32 {
    amount.clamp(0, 100) as f32 / 100.0
}

impl Rnnoise {
    /// Builds the model states (allocates: not for the audio thread).
    #[must_use]
    pub fn new(amount: u32) -> Self {
        let a = wet_share(amount);
        Self { ch: [Chan::new(), Chan::new()], pos: 0, amount: a, target: a, fresh: 0 }
    }
}

impl SoundPlugin for Rnnoise {
    fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
        let [l, r] = ch;
        let n = l.len().min(r.len());
        for i in 0..n {
            let p = self.pos;
            // What goes out is the previous frame's result at this place: a fixed 480 delay.
            let (mut yl, mut yr) = (self.ch[0].out[p], self.ch[1].out[p]);
            match self.fresh {
                // Back from a bypass with nothing collected: the sound goes on as it comes.
                3 => (yl, yr) = (l[i], r[i]),
                // Then over one frame into the same sound a frame late.
                2 => {
                    let a = (p + 1) as f32 / FRAME_SIZE as f32;
                    yl = l[i] + (yl - l[i]) * a;
                    yr = r[i] + (yr - r[i]) * a;
                }
                _ => {}
            }
            self.ch[0].collect[p] = l[i];
            self.ch[1].collect[p] = r[i];
            l[i] = yl;
            r[i] = yr;
            self.pos += 1;
            if self.pos == FRAME_SIZE {
                self.pos = 0;
                let (a0, a1) = (self.amount, self.target);
                self.ch[0].run_frame(a0, a1);
                self.ch[1].run_frame(a0, a1);
                // The first frame back plays dry (a frame late); the next goes from dry to
                // cleaned over the frame.
                if self.fresh > 1 {
                    for c in &mut self.ch {
                        for (k, (v, d)) in c.out.iter_mut().zip(&c.collect).enumerate() {
                            let dry = if d.is_finite() { *d } else { 0.0 };
                            let a = if self.fresh == 3 { 0.0 } else { (k + 1) as f32 / FRAME_SIZE as f32 };
                            *v = dry + (*v - dry) * a;
                        }
                    }
                }
                self.fresh = self.fresh.saturating_sub(1);
                self.amount = a1;
            }
        }
        true
    }

    fn latency_frames(&self) -> usize {
        LATENCY_FRAMES
    }

    /// Back from a bypass: forget the stale frame (it would play out of order) and come back in
    /// three frames (see `fresh`), so the sound has no gap while the first frame fills.
    fn reset(&mut self) {
        for c in &mut self.ch {
            c.collect = [0.0; FRAME_SIZE];
            c.out = [0.0; FRAME_SIZE];
        }
        self.pos = 0;
        self.fresh = 3;
    }

    fn retune(&mut self, effect: &AudioEffect) -> bool {
        if let AudioEffect::Rnnoise { amount } = effect {
            self.target = wet_share(*amount);
            true
        } else {
            false
        }
    }

    /// What it took in and hasn't given back: the cleaned rest of the last frame, then the
    /// sound collected since (not cleaned yet).
    fn hand_over(&mut self) -> Option<[Vec<f32>; 2]> {
        let p = self.pos;
        let held = |c: &Chan| {
            let mut v = Vec::with_capacity(FRAME_SIZE);
            v.extend_from_slice(&c.out[p..]);
            v.extend_from_slice(&c.collect[..p]);
            v
        };
        Some([held(&self.ch[0]), held(&self.ch[1])])
    }
}

#[cfg(test)]
mod tests {
    use crate::afx::{AudioEffect, AudioFilter};

    /// The longest run of samples quieter than `floor`.
    fn longest_quiet(x: &[f32], floor: f32) -> usize {
        let (mut run, mut most) = (0usize, 0usize);
        for v in x {
            run = if v.abs() < floor { run + 1 } else { 0 };
            most = most.max(run);
        }
        most
    }

    /// Back from a full bypass on a steady tone: no dropout while the model fills again (it
    /// used to give about 20 ms of silence: the emptied frame, then the first one ×0).
    #[test]
    fn re_enabling_after_a_full_bypass_has_no_dropout() {
        for amount in [100u32, 0] {
            let effect = AudioEffect::Rnnoise { amount };
            let mut f = AudioFilter::new(effect, 48_000);
            assert!(f.latency_frames() > 0, "the model is built at 48 kHz");
            // 440 Hz at -10 dBFS, in blocks that aren't a frame long.
            let tone = |i: usize| (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * 0.3;
            let mut pos = 0usize;
            let mut block = |f: &mut AudioFilter| {
                let mut l: Vec<f32> = (0..333).map(|k| tone(pos + k)).collect();
                let mut r = l.clone();
                pos += 333;
                f.process([&mut l, &mut r]);
                l
            };
            let mut before = Vec::new();
            for _ in 0..150 {
                before.extend(block(&mut f));
            }
            // What it gives on the tone once settled: "near silent" is 30 dB under that.
            let settled = before[before.len() - 4800..].iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(settled > 0.01, "amount {amount}: the tone went (peak {settled})");
            f.set_enabled(false);
            for _ in 0..30 {
                block(&mut f);
            }
            f.set_enabled(true);
            let mut after = Vec::new();
            for _ in 0..60 {
                after.extend(block(&mut f));
            }
            let quiet = longest_quiet(&after, settled * 0.03);
            assert!(quiet <= 96, "amount {amount}: {quiet} near-silent samples in a row after re-enabling");
        }
    }
}
