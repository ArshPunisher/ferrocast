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
    /// Frames still to come after a reset: the first is let go (the model's old history makes it
    /// unlike the next), the second faded in, so the sound never steps up from silence.
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
            let (yl, yr) = (self.ch[0].out[p], self.ch[1].out[p]);
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
                if self.fresh > 0 {
                    for c in &mut self.ch {
                        for (k, v) in c.out.iter_mut().enumerate() {
                            *v *= if self.fresh == 2 { 0.0 } else { k as f32 / FRAME_SIZE as f32 };
                        }
                    }
                    self.fresh -= 1;
                }
                self.amount = a1;
            }
        }
        true
    }

    fn latency_frames(&self) -> usize {
        LATENCY_FRAMES
    }

    /// Back from a bypass: forget the stale frame (it would play out of order); the filter's
    /// own 8 ms crossfade hides the 10 ms of silence while the first frame fills.
    fn reset(&mut self) {
        for c in &mut self.ch {
            c.collect = [0.0; FRAME_SIZE];
            c.out = [0.0; FRAME_SIZE];
        }
        self.pos = 0;
        self.fresh = 2;
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
