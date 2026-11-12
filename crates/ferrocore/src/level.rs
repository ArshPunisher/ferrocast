//! Sound that looks after itself: ducking (music quieter while you talk) and loudness (each
//! track steered to a target, as platforms measure it: ITU-R BS.1770 K-weighting, 3 s windows,
//! then a peak limiter).

/// Music quieter while the voices are heard.
#[derive(Debug, Clone, PartialEq)]
pub struct Ducking {
    /// The voices (mics) that duck, and the sounds (music) they duck.
    pub voices: Vec<u64>,
    pub music: Vec<u64>,
    /// How much quieter the music goes (dB, below 0).
    pub depth_db: f32,
    /// A voice above this (dBFS peak) counts as talking.
    pub threshold_db: f32,
}

impl Default for Ducking {
    fn default() -> Self {
        Ducking { voices: Vec::new(), music: Vec::new(), depth_db: -12.0, threshold_db: -40.0 }
    }
}

/// Ducking running: the music's gain now, and how long since a voice was heard.
#[derive(Debug, Clone)]
pub(crate) struct Ducker {
    pub cfg: Ducking,
    gain: f32,
    quiet_s: f32,
}

/// How quickly the music goes down, how long it waits after the last word, how quickly it comes back.
const DUCK_ATTACK_S: f32 = 0.08;
const DUCK_HOLD_S: f32 = 0.5;
const DUCK_RELEASE_S: f32 = 0.6;

impl Ducker {
    pub fn new(cfg: Ducking) -> Self {
        Ducker { cfg, gain: 1.0, quiet_s: DUCK_HOLD_S }
    }

    /// One block of `secs`: whether a voice is heard → the music's gain at its start and end.
    pub fn step(&mut self, talking: bool, secs: f32) -> (f32, f32) {
        let from = self.gain;
        self.quiet_s = if talking { 0.0 } else { self.quiet_s + secs };
        let low = 10f32.powf(self.cfg.depth_db.min(0.0) / 20.0);
        let (want, take) = if self.quiet_s < DUCK_HOLD_S { (low, DUCK_ATTACK_S) } else { (1.0, DUCK_RELEASE_S) };
        // The whole way down (or up) in `take`, evenly: it arrives, no long tail.
        let step = (1.0 - low) * secs / take;
        self.gain += (want - self.gain).clamp(-step, step);
        (from, self.gain)
    }
}

/// A biquad, one channel.
#[derive(Debug, Clone, Copy, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// BS.1770's K-weighting at `rate`: the head's shelf, then the RLB high-pass.
fn k_weighting(rate: f64) -> [Biquad; 2] {
    use std::f64::consts::PI;
    let (g, fc, q) = (3.999_843_853_973_347, 1_681.974_450_955_533, 0.707_175_236_955_419_6);
    let k = (PI * fc / rate).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad {
        b: [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
        a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        z: [0.0; 2],
    };
    let (fc, q) = (38.135_470_876_024_44, 0.500_327_037_323_877_3);
    let k = (PI * fc / rate).tan();
    let a0 = 1.0 + k / q + k * k;
    let rlb = Biquad { b: [1.0, -2.0, 1.0], a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0], z: [0.0; 2] };
    [shelf, rlb]
}

/// Below this (LUFS) it's silence: the gain holds rather than lifting the noise.
const GATE_LUFS: f64 = -50.0;
/// The gain's range and how fast it may move (dB per second): slow enough not to pump.
const MAX_GAIN_DB: f64 = 12.0;
const GAIN_DB_PER_S: f64 = 1.5;
/// The limiter's ceiling (-1 dBFS) and its release.
const CEILING: f32 = 0.891;
const LIMIT_RELEASE_S: f32 = 0.08;

/// One track steered to a loudness, then limited.
#[derive(Debug, Clone)]
pub(crate) struct Loudness {
    target: f64,
    rate: f64,
    k: [[Biquad; 2]; 2],
    /// Mean squares of the last 3 s, 100 ms at a time.
    windows: std::collections::VecDeque<f64>,
    acc: f64,
    acc_n: usize,
    gain_db: f64,
    limit: f32,
}

impl Loudness {
    pub fn new(target_lufs: f32, rate: u32) -> Self {
        let k = k_weighting(f64::from(rate));
        Loudness {
            target: f64::from(target_lufs),
            rate: f64::from(rate),
            k: [k, k],
            windows: Default::default(),
            acc: 0.0,
            acc_n: 0,
            gain_db: 0.0,
            limit: 1.0,
        }
    }

    /// The short-term loudness now (LUFS), from what came in before the gain.
    pub fn lufs(&self) -> Option<f64> {
        if self.windows.is_empty() {
            return None;
        }
        let ms = self.windows.iter().sum::<f64>() / self.windows.len() as f64;
        (ms > 0.0).then(|| -0.691 + 10.0 * ms.log10())
    }

    /// The gain it applies now (dB).
    #[cfg(test)]
    pub fn gain_db(&self) -> f64 {
        self.gain_db
    }

    /// One interleaved stereo block, in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        let hop = (self.rate * 0.1) as usize;
        for f in buf.as_chunks::<2>().0 {
            let mut sum = 0.0;
            for (c, x) in f.iter().enumerate() {
                let y = self.k[c].iter_mut().fold(f64::from(*x), |v, b| b.run(v));
                sum += y * y;
            }
            self.acc += sum;
            self.acc_n += 1;
            if self.acc_n == hop {
                self.windows.push_back(self.acc / hop as f64);
                if self.windows.len() > 30 {
                    self.windows.pop_front();
                }
                self.acc = 0.0;
                self.acc_n = 0;
            }
        }
        let secs = (buf.len() / 2) as f64 / self.rate;
        if let Some(l) = self.lufs().filter(|l| *l > GATE_LUFS) {
            let want = (self.target - l).clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
            let step = GAIN_DB_PER_S * secs;
            self.gain_db += (want - self.gain_db).clamp(-step, step);
        }
        let g = 10f32.powf(self.gain_db as f32 / 20.0);
        let release = 1.0 - (-1.0 / (LIMIT_RELEASE_S * self.rate as f32)).exp();
        for f in buf.as_chunks_mut::<2>().0 {
            let (l, r) = (f[0] * g, f[1] * g);
            let peak = l.abs().max(r.abs());
            // Down at once to fit under the ceiling, back up gently.
            let need = if peak * self.limit > CEILING { CEILING / peak } else { 1.0 };
            self.limit = if need < self.limit { need } else { self.limit + (need - self.limit) * release };
            f[0] = (l * self.limit).clamp(-1.0, 1.0);
            f[1] = (r * self.limit).clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn tone(amp: f32, secs: f32) -> Vec<f32> {
        (0..(RATE as f32 * secs) as usize)
            .flat_map(|i| {
                let v = (i as f32 * 997.0 * std::f32::consts::TAU / RATE as f32).sin() * amp;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn loudness_is_measured_as_bs1770_says() {
        // A 997 Hz tone at 0.1 in both channels reads -20 LUFS.
        let mut l = Loudness::new(-20.0, RATE);
        for block in tone(0.1, 3.0).chunks(2048) {
            let mut b = block.to_vec();
            l.process(&mut b);
        }
        let lufs = l.lufs().unwrap();
        assert!((lufs + 20.0).abs() < 0.3, "{lufs}");
    }

    #[test]
    fn a_quiet_track_is_brought_up_to_the_target_and_peaks_stay_under() {
        let mut l = Loudness::new(-14.0, RATE);
        let mut out = Vec::new();
        for block in tone(0.05, 12.0).chunks(2048) {
            let mut b = block.to_vec();
            l.process(&mut b);
            out = b;
        }
        // -26 LUFS in: up 12 dB (its most), slowly; peaks never over -1 dBFS.
        assert!((l.gain_db() - 12.0).abs() < 0.5, "{}", l.gain_db());
        assert!(out.iter().all(|v| v.abs() <= CEILING + 1e-4));
        // A loud one comes down instead.
        let mut l = Loudness::new(-14.0, RATE);
        for block in tone(0.9, 6.0).chunks(2048) {
            let mut b = block.to_vec();
            l.process(&mut b);
            assert!(b.iter().all(|v| v.abs() <= CEILING + 1e-4));
        }
        assert!(l.gain_db() < -5.0, "{}", l.gain_db());
        // Silence doesn't lift the noise.
        let mut l = Loudness::new(-14.0, RATE);
        let mut quiet = vec![0.0001f32; 96_000];
        l.process(&mut quiet);
        assert_eq!(l.gain_db(), 0.0);
    }

    #[test]
    fn music_ducks_while_talking_and_comes_back_after() {
        let mut d = Ducker::new(Ducking::default());
        let block = 1024.0 / RATE as f32;
        let mut g = 1.0;
        for _ in 0..20 {
            g = d.step(true, block).1;
        }
        assert!((g - 0.251).abs() < 0.02, "down 12 dB: {g}");
        // Still down just after the last word.
        for _ in 0..10 {
            g = d.step(false, block).1;
        }
        assert!(g < 0.3, "held: {g}");
        for _ in 0..100 {
            g = d.step(false, block).1;
        }
        assert!(g > 0.95, "back: {g}");
    }
}
