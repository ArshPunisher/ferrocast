//! Strong noise removal: DeepFilterNet 3 (MIT/Apache-2.0, github.com/Rikorose/DeepFilterNet) on
//! the CPU through tract, its model built in. It takes out what RNNoise leaves (keyboards, fans,
//! a busy room) and keeps the voice natural. It works on 10 ms blocks at 48 kHz and hands each
//! back one block later (10 ms, plus the model's own look ahead). Built only with the
//! `deepfilter` feature (the release builds), so day-to-day builds don't compile the model.

use std::collections::VecDeque;

use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray15::Array2;

/// How much quieter noise may get at a strength (1 to 100): 100 is as much as it can.
fn atten_db(strength: u32) -> f32 {
    // 0.6 dB a step, worked in whole numbers (50 is exactly 30 dB).
    if strength >= 100 { 100.0 } else { (strength.max(1) * 6) as f32 / 10.0 }
}

/// DeepFilterNet over a stream of planar samples: blocks in, cleaned blocks out, one block late.
pub(crate) struct Denoiser {
    df: DfTract,
    hop: usize,
    inp: Vec<VecDeque<f32>>,
    out: Vec<VecDeque<f32>>,
    noisy: Array2<f32>,
    clean: Array2<f32>,
    strength: u32,
}

impl Denoiser {
    /// For `channels` channels at 48 kHz.
    pub(crate) fn new(channels: usize, strength: u32) -> Result<Denoiser, String> {
        let channels = channels.clamp(1, 8);
        let rp = RuntimeParams::default_with_ch(channels).with_atten_lim(atten_db(strength));
        let df = DfTract::new(DfParams::default(), &rp).map_err(|e| format!("{e:#}"))?;
        let hop = df.hop_size;
        // One block of silence first: what's handed back is always there.
        let out = (0..channels).map(|_| std::iter::repeat_n(0.0, hop).collect()).collect();
        Ok(Denoiser {
            df,
            hop,
            inp: (0..channels).map(|_| VecDeque::with_capacity(hop * 4)).collect(),
            out,
            noisy: Array2::zeros((channels, hop)),
            clean: Array2::zeros((channels, hop)),
            strength,
        })
    }

    /// How late the cleaned sound comes out, in samples: the block of silence it starts with,
    /// the rest of the model's window, and the frames it looks ahead (40 ms with its model).
    pub(crate) fn latency(&self) -> usize {
        self.df.fft_size + self.df.lookahead * self.hop
    }

    pub(crate) fn channels(&self) -> usize {
        self.inp.len()
    }

    pub(crate) fn set_strength(&mut self, strength: u32) {
        if strength != self.strength {
            self.df.set_atten_lim(atten_db(strength));
            self.strength = strength;
        }
    }

    /// Cleans the samples in place (every plane as long as the first; at most `channels`).
    pub(crate) fn run(&mut self, planes: &mut [&mut [f32]]) {
        let n = planes.first().map_or(0, |p| p.len());
        let ch = self.channels().min(planes.len());
        for (c, p) in planes.iter().enumerate().take(ch) {
            self.inp[c].extend(p.iter().copied());
        }
        // A channel the stream lacks is silence.
        for c in ch..self.channels() {
            self.inp[c].extend(std::iter::repeat_n(0.0, n));
        }
        while self.inp[0].len() >= self.hop {
            for c in 0..self.channels() {
                for (i, s) in self.inp[c].drain(..self.hop).enumerate() {
                    self.noisy[[c, i]] = s;
                }
            }
            if self.df.process(self.noisy.view(), self.clean.view_mut()).is_err() {
                // As it was rather than nothing.
                self.clean.assign(&self.noisy);
            }
            for c in 0..self.channels() {
                self.out[c].extend(self.clean.row(c).iter().copied());
            }
        }
        for (c, p) in planes.iter_mut().enumerate().take(ch) {
            for s in p.iter_mut() {
                *s = self.out[c].pop_front().unwrap_or(0.0);
            }
        }
        for c in ch..self.channels() {
            let k = n.min(self.out[c].len());
            self.out[c].drain(..k);
        }
    }
}

impl std::fmt::Debug for Denoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser").field("channels", &self.channels()).field("strength", &self.strength).finish()
    }
}

/// A stereo block, there and back, with its number.
type Block = (u64, Vec<f32>, Vec<f32>);

/// Slow blocks in a row before the strong noise removal gives way to the light one.
const MISSES: u32 = 3;

/// Our engine's sound filter: a stereo Denoiser on a thread of its own (the model can't move
/// between threads, the mixer's filters do); each block goes there and comes back cleaned.
#[derive(Debug)]
struct Plugin {
    to: std::sync::mpsc::Sender<Block>,
    from: std::sync::mpsc::Receiver<Block>,
    /// The next block's number, slow blocks in a row now, and the model's delay (samples).
    seq: u64,
    missed: u32,
    latency: usize,
}

impl Plugin {
    fn start(strength: u32) -> Result<Self, String> {
        let (to, inbox) = std::sync::mpsc::channel::<Block>();
        let (outbox, from) = std::sync::mpsc::channel::<Block>();
        let (ready_tx, ready) = std::sync::mpsc::channel::<Result<usize, String>>();
        std::thread::Builder::new()
            .name("ferrocast-denoise".into())
            .spawn(move || {
                let mut d = match Denoiser::new(2, strength) {
                    Ok(d) => {
                        let _ = ready_tx.send(Ok(d.latency()));
                        d
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                // Until the filter goes (its sender with it).
                for (n, mut l, mut r) in inbox {
                    d.run(&mut [&mut l, &mut r]);
                    if outbox.send((n, l, r)).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let latency = ready.recv().map_err(|e| e.to_string())??;
        Ok(Self { to, from, seq: 0, missed: 0, latency })
    }
}

impl ferrocore::afx::SoundPlugin for Plugin {
    fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
        let [l, r] = ch;
        let n = self.seq;
        self.seq += 1;
        // Its thread gone: the light noise removal instead.
        if self.to.send((n, l.to_vec(), r.to_vec())).is_err() {
            return false;
        }
        // A block back within four of its own length (at least 100 ms), or the mixer mustn't
        // wait on it. Answers to blocks it gave up on before are let go as they come.
        let limit = std::time::Duration::from_secs_f64((l.len() as f64 / 48_000.0 * 4.0).max(0.1));
        let until = std::time::Instant::now() + limit;
        loop {
            let left = until.saturating_duration_since(std::time::Instant::now());
            match self.from.recv_timeout(left) {
                Ok((got, cl, cr)) if got == n && cl.len() >= l.len() && cr.len() >= r.len() => {
                    l.copy_from_slice(&cl[..l.len()]);
                    r.copy_from_slice(&cr[..r.len()]);
                    self.missed = 0;
                    return true;
                }
                Ok((got, ..)) if got < n => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // One slow block (the computer busy for a moment) is a moment of quiet, in
                    // step with the rest; a few in a row and this computer is too slow for it.
                    self.missed += 1;
                    if self.missed >= MISSES {
                        return false;
                    }
                    l.fill(0.0);
                    r.fill(0.0);
                    return true;
                }
                _ => return false,
            }
        }
    }

    fn latency_frames(&self) -> usize {
        self.latency
    }
}

/// Gives our engine the strong noise removal (once is enough).
pub(crate) fn install() {
    ferrocore::afx::set_denoiser(|strength| match Plugin::start(strength) {
        Ok(p) => Some(Box::new(p)),
        Err(e) => {
            tracing::warn!(error = %e, "the strong noise removal couldn't start");
            None
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A steady hiss (a fan), the same every run.
    fn hiss(n: usize, seed: &mut u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.2
            })
            .collect()
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
    }

    /// How late the cleaned sound comes out (in samples), found by matching it to what went in.
    fn delay_of(d: &mut Denoiser) -> usize {
        let mut seed = 3;
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for _ in 0..94 {
            // A rich, voice-like buzz (harmonics of 140 Hz) the lightest touch leaves alone.
            let base = before.len();
            let mut l: Vec<f32> = (0..1024)
                .map(|i| {
                    let t = (base + i) as f32 / 48_000.0;
                    (1..12).map(|k| (std::f32::consts::TAU * 140.0 * k as f32 * t).sin() / k as f32).sum::<f32>() * 0.1
                        + hiss(1, &mut seed)[0] * 0.05
                })
                .collect();
            let mut r = l.clone();
            before.extend_from_slice(&l);
            d.run(&mut [&mut l, &mut r]);
            after.extend_from_slice(&l);
        }
        let (a, b) = (&after[48_000..90_000], &before[..]);
        (0..4_800)
            .max_by(|x, y| {
                let c = |lag: usize| a.iter().enumerate().map(|(i, v)| v * b[48_000 + i - lag]).sum::<f32>();
                c(*x).total_cmp(&c(*y))
            })
            .unwrap_or(0)
    }

    /// The delay it says it has is the one it has (the mixer makes up for exactly that).
    #[test]
    fn its_delay_is_what_it_says() {
        let mut d = Denoiser::new(2, 1).expect("the built-in model starts");
        let said = d.latency();
        assert_eq!(delay_of(&mut d), said, "measured against reported, in samples");
        assert_eq!(said, 1920, "40 ms with the built-in model");
    }

    #[test]
    fn a_hiss_goes_and_every_sample_comes_back() {
        let mut d = Denoiser::new(2, 100).expect("the built-in model starts");
        let mut seed = 7;
        let (mut before, mut after) = (Vec::new(), Vec::new());
        // Two seconds in libobs's own odd-sized pieces (1024 frames), stereo.
        for _ in 0..94 {
            let mut l = hiss(1024, &mut seed);
            let mut r = l.clone();
            before.extend_from_slice(&l);
            d.run(&mut [&mut l, &mut r]);
            assert_eq!(l, r, "both channels alike");
            after.extend_from_slice(&l);
        }
        assert_eq!(before.len(), after.len(), "as many samples out as in");
        // The last second, once it has settled.
        let (b, a) = (rms(&before[48_000..]), rms(&after[48_000..]));
        assert!(a < b * 0.1, "the hiss is at least 20 dB quieter: {b} → {a}");
    }

    #[test]
    fn a_lighter_touch_leaves_some_of_it() {
        let mut d = Denoiser::new(1, 20).expect("the built-in model starts");
        let mut seed = 9;
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for _ in 0..94 {
            let mut m = hiss(1024, &mut seed);
            before.extend_from_slice(&m);
            d.run(&mut [&mut m]);
            after.extend_from_slice(&m);
        }
        let (b, a) = (rms(&before[48_000..]), rms(&after[48_000..]));
        // 20% is at most 12 dB less: a quarter of the level or more.
        assert!(a > b * 0.2, "some of it stays: {b} → {a}");
    }

    #[test]
    fn strength_maps_to_a_limit() {
        assert_eq!(atten_db(100), 100.0);
        assert_eq!(atten_db(50), 30.0);
        assert_eq!(atten_db(0), 0.6);
    }

    use ferrocore::afx::{AudioEffect, AudioFilter};

    #[test]
    fn our_engine_takes_the_noise_out() {
        super::install();
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength: 100 }, 48_000);
        // A second of steady hiss, in 10 ms blocks; the second half measured.
        let mut seed = 1u32;
        let (mut before, mut after) = (0.0f64, 0.0f64);
        for block in 0..100 {
            let mut l: Vec<f32> = (0..480)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1 << 24) as f32 * 0.2 - 0.1
                })
                .collect();
            let mut r = l.clone();
            let energy: f64 = l.iter().map(|v| f64::from(*v).powi(2)).sum();
            f.process([&mut l, &mut r]);
            if block >= 50 {
                before += energy;
                after += l.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
            }
        }
        assert!(after < before * 0.1, "hiss mostly gone: {after:.3} of {before:.3}");
    }
}
