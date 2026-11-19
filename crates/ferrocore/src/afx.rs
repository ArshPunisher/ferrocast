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

    /// How late its sound comes out (frames at the mixer's rate): the mixer files the source's
    /// sound that much earlier, so a voice cleaned by it stays in step with the picture.
    fn latency_frames(&self) -> usize {
        0
    }

    /// Takes new settings for the same filter (a strength) and keeps running: true. False: it
    /// can't, and a new one is made.
    fn retune(&mut self, _effect: &AudioEffect) -> bool {
        false
    }

    /// When it gives up: the sound it took in and hasn't given back yet (left, right; as long
    /// as its delay), so what plays after it goes on from there with nothing lost.
    fn hand_over(&mut self) -> Option<[Vec<f32>; 2]> {
        None
    }
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

/// A VST plugin on a source's sound: which one, and its saved state (opaque here: the VST
/// helper's component and controller chunks). Kept by (source, filter, bundle, class): a new
/// state for the same one doesn't start it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSpec {
    pub source: String,
    pub filter: String,
    /// The `.vst3` bundle.
    pub path: String,
    /// Its class id, 32 hex digits.
    pub cid: String,
    pub state: Vec<u8>,
}

static PLUGINS: std::sync::Mutex<Vec<PluginSpec>> = std::sync::Mutex::new(Vec::new());

/// The slot for `spec` (the same one as before for the same source, filter and plugin; its
/// state brought up to date).
pub fn plugin_slot(spec: PluginSpec) -> u32 {
    let mut l = PLUGINS.lock().unwrap_or_else(|e| e.into_inner());
    let same =
        |p: &PluginSpec| p.source == spec.source && p.filter == spec.filter && p.path == spec.path && p.cid == spec.cid;
    if let Some(i) = l.iter().position(same) {
        l[i].state = spec.state;
        return i as u32;
    }
    l.push(spec);
    (l.len() - 1) as u32
}

/// What slot `slot` holds.
pub fn plugin_spec(slot: u32) -> Option<PluginSpec> {
    PLUGINS.lock().unwrap_or_else(|e| e.into_inner()).get(slot as usize).cloned()
}

/// Every plugin slot in use, with its spec.
pub fn plugin_specs() -> Vec<(u32, PluginSpec)> {
    PLUGINS.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().enumerate().map(|(i, p)| (i as u32, p)).collect()
}

/// The newest state of a slot's plugin (its window changed it), kept for the next start.
pub fn set_plugin_state(slot: u32, state: Vec<u8>) {
    if let Some(p) = PLUGINS.lock().unwrap_or_else(|e| e.into_inner()).get_mut(slot as usize) {
        p.state = state;
    }
}

/// Makes the plugin in a slot at a rate (the app's VST host), or None when it can't.
pub type MakePlugin = fn(u32, u32) -> Option<Box<dyn SoundPlugin>>;

static PLUGIN_MAKER: std::sync::OnceLock<MakePlugin> = std::sync::OnceLock::new();

/// The app's VST host (set once).
pub fn set_plugin_maker(make: MakePlugin) {
    let _ = PLUGIN_MAKER.set(make);
}

/// Whether this app runs VST plugins.
pub fn plugins_available() -> bool {
    PLUGIN_MAKER.get().is_some()
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
    /// A VST plugin (`ferrocast_vst`), in the slot `plugin_slot` gave it.
    Plugin {
        slot: u32,
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
    /// True once `plugin` has processed at least one block (failed init leaves this false so
    /// we never install a permanent handover delay line around `light_denoise`).
    plugin_ran: bool,
    // Light noise removal: the noise floor it hears (RMS), steady-noise candidate, and current gain.
    floor: f32,
    nr_steady_rms: f32,
    nr_steady_s: f32,
    nr_gain: f32,
    /// After a plugin that held the sound back gave way (the strong noise removal too slow
    /// here): the sound is held back as long still, so the source doesn't jump in time.
    delay: Option<[std::collections::VecDeque<f32>; 2]>,
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
                AudioEffect::Plugin { slot } => PLUGIN_MAKER.get().and_then(|make| make(slot, rate)),
                _ => None,
            },
            plugin_ran: false,
            floor: 0.0,
            nr_steady_rms: 0.0,
            nr_steady_s: 0.0,
            nr_gain: 1.0,
            delay: None,
        }
    }

    /// Takes `effect`'s settings when it's the same kind of filter, keeping what it remembers
    /// (a gate stays open, a compressor's envelope, the EQ's filters, the noise removal's model):
    /// a slider dragged on a filter used to make it anew every step, a click or a pump each time.
    /// False: another kind (or another VST, or a plugin that can't take them): make a new one.
    pub fn retune(&mut self, effect: AudioEffect) -> bool {
        if std::mem::discriminant(&self.effect) != std::mem::discriminant(&effect) {
            return false;
        }
        if let (AudioEffect::Plugin { slot: a }, AudioEffect::Plugin { slot: b }) = (self.effect, effect)
            && a != b
        {
            return false;
        }
        if self.effect == effect {
            return true;
        }
        if let Some(p) = self.plugin.as_mut()
            && !p.retune(&effect)
        {
            return false;
        }
        self.effect = effect;
        true
    }

    /// A filter that is just `plugin` (tests: a plugin of their own, not the app's).
    #[cfg(test)]
    pub(crate) fn with_plugin(plugin: Box<dyn SoundPlugin>, rate: u32) -> Self {
        let mut f = Self::new(AudioEffect::Gain { db: 0.0 }, rate);
        f.plugin = Some(plugin);
        f
    }

    /// How late this filter's sound comes out, in frames (a plugin's; the engine's own add none).
    pub fn latency_frames(&self) -> usize {
        match (&self.plugin, &self.delay) {
            (Some(p), _) => p.latency_frames(),
            (None, Some(h)) => h[0].len(),
            (None, None) => 0,
        }
    }

    /// Filters a block in place (both sides the same length).
    pub fn process(&mut self, ch: [&mut [f32]; 2]) {
        let [l, r] = ch;
        let n = l.len().min(r.len());
        let rate = self.rate;
        if let Some(p) = self.plugin.as_mut() {
            if p.process([&mut *l, &mut *r]) {
                self.plugin_ran = true;
                return;
            }
            // A VST that was late or stopped: this block passes as it was; it starts itself again.
            if matches!(self.effect, AudioEffect::Plugin { .. }) {
                return;
            }
            tracing::warn!("the strong noise removal fell behind or stopped: the light one takes over");
            let mut gone = self.plugin.take();
            // Only keep a handover delay line when the plugin actually ran and delayed audio;
            // a failed init never delayed the stream and must not leave an 82.7 ms delay behind.
            let late = if self.plugin_ran { gone.as_ref().map_or(0, |p| p.latency_frames()) } else { 0 };
            if late > 0 {
                // What it still held plays first (silence only for what it never had).
                let kept = gone.as_mut().and_then(|p| p.hand_over());
                let line = |c: usize| {
                    let mut q: std::collections::VecDeque<f32> =
                        kept.as_ref().map(|k| k[c].iter().copied().collect()).unwrap_or_default();
                    while q.len() < late {
                        q.push_front(0.0);
                    }
                    while q.len() > late {
                        q.pop_front();
                    }
                    q
                };
                self.delay = Some([line(0), line(1)]);
            }
        }
        self.run_own(l, r, n, rate);
        if let Some(h) = self.delay.as_mut() {
            for (c, x) in [&mut *l, &mut *r].into_iter().enumerate() {
                for v in x[..n].iter_mut() {
                    h[c].push_back(*v);
                    *v = h[c].pop_front().unwrap_or(0.0);
                }
            }
        }
    }

    /// The engine's own filters (and the light noise removal).
    fn run_own(&mut self, l: &mut [f32], r: &mut [f32], n: usize, rate: f32) {
        match self.effect {
            // No VST host in this app, or not running yet: the sound passes as it is.
            AudioEffect::Plugin { .. } => {}
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
                // Closing at or above where it opens never closes (the level never falls: the
                // gate stuck open, as in OBS): the More form allows it, so it closes at least
                // 3 dB under where it opens (how fast it falls follows that gap).
                let close = db_to_mul(close_db.min(open_db - 3.0));
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

    /// Light noise removal, for any computer: it tracks the background room noise floor (frozen
    /// during speech so continuous talking is never ducked) and turns down what stays near the
    /// floor (hiss, hum, a fan between words) by up to 24 dB at strength 100; a voice above it
    /// passes untouched.
    fn light_denoise(&mut self, l: &mut [f32], r: &mut [f32], strength: u32) {
        let n = l.len();
        if n == 0 {
            return;
        }
        let rms = (l.iter().chain(r.iter()).map(|v| v * v).sum::<f32>() / (2 * n) as f32).sqrt();
        let secs = n as f32 / self.rate;
        // Digital silence (a gate shut before it, a muted device) says nothing about the room:
        // it isn't learnt, and the floor starts at a quiet room's (-60 dBFS), so a voice coming
        // after silence isn't taken for the noise (peer, 18 Nov).
        const SILENCE: f32 = 3.2e-5; // -90 dBFS
        const START: f32 = 1e-3; // -60 dBFS
        // Hard ceiling on background floor (-36 dBFS): continuous speech above this never pulls
        // the floor upward and never gets ducked mid-sentence (M2-01).
        const MAX_FLOOR: f32 = 0.015_848_932; // -36 dBFS
        const STEADY_HOLD_S: f32 = 0.25;
        if self.floor <= 0.0 {
            self.floor = START;
        }
        if rms >= SILENCE {
            if rms < self.floor {
                self.floor = rms;
                self.nr_steady_rms = rms;
                self.nr_steady_s = 0.0;
            } else if rms <= MAX_FLOOR {
                let over_floor = mul_to_db(rms / self.floor.max(1e-9));
                let steady_diff =
                    if self.nr_steady_rms > 0.0 { mul_to_db(rms / self.nr_steady_rms).abs() } else { f32::INFINITY };
                if steady_diff <= 4.5 {
                    self.nr_steady_s = (self.nr_steady_s + secs).min(STEADY_HOLD_S + 1.0);
                    self.nr_steady_rms += (rms - self.nr_steady_rms) * (secs * 3.0).min(1.0);
                } else {
                    self.nr_steady_rms = rms;
                    self.nr_steady_s = 0.0;
                }
                if over_floor <= 6.0 {
                    self.floor = (self.floor * db_to_mul(3.0 * secs)).min(rms);
                } else if self.nr_steady_s >= STEADY_HOLD_S {
                    // Steady room noise above the initial -60 dBFS guess (or a fan turning on).
                    self.floor = (self.floor * db_to_mul(18.0 * secs)).min(rms).min(MAX_FLOOR);
                }
            } else {
                // Active speech above background noise ceiling: freeze the floor completely.
                self.nr_steady_rms = 0.0;
                self.nr_steady_s = 0.0;
            }
        }
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

    /// A gate's threshold dragged while someone talks: the gate stays open (it was made anew
    /// each step, so it shut and faded back in 25 ms: the mic chopped, 18 Nov).
    #[test]
    fn a_retuned_gate_stays_open() {
        let gate = |open_db: f32| AudioEffect::NoiseGate {
            open_db,
            close_db: open_db - 6.0,
            attack_ms: 25.0,
            hold_ms: 200.0,
            release_ms: 150.0,
        };
        let mut f = AudioFilter::new(gate(-26.0), RATE);
        let talk = |len: usize| -> Vec<f32> { (0..len).map(|i| (i as f32 * 0.05).sin() * 0.5).collect() };
        for _ in 0..20 {
            let (mut l, mut r) = (talk(1024), talk(1024));
            f.process([&mut l, &mut r]);
        }
        assert!(f.retune(gate(-30.0)), "the same kind takes the new settings");
        let (mut l, mut r) = (talk(1024), talk(1024));
        let input = l.clone();
        f.process([&mut l, &mut r]);
        assert!((rms(&l) - rms(&input)).abs() < 1e-4, "still open: no dip, no fade in");
        // Another kind: not taken.
        assert!(!f.retune(AudioEffect::Gain { db: 0.0 }));
    }

    /// A plugin that fails immediately on block 1 (failed model init) never delayed the stream
    /// and must NOT leave a permanent handover delay line around `light_denoise` (M2-02).
    #[test]
    fn failed_init_leaves_zero_delay() {
        #[derive(Debug)]
        struct FailsAtInit(usize);
        impl SoundPlugin for FailsAtInit {
            fn process(&mut self, _: [&mut [f32]; 2]) -> bool {
                false
            }
            fn latency_frames(&self) -> usize {
                self.0
            }
        }
        let mut f = AudioFilter::with_plugin(Box::new(FailsAtInit(3968)), RATE);
        f.effect = AudioEffect::Denoise { strength: 65 };
        let (mut l, mut r) = (vec![0.5f32; 1024], vec![0.5f32; 1024]);
        f.process([&mut l, &mut r]);
        assert_eq!(f.latency_frames(), 0, "failed init must not leave an 82.7 ms delay");
        assert!(rms(&l) > 0.4, "audio passes through light_denoise immediately, not silenced");
    }

    /// A plugin that actually ran and held the sound back, and then gives up mid-stream: the
    /// light noise removal takes over with the same delay, so the source doesn't jump in time.
    #[test]
    fn giving_up_keeps_the_delay() {
        #[derive(Debug)]
        struct RunsOnceThenQuits(usize, bool);
        impl SoundPlugin for RunsOnceThenQuits {
            fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
                if self.1 {
                    return false;
                }
                self.1 = true;
                ch[0].fill(0.0);
                ch[1].fill(0.0);
                true
            }
            fn latency_frames(&self) -> usize {
                self.0
            }
        }
        let mut f = AudioFilter::with_plugin(Box::new(RunsOnceThenQuits(2944, false)), RATE);
        f.effect = AudioEffect::Denoise { strength: 100 };
        assert_eq!(f.latency_frames(), 2944);
        let (mut l, mut r) = (vec![0.5f32; 1024], vec![0.5f32; 1024]);
        f.process([&mut l, &mut r]);
        let (mut l, mut r) = (vec![0.5f32; 1024], vec![0.5f32; 1024]);
        f.process([&mut l, &mut r]);
        assert_eq!(f.latency_frames(), 2944, "the same delay after it gave way mid-stream");
        assert!(l.iter().all(|v| *v == 0.0), "held back: the handover delay stays aligned");
    }

    /// A plugin that holds the sound back and gives up mid-stream hands over what it still
    /// held: the sound goes on exactly where it was, nothing lost or played twice (the peer's
    /// S1: 61 ms dropped and 61 ms of silence).
    #[test]
    fn giving_up_mid_stream_loses_nothing() {
        #[derive(Debug)]
        struct QuitsAfter(usize, [std::collections::VecDeque<f32>; 2]);
        impl SoundPlugin for QuitsAfter {
            fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
                if self.0 == 0 {
                    return false;
                }
                self.0 -= 1;
                for (c, x) in ch.into_iter().enumerate() {
                    for v in x.iter_mut() {
                        self.1[c].push_back(*v);
                        *v = self.1[c].pop_front().unwrap_or(0.0);
                    }
                }
                true
            }
            fn latency_frames(&self) -> usize {
                self.1[0].len()
            }
            fn hand_over(&mut self) -> Option<[Vec<f32>; 2]> {
                Some([self.1[0].drain(..).collect(), self.1[1].drain(..).collect()])
            }
        }
        let late = 2944;
        let zeros = || std::iter::repeat_n(0.0f32, late).collect::<std::collections::VecDeque<f32>>();
        let mut f = AudioFilter::with_plugin(Box::new(QuitsAfter(5, [zeros(), zeros()])), RATE);
        // As the light noise removal at its lightest, so the count passes nearly as it is.
        f.effect = AudioEffect::Gain { db: 0.0 };
        let mut out = Vec::new();
        for b in 0..12 {
            let mut l: Vec<f32> = (0..1024).map(|i| (b * 1024 + i) as f32).collect();
            let mut r = l.clone();
            f.process([&mut l, &mut r]);
            out.extend(l);
        }
        for (i, v) in out.iter().enumerate().skip(late) {
            assert_eq!(*v, (i - late) as f32, "sample {i}: nothing lost, nothing twice");
        }
    }

    /// A voice after digital silence (a gate before it, a muted mic) passes as it is: it was
    /// taken for the noise floor and turned down 12-24 dB.
    #[test]
    fn a_voice_after_silence_is_not_the_noise() {
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength: 100 }, RATE);
        f.plugin = None;
        for _ in 0..47 {
            let (mut l, mut r) = (vec![0.0f32; 1024], vec![0.0f32; 1024]);
            f.process([&mut l, &mut r]);
        }
        let voice: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.07).sin() * 0.2).collect();
        let mut last = 0.0;
        for _ in 0..20 {
            let (mut l, mut r) = (voice.clone(), voice.clone());
            f.process([&mut l, &mut r]);
            last = rms(&l);
        }
        assert!(last > rms(&voice) * 0.9, "the voice passes: {last} of {}", rms(&voice));
    }

    /// A gate set to close above where it opens still closes.
    #[test]
    fn a_gate_set_backwards_still_closes() {
        let gate = AudioEffect::NoiseGate {
            open_db: -40.0,
            close_db: -20.0,
            attack_ms: 25.0,
            hold_ms: 10.0,
            release_ms: 50.0,
        };
        let mut f = AudioFilter::new(gate, RATE);
        let (mut l, mut r) = (vec![0.5f32; 4800], vec![0.5f32; 4800]);
        f.process([&mut l, &mut r]);
        let mut out = Vec::new();
        for _ in 0..150 {
            let (mut l, mut r) = (vec![0.001f32; 1024], vec![0.001f32; 1024]);
            f.process([&mut l, &mut r]);
            out = l;
        }
        assert!(out.iter().all(|v| v.abs() < 1e-4), "shut on quiet sound");
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

    fn voiced_speech(n: usize, target_rms: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(n);
        let mut phase = 0.0f64;
        let rate = f64::from(RATE);
        for i in 0..n {
            let t = i as f64 / rate;
            let f0 = 135.0 + 12.0 * (std::f64::consts::TAU * 0.65 * t).sin();
            phase += std::f64::consts::TAU * f0 / rate;
            let f1 = 550.0 + 150.0 * (std::f64::consts::TAU * 0.9 * t).sin();
            let f2 = 1650.0 + 350.0 * (std::f64::consts::TAU * 0.7 * t).cos();
            let f3 = 2650.0 + 200.0 * (std::f64::consts::TAU * 1.1 * t).sin();
            let mut s = 0.0f64;
            for k in 1..=24 {
                let fk = f0 * f64::from(k);
                let g1 = 1.0 / ((fk - f1).powi(2) + 45.0f64.powi(2)).sqrt();
                let g2 = 0.6 / ((fk - f2).powi(2) + 65.0f64.powi(2)).sqrt();
                let g3 = 0.3 / ((fk - f3).powi(2) + 90.0f64.powi(2)).sqrt();
                let env = (g1 + g2 + g3) / f64::from(k).powf(0.5);
                s += (phase * f64::from(k)).sin() * env;
            }
            let syll = 0.65 + 0.35 * (0.5 + 0.5 * (std::f64::consts::TAU * 3.5 * t).sin());
            out.push((s * syll) as f32);
        }
        let r = rms(&out).max(1e-9);
        for v in &mut out {
            *v *= target_rms / r;
        }
        out
    }

    /// 15 s of continuous voiced formant speech over -45 dBFS pink noise through `light_denoise`:
    /// voice energy stays flat (no floor creep, no pumping) while pauses between words are
    /// attenuated monotonically with strength (M2-01).
    #[test]
    fn light_denoise_keeps_continuous_speech_steady_without_pumping() {
        let r = RATE as usize;
        let mut seed = 42u32;
        let noise_rms = db_to_mul(-45.0);
        let speech_rms = db_to_mul(-18.0);
        let noise = pink_noise(18 * r, noise_rms, &mut seed);
        let speech = voiced_speech(15 * r, speech_rms);
        let mut input = noise.clone();
        for i in 0..(15 * r) {
            input[r + i] += speech[i];
        }
        let mut prev_pause_nr = 0.0f32;
        for strength in [25u32, 50, 75, 100] {
            let mut f = AudioFilter::new(AudioEffect::Denoise { strength }, RATE);
            f.plugin = None;
            let (mut l, mut rr) = (input.clone(), input.clone());
            for (a, b) in l.chunks_mut(1024).zip(rr.chunks_mut(1024)) {
                f.process([a, b]);
            }
            let early = mul_to_db(rms(&l[2 * r..5 * r]) / rms(&input[2 * r..5 * r]));
            let late = mul_to_db(rms(&l[12 * r..15 * r]) / rms(&input[12 * r..15 * r]));
            let pause_after = mul_to_db(rms(&l[17 * r..18 * r]) / rms(&noise[17 * r..18 * r]));
            let mut w_min = f32::INFINITY;
            let mut w_max = f32::NEG_INFINITY;
            for sec in 2..15 {
                let g = mul_to_db(rms(&l[sec * r..(sec + 1) * r]) / rms(&input[sec * r..(sec + 1) * r]));
                w_min = w_min.min(g);
                w_max = w_max.max(g);
            }
            let drift = late - early;
            let spread = w_max - w_min;
            assert!(
                drift.abs() < 0.5,
                "strength={strength}: speech drifted {drift:+.2} dB (early {early:+.2}, late {late:+.2})"
            );
            assert!(spread < 1.0, "strength={strength}: speech pumped {spread:.2} dB across 13 s");
            assert!(early > -0.5, "strength={strength}: speech attenuated {early:+.2} dB");
            assert!(
                pause_after < prev_pause_nr - 3.0,
                "strength={strength}: pause_nr {pause_after:+.2} dB should strengthen past {prev_pause_nr:+.2} dB"
            );
            prev_pause_nr = pause_after;
        }
        assert!(prev_pause_nr < -20.0, "strength=100 pause noise reduced by >20 dB: {prev_pause_nr:+.2} dB");
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
