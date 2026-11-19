//! Strong noise removal: DeepFilterNet 3 (MIT/Apache-2.0, github.com/Rikorose/DeepFilterNet) on
//! the CPU through tract, its model built in. It takes out what RNNoise leaves (keyboards, fans,
//! a busy room) and keeps the voice natural. It works on 10 ms blocks at 48 kHz and hands each
//! back one block later (10 ms, plus the model's own look ahead). Built only with the
//! `deepfilter` feature (the release builds), so day-to-day builds don't compile the model.

use std::collections::VecDeque;

use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray15::Array2;

/// Maximum attenuation limit (dB) at strength 100: caps DeepFilterNet so it never hits its
/// unlimited (>=100 dB) hard-zeroing cliff that eats soft voice tails and consonants (M2-01).
const MAX_ATTEN_DB: f32 = 30.0;

/// How much quieter noise may get at a strength (1 to 100): smooth 0.3 dB per step up to
/// [`MAX_ATTEN_DB`] (30 dB), with no jump between 99 and 100.
fn atten_db(strength: u32) -> f32 {
    // 0.3 dB a step, worked in whole numbers (50 is 15.0 dB, 65 is 19.5 dB, 80 is 24.0 dB, 100 is 30.0 dB).
    let db = (strength.clamp(1, 100) * 3) as f32 / 10.0;
    db.min(MAX_ATTEN_DB)
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
/// What the denoiser's thread is handed: the next block (left, right), or a new strength.
enum Ask {
    Block(Vec<f32>, Vec<f32>),
    Strength(u32),
}

/// The built-in model's own delay (samples): `Denoiser::latency` once it's made, known before
/// (a test holds them equal), so the mixer files the sound the same way from the first block.
const MODEL_LATENCY: usize = 1920;
/// How far behind the model's answers are taken: two mixer blocks. The mixer never waits for
/// the model (it ran inside the frame loop and held it up to 100 ms when the computer was busy,
/// 18 Nov); the frame loop mixes two blocks back to back now and then (30 fps), so one block
/// behind left the second's answer not back yet a third of the time, and it gave up.
const PIPE: usize = 2048;
/// The most the mixer waits for an answer that's nearly there.
const GRACE: std::time::Duration = std::time::Duration::from_millis(2);
/// Samples a change to the sound as it came is eased over when there's no answer to fade from.
const DECLICK: usize = 64;
/// The longest the model may take to get ready before the light noise removal is used instead.
const READY_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a stretch is judged over, and the share of it that may pass dry (the model's answer
/// late) before the light noise removal takes over: a computer just too slow for it flips
/// between the two every few blocks, and the noise pumped all stream long.
const JUDGE_SECS: usize = 10;
const MOST_DRY: f32 = 0.2;

/// Our engine's sound filter: a stereo Denoiser on a thread of its own (the model can't move
/// between threads, the mixer's filters do). The model is made on that thread (it takes a moment)
/// and every block goes there in order; what comes back is played one block later. Until the
/// model is ready, or when an answer is late, the sound plays as it came, delayed the same, so
/// nothing is silenced and nothing moves in time; switching between the two fades over a block.
#[derive(Debug)]
struct Plugin {
    to: std::sync::mpsc::Sender<Ask>,
    from: std::sync::mpsc::Receiver<(Vec<f32>, Vec<f32>)>,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The model's answers not played yet (planar), and the index of the first one.
    wet: [VecDeque<f32>; 2],
    wet_at: u64,
    /// Samples handed out so far.
    out_at: u64,
    /// What came in, `latency` samples ago (what plays when there's no answer).
    dry: [VecDeque<f32>; 2],
    /// The last block was the model's.
    was_wet: bool,
    /// Samples judged in this stretch, and how many of them passed dry, since the model was
    /// ready.
    judged: usize,
    dry_run: usize,
    /// Too slow for this computer: the next block isn't taken.
    gave_up: bool,
    /// The last sample handed out, each side.
    last: [f32; 2],
    /// When it was made: a model that never gets ready gives up (its blocks would pile up).
    made: std::time::Instant,
    strength: u32,
}

impl Plugin {
    fn start(strength: u32) -> Result<Self, String> {
        let (to, inbox) = std::sync::mpsc::channel::<Ask>();
        let (outbox, from) = std::sync::mpsc::channel();
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let up = ready.clone();
        std::thread::Builder::new()
            .name("ferrocast-denoise".into())
            .spawn(move || {
                // As soon as the frame loop: it waits for nothing here any more, but a model
                // woken late leaves the sound dry.
                ferrocore::engine::raise_this_thread();
                let mut d = match Denoiser::new(2, strength) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(error = %e, "the strong noise removal couldn't start");
                        return;
                    }
                };
                if d.latency() != MODEL_LATENCY {
                    tracing::warn!(latency = d.latency(), "the noise removal's model has another delay");
                }
                up.store(true, std::sync::atomic::Ordering::Release);
                // Until the filter goes (its sender with it).
                for ask in inbox {
                    match ask {
                        Ask::Strength(s) => d.set_strength(s),
                        Ask::Block(mut l, mut r) => {
                            d.run(&mut [&mut l, &mut r]);
                            if outbox.send((l, r)).is_err() {
                                return;
                            }
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let silence = || std::iter::repeat_n(0.0, MODEL_LATENCY + PIPE).collect::<VecDeque<f32>>();
        Ok(Self {
            to,
            from,
            ready,
            wet: [VecDeque::new(), VecDeque::new()],
            wet_at: 0,
            out_at: 0,
            dry: [silence(), silence()],
            was_wet: false,
            judged: 0,
            dry_run: 0,
            gave_up: false,
            last: [0.0; 2],
            made: std::time::Instant::now(),
            strength,
        })
    }

    /// Takes the answers that came back.
    fn take(&mut self) {
        while let Ok((l, r)) = self.from.try_recv() {
            self.wet[0].extend(l);
            self.wet[1].extend(r);
        }
    }

    /// The model's answers for samples `from..from + n` of its stream (it starts `PIPE` behind
    /// what's played), when they're all here.
    fn wet_for(&mut self, n: usize) -> bool {
        let from = self.out_at.saturating_sub(PIPE as u64);
        // Answers for samples already played: let go.
        let old = from.saturating_sub(self.wet_at).min(self.wet[0].len() as u64) as usize;
        self.wet[0].drain(..old);
        self.wet[1].drain(..old);
        self.wet_at += old as u64;
        // The first `PIPE` samples have no answer (the model's stream starts later): silence.
        let head = (PIPE as u64).saturating_sub(self.out_at).min(n as u64) as usize;
        self.wet_at <= from + head as u64 && self.wet[0].len() + head >= n
    }
}

impl ferrocore::afx::SoundPlugin for Plugin {
    fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
        let [l, r] = ch;
        let n = l.len().min(r.len());
        if self.gave_up {
            return false;
        }
        // Every block goes to the model in order (its state follows the sound); a thread that's
        // gone: the light noise removal instead.
        if self.to.send(Ask::Block(l[..n].to_vec(), r[..n].to_vec())).is_err() {
            return false;
        }
        // The sound as it came, delayed as the model's is.
        let mut dry = [Vec::with_capacity(n), Vec::with_capacity(n)];
        for (c, x) in [&*l, &*r].into_iter().enumerate() {
            self.dry[c].extend(&x[..n]);
            dry[c].extend(self.dry[c].drain(..n));
        }
        let ready = self.ready.load(std::sync::atomic::Ordering::Acquire);
        if !ready && self.made.elapsed() > READY_WITHIN {
            tracing::warn!("the strong noise removal didn't get ready: the light one takes over");
            self.gave_up = true;
        }
        self.take();
        let mut wet = ready && self.wet_for(n);
        if ready && !wet {
            // Nearly there: a moment's wait, never more.
            let until = std::time::Instant::now() + GRACE;
            while !wet && std::time::Instant::now() < until {
                match self.from.recv_timeout(until.saturating_duration_since(std::time::Instant::now())) {
                    Ok((a, b)) => {
                        self.wet[0].extend(a);
                        self.wet[1].extend(b);
                        wet = self.wet_for(n);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        }
        // The model's answers for this block are here (`have`), and they're played (`wet`).
        let have = wet;
        // Judged before this block plays: one that gives up fades to the sound as it came here,
        // so the light noise removal after it starts from there (no step).
        if ready {
            self.judged += n;
            if !wet {
                self.dry_run += n;
            }
            if self.judged >= JUDGE_SECS * 48_000 {
                self.gave_up = self.dry_run as f32 > self.judged as f32 * MOST_DRY;
                if self.gave_up {
                    tracing::warn!(
                        dry_pct = self.dry_run * 100 / self.judged,
                        "the strong noise removal is too slow for this computer"
                    );
                    wet = false;
                }
                self.judged = 0;
                self.dry_run = 0;
            }
        }
        let head = (PIPE as u64).saturating_sub(self.out_at).min(n as u64) as usize;
        for (c, x) in [&mut *l, &mut *r].into_iter().enumerate() {
            // Where the last block ended, for a change with no answer to fade from.
            let step = self.last[c] - dry[c].first().copied().unwrap_or(0.0);
            for (i, v) in x[..n].iter_mut().enumerate() {
                // Before the model's stream starts (`head`): nothing of it.
                let w = if have && i >= head { self.wet[c].get(i - head).copied().unwrap_or(0.0) } else { 0.0 };
                let t = i as f32 / n as f32;
                *v = match (self.was_wet, wet, have) {
                    // A change between the two fades across the block: no step, no click.
                    (false, true, _) => dry[c][i] + (w - dry[c][i]) * t,
                    (true, false, true) => w + (dry[c][i] - w) * t,
                    // The answer late: the sound as it came, eased in from where the last block
                    // ended (fading from no answer at all dipped to silence; CodeRabbit).
                    (true, false, false) => dry[c][i] + step * (1.0 - (i as f32 / DECLICK as f32)).max(0.0),
                    (true, true, _) => w,
                    (false, false, _) => dry[c][i],
                };
            }
            if let Some(end) = x[..n].last() {
                self.last[c] = *end;
            }
        }
        self.out_at += n as u64;
        self.was_wet = wet;
        // Too slow for this computer: from the next block the light one takes over, going on
        // from what this one still holds (`hand_over`).
        true
    }

    fn latency_frames(&self) -> usize {
        MODEL_LATENCY + PIPE
    }

    fn hand_over(&mut self) -> Option<[Vec<f32>; 2]> {
        Some([self.dry[0].drain(..).collect(), self.dry[1].drain(..).collect()])
    }

    fn retune(&mut self, effect: &ferrocore::afx::AudioEffect) -> bool {
        let ferrocore::afx::AudioEffect::Denoise { strength } = *effect else { return false };
        if strength != self.strength {
            self.strength = strength;
            return self.to.send(Ask::Strength(strength.clamp(1, 100))).is_ok();
        }
        true
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

    fn db_to_mul(db: f32) -> f32 {
        10f32.powf(db / 20.0)
    }

    fn mul_to_db(m: f32) -> f32 {
        20.0 * m.max(1e-9).log10()
    }

    fn pink_noise(n: usize, target_rms: f32, seed: &mut u32) -> Vec<f32> {
        let (mut b0, mut b1, mut b2, mut b3, mut b4, mut b5, mut b6) = (0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let w = ((*seed >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0;
            b0 = 0.99886 * b0 + w * 0.0555179;
            b1 = 0.99332 * b1 + w * 0.0750759;
            b2 = 0.96900 * b2 + w * 0.1538520;
            b3 = 0.86650 * b3 + w * 0.3104856;
            b4 = 0.55000 * b4 + w * 0.5329522;
            b5 = -0.7616 * b5 - w * 0.0168980;
            let p = b0 + b1 + b2 + b3 + b4 + b5 + b6 + w * 0.5362;
            b6 = w * 0.115926;
            out.push(p);
        }
        let r = rms(&out).max(1e-9);
        for v in &mut out {
            *v *= target_rms / r;
        }
        out
    }

    fn bandpass_voice(x: &[f32]) -> Vec<f32> {
        // 2nd-order Butterworth HP 300 Hz + LP 3400 Hz at 48 kHz.
        let biquad = |inp: &[f32], b: [f64; 3], a: [f64; 2]| -> Vec<f32> {
            let (mut z0, mut z1) = (0.0f64, 0.0f64);
            inp.iter()
                .map(|&s| {
                    let s = f64::from(s);
                    let y = b[0] * s + z0;
                    z0 = b[1] * s - a[0] * y + z1;
                    z1 = b[2] * s - a[1] * y;
                    y as f32
                })
                .collect()
        };
        let hp = |fc: f64| -> ([f64; 3], [f64; 2]) {
            let k = (std::f64::consts::PI * fc / 48_000.0).tan();
            let q = std::f64::consts::FRAC_1_SQRT_2;
            let a0 = 1.0 + k / q + k * k;
            ([1.0 / a0, -2.0 / a0, 1.0 / a0], [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0])
        };
        let lp = |fc: f64| -> ([f64; 3], [f64; 2]) {
            let k = (std::f64::consts::PI * fc / 48_000.0).tan();
            let q = std::f64::consts::FRAC_1_SQRT_2;
            let a0 = 1.0 + k / q + k * k;
            ([k * k / a0, 2.0 * k * k / a0, k * k / a0], [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0])
        };
        let (hb, ha) = hp(300.0);
        let (lb, la) = lp(3400.0);
        biquad(&biquad(x, hb, ha), lb, la)
    }

    /// Objective speech-clarity test: synthetic voiced harmonic formant speech + pink noise
    /// across strengths. Verifies:
    /// 1) voice-band energy (300–3400 Hz) stays preserved at every strength;
    /// 2) background pink noise reduction increases smoothly and monotonically with strength;
    /// 3) no attenuation cliff between strength 99 and 100 (capped smoothly at 30 dB);
    /// 4) no level pumping across continuous speech.
    #[test]
    fn speech_clarity_and_noise_reduction_across_strengths() {
        let r = 48_000usize;
        let make_articulated = |n: usize, target_rms: f32| -> Vec<f32> {
            let mut out = Vec::with_capacity(n);
            let mut phase = 0.0f64;
            let rate = 48_000.0f64;
            for i in 0..n {
                let t = i as f64 / rate;
                let f0 = 140.0
                    + 25.0 * (std::f64::consts::TAU * 0.8 * t).sin()
                    + 10.0 * (std::f64::consts::TAU * 2.1 * t).cos();
                phase += std::f64::consts::TAU * f0 / rate;
                let f1 = 520.0 + 220.0 * (std::f64::consts::TAU * 1.9 * t).sin();
                let f2 = 1550.0 + 550.0 * (std::f64::consts::TAU * 1.4 * t).cos();
                let f3 = 2600.0 + 300.0 * (std::f64::consts::TAU * 1.1 * t).sin();
                let mut s = 0.0f64;
                for k in 1..=26 {
                    let fk = f0 * f64::from(k);
                    let g1 = 1.0 / ((fk - f1).powi(2) + 55.0f64.powi(2)).sqrt();
                    let g2 = 0.7 / ((fk - f2).powi(2) + 80.0f64.powi(2)).sqrt();
                    let g3 = 0.35 / ((fk - f3).powi(2) + 110.0f64.powi(2)).sqrt();
                    let env = (g1 + g2 + g3) / f64::from(k).powf(0.5);
                    let phi = std::f64::consts::PI * f64::from(k * k) / 26.0;
                    s += (phase * f64::from(k) + phi).sin() * env;
                }
                let cyc = (t * 3.2) % 1.0;
                let env_t = if cyc < 0.78 { (std::f64::consts::PI * cyc / 0.78).sin().powf(0.6) } else { 0.0 };
                out.push((s * env_t) as f32);
            }
            let cur = rms(&out).max(1e-9);
            for v in &mut out {
                *v *= target_rms / cur;
            }
            out
        };

        let mut seed = 42u32;
        let total = 5 * r; // 1 s pink noise, 2.5 s speech+noise, 1.5 s pink noise
        let noise = pink_noise(total, db_to_mul(-36.0), &mut seed);
        let speech = make_articulated(2 * r + r / 2, db_to_mul(-28.0));
        let mut input = noise.clone();
        for i in 0..speech.len() {
            input[r + i] += speech[i];
        }
        let sp_ref_bp = bandpass_voice(&speech[r / 4..speech.len() - r / 4]);
        let sp_ref_rms = rms(&sp_ref_bp);

        let mut prev_pause_nr = 0.0f32;
        let mut pause_at_99 = 0.0f32;
        let mut pause_at_100 = 0.0f32;
        for strength in [20u32, 40, 65, 80, 99, 100] {
            let mut d = Denoiser::new(1, strength).expect("model starts");
            let lat = d.latency();
            let mut padded = input.clone();
            padded.resize(total + lat + 1024, 0.0);
            let mut out = Vec::with_capacity(padded.len());
            for chunk in padded.chunks(1024) {
                let mut buf = chunk.to_vec();
                d.run(&mut [&mut buf]);
                out.extend_from_slice(&buf);
            }
            let aligned = &out[lat..lat + total];
            let sp_out_bp = bandpass_voice(&aligned[r + r / 4..r + speech.len() - r / 4]);
            let voice_kept_db = mul_to_db(rms(&sp_out_bp) / sp_ref_rms);
            let pause_nr_db = mul_to_db(rms(&aligned[4 * r..5 * r]) / rms(&noise[4 * r..5 * r]));
            // Stability across 0.5 s windows during speech (no pumping).
            let mut w_min = f32::INFINITY;
            let mut w_max = f32::NEG_INFINITY;
            for w in 0..4 {
                let a = r + r / 4 + w * (r / 2);
                let b = a + r / 2;
                let g = mul_to_db(rms(&bandpass_voice(&aligned[a..b])) / rms(&bandpass_voice(&speech[a - r..b - r])));
                w_min = w_min.min(g);
                w_max = w_max.max(g);
            }
            let spread = w_max - w_min;
            assert!(voice_kept_db > -1.5, "s={strength}: voice band dropped {voice_kept_db:+.2} dB");
            assert!(spread < 1.5, "s={strength}: speech pumped {spread:.2} dB across windows");
            assert!(
                pause_nr_db < prev_pause_nr,
                "s={strength}: pause_nr {pause_nr_db:+.2} dB should tighten past {prev_pause_nr:+.2} dB"
            );
            if strength == 99 {
                pause_at_99 = pause_nr_db;
            } else if strength == 100 {
                pause_at_100 = pause_nr_db;
            }
            prev_pause_nr = pause_nr_db;
        }
        assert!(
            (pause_at_100 - pause_at_99).abs() < 2.0,
            "no 99→100 cliff: s=99 gave {pause_at_99:+.2} dB vs s=100 {pause_at_100:+.2} dB"
        );
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
        assert_eq!(said, MODEL_LATENCY, "40 ms with the built-in model (the plugin says it before the model is made)");
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
        assert_eq!(atten_db(100), 30.0, "capped smoothly at 30 dB, never unlimited 100 dB");
        assert_eq!(atten_db(99), 29.7);
        assert!((atten_db(100) - atten_db(99)).abs() < 1.0, "no 99→100 attenuation cliff");
        assert_eq!(atten_db(80), 24.0);
        assert_eq!(atten_db(65), 19.5);
        assert_eq!(atten_db(50), 15.0);
        assert_eq!(atten_db(0), 0.3);
        for s in 2..=100 {
            assert!(atten_db(s) > atten_db(s - 1), "strictly monotonic at {s}");
        }
    }

    use ferrocore::afx::{AudioEffect, AudioFilter};

    /// Hiss in `n`-frame blocks at the pace sound comes (the model starts on its own thread
    /// meanwhile): (energy in, energy out) over the last half, and each block's time in the
    /// mixer (ms).
    fn through(f: &mut AudioFilter, blocks: usize, n: usize) -> (f64, f64, Vec<f64>) {
        let mut seed = 1u32;
        let (mut before, mut after, mut took) = (0.0f64, 0.0f64, Vec::new());
        for block in 0..blocks {
            let mut l: Vec<f32> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1 << 24) as f32 * 0.2 - 0.1
                })
                .collect();
            let mut r = l.clone();
            let energy: f64 = l.iter().map(|v| f64::from(*v).powi(2)).sum();
            let t = std::time::Instant::now();
            f.process([&mut l, &mut r]);
            took.push(t.elapsed().as_secs_f64() * 1000.0);
            if block >= blocks / 2 {
                before += energy;
                after += l.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
            }
            std::thread::sleep(std::time::Duration::from_secs_f64(n as f64 / 48_000.0));
        }
        (before, after, took)
    }

    #[test]
    fn our_engine_takes_the_noise_out() {
        super::install();
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength: 100 }, 48_000);
        // Two seconds of steady hiss in 10 ms blocks; the second half measured.
        let (before, after, _) = through(&mut f, 200, 480);
        assert!(after < before * 0.1, "hiss mostly gone: {after:.3} of {before:.3}");
    }

    /// The mixer never waits for the model (it held the frame loop up to 100 ms a block when the
    /// computer was busy, 18 Nov): from the very first block, while it loads and once it runs,
    /// each block is back at once, and the delay the mixer files the sound by never changes.
    #[test]
    fn the_mixer_never_waits_for_it() {
        super::install();
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength: 100 }, 48_000);
        let said = f.latency_frames();
        assert_eq!(said, MODEL_LATENCY + PIPE, "known before the model is made");
        let (before, after, took) = through(&mut f, 140, 1024);
        assert_eq!(f.latency_frames(), said, "the same delay once it runs");
        let worst = took.iter().copied().fold(0.0, f64::max);
        assert!(worst < 6.0, "no block waited: worst {worst:.2} ms");
        assert!(after < before * 0.1, "and it cleans: {after:.3} of {before:.3}");
    }

    /// Blocks come in twos, back to back, as the frame loop mixes them at 30 fps: the model's
    /// answers are still there in time (one block behind wasn't: a third went dry).
    #[test]
    fn blocks_in_pairs_stay_cleaned() {
        super::install();
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength: 100 }, 48_000);
        // Ready first.
        let _ = through(&mut f, 30, 1024);
        let mut seed = 5u32;
        let (mut before, mut after) = (0.0f64, 0.0f64);
        for _ in 0..60 {
            for _ in 0..2 {
                let mut l: Vec<f32> = (0..1024)
                    .map(|_| {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (seed >> 8) as f32 / (1 << 24) as f32 * 0.2 - 0.1
                    })
                    .collect();
                let mut r = l.clone();
                before += l.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
                f.process([&mut l, &mut r]);
                after += l.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
            }
            std::thread::sleep(std::time::Duration::from_millis(42));
        }
        assert!(after < before * 0.1, "cleaned all along: {after:.3} of {before:.3}");
    }

    /// A strength change goes to the running model (no new model, no gap), and what comes out
    /// is still in step: a click goes in, and comes out exactly the delay said later.
    #[test]
    fn a_new_strength_keeps_it_running_and_in_step() {
        super::install();
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength: 100 }, 48_000);
        let said = f.latency_frames();
        let _ = through(&mut f, 40, 1024);
        assert!(f.retune(AudioEffect::Denoise { strength: 60 }), "taken by the running one");
        // A buzz with a sharp start, after silence: where it comes out.
        let mut outl = Vec::new();
        let mut at = 0usize;
        for block in 0..60 {
            let mut l: Vec<f32> = (0..1024)
                .map(|i| {
                    let k = block * 1024 + i;
                    if k < 20_000 {
                        0.0
                    } else {
                        let t = k as f32 / 48_000.0;
                        (1..12).map(|h| (std::f32::consts::TAU * 140.0 * h as f32 * t).sin() / h as f32).sum::<f32>()
                            * 0.2
                    }
                })
                .collect();
            let mut r = l.clone();
            f.process([&mut l, &mut r]);
            outl.extend(l);
            at += 1024;
            std::thread::sleep(std::time::Duration::from_millis(21));
        }
        assert_eq!(at, outl.len());
        // Skip the pipeline tail of the preceding hiss before locating the buzz onset at k = 20_000.
        let first =
            outl.iter().enumerate().skip(said + 1024).find(|(_, v)| v.abs() > 0.02).map_or(usize::MAX, |(i, _)| i);
        assert!(first >= 20_000 + said - 64 && first <= 20_000 + said + 480, "came out at {first}, said {said}");
    }
}
