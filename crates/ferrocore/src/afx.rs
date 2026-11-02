//! OBS's audio filters, ported from plugins/obs-filters line for line so they sound the same:
//! gain, invert polarity, noise gate, compressor, limiter, expander (and its gate preset),
//! upward compressor, and the 3-band EQ. Each keeps its state between blocks; stereo, planar.

/// libobs's `db_to_mul` and `mul_to_db`.
pub fn db_to_mul(db: f32) -> f32 {
    if db.is_finite() { 10f32.powf(db / 20.0) } else { 0.0 }
}

pub fn mul_to_db(mul: f32) -> f32 {
    if mul == 0.0 { f32::NEG_INFINITY } else { 20.0 * mul.log10() }
}

/// A sound filter made outside the engine (DeepFilterNet's noise removal, which the app builds
/// in): it cleans stereo blocks in place, keeping its own state. False: it couldn't (gone, or
/// too slow for this computer); the block is left as it was and the engine's light noise
/// removal takes over.
pub trait SoundPlugin: Send + std::fmt::Debug {
    fn process(&mut self, ch: [&mut [f32]; 2]) -> bool;
}

/// Makes the strong noise removal at a strength (1 to 100), when the app has one.
pub type MakeDenoiser = fn(u32) -> Option<Box<dyn SoundPlugin>>;

static DENOISER: std::sync::OnceLock<MakeDenoiser> = std::sync::OnceLock::new();

/// The app's strong noise removal (set once; later calls change nothing).
pub fn set_denoiser(make: MakeDenoiser) {
    let _ = DENOISER.set(make);
}

/// Whether strong noise removal is here (the app built with it).
pub fn denoiser_available() -> bool {
    DENOISER.get().is_some()
}

/// A filter's kind and settings (OBS's keys and defaults in the comments).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioEffect {
    /// "db", 0.
    Gain {
        db: f32,
    },
    InvertPolarity,
    /// "open_threshold" -26, "close_threshold" -32, "attack_time" 25, "hold_time" 200,
    /// "release_time" 150 (ms).
    NoiseGate {
        open_db: f32,
        close_db: f32,
        attack_ms: f32,
        hold_ms: f32,
        release_ms: f32,
    },
    /// "ratio" 10, "threshold" -18, "attack_time" 6, "release_time" 60, "output_gain" 0.
    Compressor {
        ratio: f32,
        threshold_db: f32,
        attack_ms: f32,
        release_ms: f32,
        output_db: f32,
    },
    /// "threshold" -6, "release_time" 60.
    Limiter {
        threshold_db: f32,
        release_ms: f32,
    },
    /// Expander and its gate preset: "ratio" 2 (gate 10), "threshold" -40, "attack_time" 10,
    /// "release_time" 50 (gate 125), "output_gain" 0, "detector" RMS or peak.
    Expander {
        ratio: f32,
        threshold_db: f32,
        attack_ms: f32,
        release_ms: f32,
        output_db: f32,
        rms: bool,
    },
    /// "ratio" 0.5, "threshold" -20, "attack_time" 10, "release_time" 50, "output_gain" 0,
    /// "knee_width" 10.
    UpwardCompressor {
        ratio: f32,
        threshold_db: f32,
        attack_ms: f32,
        release_ms: f32,
        output_db: f32,
        knee_db: f32,
    },
    /// "low", "mid", "high" in dB (crossovers at 800 Hz and 5 kHz).
    Eq3 {
        low_db: f32,
        mid_db: f32,
        high_db: f32,
    },
    /// Strong noise removal (DeepFilterNet, 48 kHz): "strength" 1 to 100 (100).
    Denoise {
        strength: u32,
    },
}

#[derive(Debug, Clone, Copy, Default)]
struct EqState {
    lf: [f32; 4],
    hf: [f32; 4],
    s: [f32; 3],
}

/// A filter running: its settings and what it remembers.
#[derive(Debug)]
pub struct AudioFilter {
    pub effect: AudioEffect,
    rate: f32,
    // Gate.
    is_open: bool,
    attenuation: f32,
    level: f32,
    held: f32,
    // Compressor, limiter: the envelope; expander: per channel RMS and gain.
    envelope: f32,
    runave: [f32; 2],
    gain_db: [f32; 2],
    eq: [EqState; 2],
    /// A filter made outside the engine (noise removal).
    plugin: Option<Box<dyn SoundPlugin>>,
    // Light noise removal: the noise floor it hears (RMS) and the gain now.
    floor: f32,
    nr_gain: f32,
}

/// libobs's compressor `gain_coefficient`.
fn coefficient(rate: f32, secs: f32) -> f32 {
    (-1.0 / (rate * secs)).exp()
}

impl AudioFilter {
    pub fn new(effect: AudioEffect, rate: u32) -> Self {
        Self {
            effect,
            rate: rate as f32,
            is_open: false,
            attenuation: 0.0,
            level: 0.0,
            held: 0.0,
            envelope: 0.0,
            runave: [0.0; 2],
            gain_db: [0.0; 2],
            eq: [EqState::default(); 2],
            plugin: match effect {
                // Its model works at 48 kHz only; elsewhere the sound passes as it is.
                AudioEffect::Denoise { strength } if rate == 48_000 => {
                    DENOISER.get().and_then(|make| make(strength.clamp(1, 100)))
                }
                _ => None,
            },
            floor: 0.0,
            nr_gain: 1.0,
        }
    }

    /// Filters a block in place (both sides the same length).
    pub fn process(&mut self, ch: [&mut [f32]; 2]) {
        let [l, r] = ch;
        let n = l.len().min(r.len());
        let rate = self.rate;
        if let Some(p) = self.plugin.as_mut() {
            if p.process([&mut *l, &mut *r]) {
                return;
            }
            tracing::warn!("the strong noise removal fell behind or stopped: the light one takes over");
            self.plugin = None;
        }
        match self.effect {
            AudioEffect::Gain { db } => {
                let m = db_to_mul(db);
                for v in l.iter_mut().chain(r.iter_mut()) {
                    *v *= m;
                }
            }
            AudioEffect::InvertPolarity => {
                for v in l.iter_mut().chain(r.iter_mut()) {
                    *v = -*v;
                }
            }
            AudioEffect::NoiseGate { open_db, close_db, attack_ms, hold_ms, release_ms } => {
                let open = db_to_mul(open_db);
                let close = db_to_mul(close_db);
                let attack_rate = 1.0 / (attack_ms / 1000.0 * rate);
                let release_rate = 1.0 / (release_ms / 1000.0 * rate);
                let decay_rate = (open - close) / ((1.0 / 75.0) * rate);
                let hold = hold_ms / 1000.0;
                let rate_i = 1.0 / rate;
                for i in 0..n {
                    let cur = l[i].abs().max(r[i].abs());
                    if cur > open && !self.is_open {
                        self.is_open = true;
                    }
                    if self.level < close && self.is_open {
                        self.held = 0.0;
                        self.is_open = false;
                    }
                    self.level = self.level.max(cur) - decay_rate;
                    if self.is_open {
                        self.attenuation = (self.attenuation + attack_rate).min(1.0);
                    } else {
                        self.held += rate_i;
                        if self.held > hold {
                            self.attenuation = (self.attenuation - release_rate).max(0.0);
                        }
                    }
                    l[i] *= self.attenuation;
                    r[i] *= self.attenuation;
                }
            }
            AudioEffect::Compressor { ratio, threshold_db, attack_ms, release_ms, output_db } => {
                self.compress(
                    l,
                    r,
                    n,
                    1.0 - 1.0 / ratio.max(1.0),
                    threshold_db,
                    attack_ms,
                    release_ms,
                    db_to_mul(output_db),
                );
            }
            AudioEffect::Limiter { threshold_db, release_ms } => {
                self.compress(l, r, n, 1.0, threshold_db, 0.001, release_ms, 1.0);
            }
            AudioEffect::Expander { ratio, threshold_db, attack_ms, release_ms, output_db, rms } => {
                self.expand(l, r, n, false, ratio, threshold_db, attack_ms, release_ms, output_db, 0.0, rms);
            }
            AudioEffect::UpwardCompressor { ratio, threshold_db, attack_ms, release_ms, output_db, knee_db } => {
                self.expand(l, r, n, true, ratio, threshold_db, attack_ms, release_ms, output_db, knee_db, true);
            }
            AudioEffect::Eq3 { low_db, mid_db, high_db } => {
                let lf = 2.0 * (std::f32::consts::PI * 800.0 / rate).sin();
                let hf = 2.0 * (std::f32::consts::PI * 5000.0 / rate).sin();
                let (lg, mg, hg) = (db_to_mul(low_db), db_to_mul(mid_db), db_to_mul(high_db));
                const EPS: f32 = 1.0 / 4_294_967_295.0;
                for (c, data) in [&mut *l, &mut *r].into_iter().enumerate() {
                    let st = &mut self.eq[c];
                    for v in data.iter_mut().take(n) {
                        let sample = *v;
                        st.lf[0] += lf * (sample - st.lf[0]) + EPS;
                        st.lf[1] += lf * (st.lf[0] - st.lf[1]);
                        st.lf[2] += lf * (st.lf[1] - st.lf[2]);
                        st.lf[3] += lf * (st.lf[2] - st.lf[3]);
                        let lo = st.lf[3];
                        st.hf[0] += hf * (sample - st.hf[0]) + EPS;
                        st.hf[1] += hf * (st.hf[0] - st.hf[1]);
                        st.hf[2] += hf * (st.hf[1] - st.hf[2]);
                        st.hf[3] += hf * (st.hf[2] - st.hf[3]);
                        let hi = st.s[2] - st.hf[3];
                        let mid = st.s[2] - (hi + lo);
                        st.s[2] = st.s[1];
                        st.s[1] = st.s[0];
                        st.s[0] = sample;
                        *v = lo * lg + mid * mg + hi * hg;
                    }
                }
            }
            // Without the app's strong noise removal (not in this build, it didn't start, or this
            // computer is too slow for it): the light one.
            AudioEffect::Denoise { strength } => self.light_denoise(&mut l[..n], &mut r[..n], strength),
        }
    }

    /// Light noise removal, for any computer: it follows the noise floor (the quietest the sound
    /// gets, rising slowly) and turns down what stays near it (hiss, hum, a fan between words)
    /// by up to 24 dB at strength 100; a voice above it passes untouched.
    fn light_denoise(&mut self, l: &mut [f32], r: &mut [f32], strength: u32) {
        let n = l.len();
        if n == 0 {
            return;
        }
        let rms = (l.iter().chain(r.iter()).map(|v| v * v).sum::<f32>() / (2 * n) as f32).sqrt();
        let secs = n as f32 / self.rate;
        // Down at once to anything quieter; up 3 dB a second (a louder room is learnt).
        self.floor = if self.floor <= 0.0 || rms < self.floor { rms } else { self.floor * db_to_mul(3.0 * secs) };
        let depth = 24.0 * strength.clamp(1, 100) as f32 / 100.0;
        // Within 6 dB of the floor is noise; 12 dB over is surely not: in between, partly.
        let over = mul_to_db(rms.max(1e-9) / self.floor.max(1e-9));
        let target = db_to_mul(-depth * (1.0 - ((over - 6.0) / 6.0).clamp(0.0, 1.0)));
        // Opens in 5 ms (a word's start isn't clipped), closes in 120 ms.
        let open = 1.0 - coefficient(self.rate, 0.005);
        let close = 1.0 - coefficient(self.rate, 0.120);
        for i in 0..n {
            let k = if target > self.nr_gain { open } else { close };
            self.nr_gain += (target - self.nr_gain) * k;
            l[i] *= self.nr_gain;
            r[i] *= self.nr_gain;
        }
    }

    /// libobs's compressor and limiter: a peak envelope (the louder side), then gain.
    #[allow(clippy::too_many_arguments)]
    fn compress(
        &mut self,
        l: &mut [f32],
        r: &mut [f32],
        n: usize,
        slope: f32,
        threshold: f32,
        attack_ms: f32,
        release_ms: f32,
        out: f32,
    ) {
        let ag = coefficient(self.rate, attack_ms / 1000.0);
        let rg = coefficient(self.rate, release_ms / 1000.0);
        let mut env_buf = vec![0f32; n];
        for data in [&*l, &*r] {
            let mut env = self.envelope;
            for (i, s) in data.iter().take(n).enumerate() {
                let x = s.abs();
                env = if env < x { x + ag * (env - x) } else { x + rg * (env - x) };
                env_buf[i] = env_buf[i].max(env);
            }
        }
        if let Some(last) = env_buf.last() {
            self.envelope = *last;
        }
        for (i, e) in env_buf.iter().enumerate() {
            let gain = db_to_mul((slope * (threshold - mul_to_db(*e))).min(0.0));
            l[i] *= gain * out;
            r[i] *= gain * out;
        }
    }

    /// libobs's expander, gate preset and upward compressor: per side, RMS (10 ms) or peak
    /// detection, gain in dB with attack and release.
    #[allow(clippy::too_many_arguments)]
    fn expand(
        &mut self,
        l: &mut [f32],
        r: &mut [f32],
        n: usize,
        upward: bool,
        ratio: f32,
        threshold: f32,
        attack_ms: f32,
        release_ms: f32,
        output_db: f32,
        knee: f32,
        rms: bool,
    ) {
        let ag = coefficient(self.rate, attack_ms / 1000.0);
        let rg = coefficient(self.rate, release_ms / 1000.0);
        let slope = 1.0 - ratio;
        let out = db_to_mul(output_db);
        let rmscoef = (-100.0 / self.rate).exp2();
        for (c, data) in [&mut *l, &mut *r].into_iter().enumerate() {
            // Each side's envelope (libobs keeps the per-sample max over sides only per side).
            let mut env = vec![0f32; n];
            let mut run = self.runave[c];
            for (i, s) in data.iter().take(n).enumerate() {
                if rms {
                    run = rmscoef * run + (1.0 - rmscoef) * s * s;
                    env[i] = run.max(0.0).sqrt();
                } else {
                    run = s * s;
                    env[i] = s.abs();
                }
            }
            self.runave[c] = run;
            let mut prev = self.gain_db[c];
            for i in 0..n {
                let env_db = mul_to_db(env[i]);
                let mut diff = threshold - env_db;
                if upward && env_db <= (threshold - 60.0) / 2.0 {
                    diff = if env_db + 60.0 > 0.0 { env_db + 60.0 } else { 0.0 };
                }
                let p = if upward { prev.max(0.0) } else { prev };
                let gain = if upward {
                    if env_db >= threshold + knee / 2.0 {
                        0.0
                    } else if threshold - knee / 2.0 >= env_db {
                        slope * diff
                    } else {
                        slope * (diff + knee / 2.0).powi(2) / (2.0 * knee)
                    }
                } else if diff > 0.0 {
                    (slope * diff).max(-60.0)
                } else {
                    0.0
                };
                let g = if gain > p { ag * p + (1.0 - ag) * gain } else { rg * p + (1.0 - rg) * gain };
                prev = g;
                let m = if upward { db_to_mul(g) } else { db_to_mul(g.min(0.0)) };
                data[i] *= m * out;
            }
            self.gain_db[c] = prev;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn run(effect: AudioEffect, input: &[f32]) -> Vec<f32> {
        let mut f = AudioFilter::new(effect, RATE);
        let (mut l, mut r) = (input.to_vec(), input.to_vec());
        for (a, b) in l.chunks_mut(1024).zip(r.chunks_mut(1024)) {
            f.process([a, b]);
        }
        l
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// A slow plugin that gives up, as the strong noise removal does on a slow computer.
    #[derive(Debug)]
    struct GivesUp;
    impl SoundPlugin for GivesUp {
        fn process(&mut self, _: [&mut [f32]; 2]) -> bool {
            false
        }
    }

    #[test]
    fn light_noise_removal_quiets_the_room_not_the_voice() {
        // A fan's hiss (-50 dBFS), then a voice over it (-12 dBFS), then the fan alone again.
        let hiss: Vec<f32> = (0..RATE as usize).map(|i| ((i * 7919 % 1000) as f32 / 500.0 - 1.0) * 0.0055).collect();
        let voice: Vec<f32> = sine(220.0, 0.35, 1.0).iter().zip(&hiss).map(|(v, h)| v + h).collect();
        let input: Vec<f32> = hiss.iter().chain(&voice).chain(&hiss).copied().collect();
        let out = run(AudioEffect::Denoise { strength: 100 }, &input);
        let r = RATE as usize;
        // The fan between words: about 24 dB quieter once the floor is learnt.
        let fan = mul_to_db(rms(&out[2 * r + r / 2..3 * r]) / rms(&input[2 * r + r / 2..3 * r]));
        assert!(fan < -18.0, "{fan} dB");
        // The voice: as it was (after its first 50 ms).
        let said = mul_to_db(rms(&out[r + r / 20..2 * r]) / rms(&input[r + r / 20..2 * r]));
        assert!(said > -1.0, "{said} dB");
    }

    #[test]
    fn a_plugin_that_gives_up_hands_over_to_the_light_one() {
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength: 100 }, RATE);
        f.plugin = Some(Box::new(GivesUp));
        let (mut l, mut r) = (vec![0.01; 1024], vec![0.01; 1024]);
        f.process([&mut l, &mut r]);
        assert!(f.plugin.is_none(), "it's dropped");
        assert!(f.floor > 0.0, "and the light one ran on the same block");
    }

    fn sine(hz: f32, amp: f32, secs: f32) -> Vec<f32> {
        (0..(RATE as f32 * secs) as usize)
            .map(|i| (i as f32 * hz * std::f32::consts::TAU / RATE as f32).sin() * amp)
            .collect()
    }

    fn peak(v: &[f32]) -> f32 {
        v.iter().fold(0.0, |a, x| a.max(x.abs()))
    }

    #[test]
    fn gain_and_polarity() {
        let out = run(AudioEffect::Gain { db: -6.0 }, &[0.5, -0.5]);
        assert!((out[0] - 0.5 * db_to_mul(-6.0)).abs() < 1e-6);
        assert_eq!(run(AudioEffect::InvertPolarity, &[0.25])[0], -0.25);
    }

    #[test]
    fn gate_mutes_quiet_and_opens_for_loud() {
        let g = AudioEffect::NoiseGate {
            open_db: -26.0,
            close_db: -32.0,
            attack_ms: 25.0,
            hold_ms: 200.0,
            release_ms: 150.0,
        };
        // -40 dB hiss stays shut.
        assert!(peak(&run(g, &sine(440.0, 0.01, 1.0))) < 1e-6);
        // A -6 dB voice opens it fully after the attack.
        let out = run(g, &sine(440.0, 0.5, 1.0));
        assert!((peak(&out[4800..]) - 0.5).abs() < 0.01);
    }

    #[test]
    fn compressor_reaches_its_ratio() {
        // A steady tone 12 dB over a -18 dB threshold at 4:1: 9 dB lower in theory; libobs's
        // peak envelope relaxes between a sine's peaks, so a little less (about 8).
        let input = sine(1000.0, db_to_mul(-6.0), 1.0);
        let c = AudioEffect::Compressor {
            ratio: 4.0,
            threshold_db: -18.0,
            attack_ms: 6.0,
            release_ms: 60.0,
            output_db: 0.0,
        };
        let out = run(c, &input);
        let reduction = mul_to_db(peak(&out[RATE as usize / 2..])) - mul_to_db(peak(&input));
        assert!((-9.5..-7.5).contains(&reduction), "{reduction} dB");
    }

    #[test]
    fn limiter_holds_the_ceiling() {
        let out = run(AudioEffect::Limiter { threshold_db: -6.0, release_ms: 60.0 }, &sine(200.0, 1.0, 1.0));
        assert!(mul_to_db(peak(&out[2400..])) < -5.9);
    }

    #[test]
    fn expander_lowers_what_is_under_the_threshold() {
        let e = AudioEffect::Expander {
            ratio: 2.0,
            threshold_db: -40.0,
            attack_ms: 10.0,
            release_ms: 50.0,
            output_db: 0.0,
            rms: true,
        };
        // A -50 dB tone (RMS about -53) is pushed down further; a loud one is left.
        let quiet = sine(500.0, db_to_mul(-50.0), 1.0);
        assert!(peak(&run(e, &quiet)[24_000..]) < peak(&quiet) * 0.5);
        let loud = sine(500.0, 0.5, 1.0);
        assert!((peak(&run(e, &loud)[24_000..]) - 0.5).abs() < 0.01);
    }

    #[test]
    fn eq_flat_is_a_delay_and_bands_cut() {
        // Flat: the same sound (libobs's EQ is three samples late on the mid band).
        let input = sine(1000.0, 0.5, 0.5);
        let flat = run(AudioEffect::Eq3 { low_db: 0.0, mid_db: 0.0, high_db: 0.0 }, &input);
        assert!((peak(&flat[4800..]) - 0.5).abs() < 0.02);
        // Low band down 20 dB: a 100 Hz tone drops (libobs's bands overlap, so not by the full
        // 20 dB), a 2 kHz one hardly.
        let cut = AudioEffect::Eq3 { low_db: -20.0, mid_db: 0.0, high_db: 0.0 };
        let low = peak(&run(cut, &sine(100.0, 0.5, 0.5))[12_000..]);
        assert!(mul_to_db(low / 0.5) < -6.0, "100 Hz down {} dB", mul_to_db(low / 0.5));
        assert!(peak(&run(cut, &sine(2000.0, 0.5, 0.5))[12_000..]) > 0.4);
    }
}
