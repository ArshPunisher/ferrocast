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

/// Flushes tiny exponential-decay tails well before they enter IEEE 754 `f32` subnormal range
/// (`< 1.175e-38`), preventing x86 denormal microcode stalls in silent audio filters (M2-06).
#[inline]
fn flush_denormal(x: f32) -> f32 {
    if x.abs() < 1e-20 { 0.0 } else { x }
}

/// Enables Flush-To-Zero (FTZ) and Denormals-Are-Zero (DAZ) in MXCSR on x86/x86_64 audio
/// threads so residual subnormals in filters or third-party DSP never trigger x86 microcode
/// traps (M2-06). Safe and idempotent; no-op on non-x86 targets.
#[allow(unsafe_code)]
pub fn enable_ftz_daz() {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        #[cfg(target_arch = "x86")]
        let has_sse = std::arch::is_x86_feature_detected!("sse");
        #[cfg(target_arch = "x86_64")]
        let has_sse = true;
        if has_sse {
            const FTZ: u32 = 0x8000;
            const DAZ: u32 = 0x0040;
            let mut bits = FTZ;
            if std::arch::is_x86_feature_detected!("sse3") {
                bits |= DAZ;
            }
            let mut mxcsr: u32 = 0;
            // SAFETY: SSE (MXCSR) is present; DAZ is gated on SSE3; `stmxcsr`/`ldmxcsr` only
            // read/write this thread's floating-point control register and a local `u32`.
            unsafe {
                core::arch::asm!(
                    "stmxcsr [{ptr}]",
                    ptr = in(reg) &raw mut mxcsr,
                    options(nostack, preserves_flags),
                );
                mxcsr |= bits;
                core::arch::asm!(
                    "ldmxcsr [{ptr}]",
                    ptr = in(reg) &raw const mxcsr,
                    options(nostack, preserves_flags),
                );
            }
        }
    }
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
static PLUGINS_DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Enables or suppresses third-party VST plugin creation (Safe Mode sets `true`).
pub fn set_plugins_disabled(disabled: bool) {
    PLUGINS_DISABLED.store(disabled, std::sync::atomic::Ordering::Release);
}

/// Whether third-party VST plugins are suppressed (Safe Mode).
pub fn plugins_disabled() -> bool {
    PLUGINS_DISABLED.load(std::sync::atomic::Ordering::Acquire)
}

/// The app's VST host (set once).
pub fn set_plugin_maker(make: MakePlugin) {
    let _ = PLUGIN_MAKER.set(make);
}

/// Whether this app runs VST plugins.
pub fn plugins_available() -> bool {
    !plugins_disabled() && PLUGIN_MAKER.get().is_some()
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

/// Click-free linear parameter smoother (~8 ms ramp on `retune()`, exact at target on `new()`).
/// Zero heap allocation, exact convergence (snaps to `target`), denormal-free.
#[derive(Debug, Clone, Copy, Default)]
struct SmoothRamp {
    cur: f32,
    target: f32,
    step: f32,
    rem: u32,
}

/// Parameter smoothing and enable/disable crossfade duration (8 ms).
const SMOOTH_S: f32 = 0.008;

impl SmoothRamp {
    fn at(val: f32) -> Self {
        Self { cur: val, target: val, step: 0.0, rem: 0 }
    }

    fn set_target(&mut self, val: f32, samples: u32) {
        if val.to_bits() == self.target.to_bits() {
            return;
        }
        if !val.is_finite() || !self.cur.is_finite() || samples == 0 {
            self.cur = val;
            self.target = val;
            self.step = 0.0;
            self.rem = 0;
            return;
        }
        self.target = val;
        self.step = flush_denormal((val - self.cur) / samples as f32);
        if self.step == 0.0 {
            self.cur = val;
            self.rem = 0;
        } else {
            self.rem = samples;
        }
    }

    #[inline]
    fn next(&mut self) -> f32 {
        if self.rem > 0 {
            self.rem -= 1;
            if self.rem == 0 {
                self.cur = self.target;
                self.step = 0.0;
            } else {
                self.cur = flush_denormal(self.cur + self.step);
            }
        }
        self.cur
    }

    #[cfg(test)]
    fn is_subnormal(&self) -> bool {
        self.cur.is_subnormal() || self.target.is_subnormal() || self.step.is_subnormal()
    }
}

/// A filter running: its settings and what it remembers.
#[derive(Debug)]
pub struct AudioFilter {
    pub effect: AudioEffect,
    rate: f32,
    /// Click-free enable/disable state: `bypass_mix` ramps dry↔wet over [`SMOOTH_S`].
    enabled: bool,
    bypass_mix: SmoothRamp,
    // Gate.
    is_open: bool,
    attenuation: f32,
    level: f32,
    held: f32,
    // Compressor, limiter: stereo-linked peak envelope; expander/upward: per-channel RMS, linked gain.
    envelope: f32,
    runave: [f32; 2],
    gain_db: [f32; 2],
    // Smoothed linear gains / thresholds / slopes to eliminate zipper noise on live slider drags.
    smooth_gain: SmoothRamp,
    smooth_out: SmoothRamp,
    smooth_thresh: SmoothRamp,
    smooth_slope: SmoothRamp,
    smooth_eq: [SmoothRamp; 3],
    eq: [EqState; 2],
    /// A filter made outside the engine (noise removal).
    plugin: Option<Box<dyn SoundPlugin>>,
    /// True once `plugin` has processed at least one block (failed init leaves this false so
    /// we never install a permanent handover delay line around `light_denoise`).
    plugin_ran: bool,
    // Light noise removal: the noise floor it hears (RMS), how long and at what level the sound
    // has been steady, the voice detector's history, and the current gain.
    floor: f32,
    nr_steady_rms: f32,
    nr_steady_s: f32,
    nr_gain: f32,
    /// The last 40 ms as the voice detector reads it (8 kHz, mono, pre-emphasised), in an inline
    /// ring buffer (zero heap allocation in `process()`), and the decimation accumulator.
    nr_hist: [f32; VOICE_WINDOW],
    nr_hist_pos: usize,
    nr_hist_len: usize,
    nr_dec: (f32, u32, f32),
    /// Seconds left of "a voice is speaking": a voice's blocks (and the breaths between its
    /// syllables) are never turned down, and never taken for the noise.
    nr_voice_s: f32,
    /// The detector's pitch (in samples at 8 kHz) and for how long it has held the same one: a
    /// hum does for seconds, a voice never.
    nr_pitch: (usize, f32),
    /// The level the pitch has held at (RMS, smoothed): a hum doesn't move, a voice does.
    nr_pitch_rms: f32,
    /// After a plugin that held the sound back gave way (the strong noise removal too slow
    /// here): the sound is held back as long still, so the source doesn't jump in time.
    delay: Option<[std::collections::VecDeque<f32>; 2]>,
    /// True once a plugin inside this filter failed or gave up, so non-blocking filter edits
    /// don't re-request plugin construction for a filter that intentionally fell back.
    plugin_gave_up: bool,
    /// Identifier of an in-flight off-thread plugin build awaiting attachment.
    pub(crate) pending_build: Option<u64>,
}

/// Samples (at 8 kHz) the voice detector looks at: 40 ms.
const VOICE_WINDOW: usize = 320;

/// libobs's compressor `gain_coefficient` (guards non-positive/non-finite times so negative
/// imported parameters never produce `exp(+inf)` / `NaN`, M2-07).
fn coefficient(rate: f32, secs: f32) -> f32 {
    if !rate.is_finite() || rate <= 0.0 || !secs.is_finite() || secs <= 0.0 {
        return 0.0;
    }
    (-1.0 / (rate * secs)).exp()
}

fn comp_targets(effect: AudioEffect) -> (f32, f32, f32) {
    match effect {
        AudioEffect::Compressor { ratio, threshold_db, output_db, .. } => {
            let ratio = if ratio.is_finite() { ratio.max(1.0) } else { 1.0 };
            let thresh = if threshold_db.is_finite() { threshold_db } else { -18.0 };
            let out = if output_db.is_finite() { db_to_mul(output_db) } else { 1.0 };
            (thresh, 1.0 - 1.0 / ratio, out)
        }
        AudioEffect::Limiter { threshold_db, .. } => {
            let thresh = if threshold_db.is_finite() { threshold_db } else { -6.0 };
            (thresh, 1.0, 1.0)
        }
        AudioEffect::Expander { ratio, threshold_db, output_db, .. } => {
            let ratio = if ratio.is_finite() { ratio.max(1.0) } else { 1.0 };
            let thresh = if threshold_db.is_finite() { threshold_db } else { -40.0 };
            let out = if output_db.is_finite() { db_to_mul(output_db) } else { 1.0 };
            (thresh, 1.0 - ratio, out)
        }
        AudioEffect::UpwardCompressor { ratio, threshold_db, output_db, .. } => {
            let ratio = if ratio.is_finite() { ratio.clamp(0.0, 1.0) } else { 0.5 };
            let thresh = if threshold_db.is_finite() { threshold_db } else { -20.0 };
            let out = if output_db.is_finite() { db_to_mul(output_db) } else { 1.0 };
            (thresh, 1.0 - ratio, out)
        }
        _ => (-18.0, 0.0, 1.0),
    }
}

fn eq_targets(effect: AudioEffect) -> [f32; 3] {
    match effect {
        AudioEffect::Eq3 { low_db, mid_db, high_db } => [
            if low_db.is_finite() { db_to_mul(low_db) } else { 1.0 },
            if mid_db.is_finite() { db_to_mul(mid_db) } else { 1.0 },
            if high_db.is_finite() { db_to_mul(high_db) } else { 1.0 },
        ],
        _ => [1.0; 3],
    }
}

fn gain_target(effect: AudioEffect) -> f32 {
    match effect {
        AudioEffect::Gain { db } => db_to_mul(db),
        _ => 1.0,
    }
}

impl AudioFilter {
    /// Builds the filter without constructing any external [`SoundPlugin`] (`Denoise` model or
    /// VST host), so real-time threads can update filter chains without blocking.
    pub fn new_without_plugin(effect: AudioEffect, rate: u32) -> Self {
        let (thresh, slope, out) = comp_targets(effect);
        let [lg, mg, hg] = eq_targets(effect);
        Self {
            effect,
            rate: (rate as f32).max(1.0),
            enabled: true,
            bypass_mix: SmoothRamp::at(1.0),
            is_open: false,
            attenuation: 0.0,
            level: 0.0,
            held: 0.0,
            envelope: 0.0,
            runave: [0.0; 2],
            gain_db: [0.0; 2],
            smooth_gain: SmoothRamp::at(gain_target(effect)),
            smooth_out: SmoothRamp::at(out),
            smooth_thresh: SmoothRamp::at(thresh),
            smooth_slope: SmoothRamp::at(slope),
            smooth_eq: [SmoothRamp::at(lg), SmoothRamp::at(mg), SmoothRamp::at(hg)],
            eq: [EqState::default(); 2],
            plugin: None,
            plugin_ran: false,
            floor: 0.0,
            nr_steady_rms: 0.0,
            nr_steady_s: 0.0,
            nr_gain: 1.0,
            nr_hist: [0.0; VOICE_WINDOW],
            nr_hist_pos: 0,
            nr_hist_len: 0,
            nr_dec: (0.0, 0, 0.0),
            nr_voice_s: 0.0,
            nr_pitch: (0, 0.0),
            nr_pitch_rms: 0.0,
            delay: None,
            plugin_gave_up: false,
            pending_build: None,
        }
    }

    fn smooth_samples(&self) -> u32 {
        (SMOOTH_S * self.rate).round().clamp(1.0, 4800.0) as u32
    }

    fn sync_smooth_targets(&mut self) {
        let samples = self.smooth_samples();
        match self.effect {
            AudioEffect::Gain { .. } => {
                self.smooth_gain.set_target(gain_target(self.effect), samples);
            }
            AudioEffect::Compressor { .. }
            | AudioEffect::Limiter { .. }
            | AudioEffect::Expander { .. }
            | AudioEffect::UpwardCompressor { .. } => {
                let (thresh, slope, out) = comp_targets(self.effect);
                self.smooth_thresh.set_target(thresh, samples);
                self.smooth_slope.set_target(slope, samples);
                self.smooth_out.set_target(out, samples);
            }
            AudioEffect::Eq3 { .. } => {
                let [lg, mg, hg] = eq_targets(self.effect);
                self.smooth_eq[0].set_target(lg, samples);
                self.smooth_eq[1].set_target(mg, samples);
                self.smooth_eq[2].set_target(hg, samples);
            }
            _ => {}
        }
    }

    /// Enables or bypasses this filter with a smooth 8 ms dry↔wet crossfade (zero clicks).
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.bypass_mix.set_target(if enabled { 1.0 } else { 0.0 }, self.smooth_samples());
        }
    }

    /// Whether this filter is currently enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether `effect` at `rate` expects an external [`SoundPlugin`] to be built.
    #[must_use]
    pub fn effect_needs_plugin(effect: AudioEffect, rate: u32) -> bool {
        match effect {
            AudioEffect::Denoise { .. } => rate == 48_000 && denoiser_available(),
            AudioEffect::Plugin { .. } => plugins_available(),
            _ => false,
        }
    }

    /// Builds an external [`SoundPlugin`] for `effect` at `rate` (may block; run off RT threads).
    #[must_use]
    pub fn build_plugin(effect: AudioEffect, rate: u32) -> Option<Box<dyn SoundPlugin>> {
        match effect {
            // Its model works at 48 kHz only; elsewhere the sound passes as it is.
            AudioEffect::Denoise { strength } if rate == 48_000 => {
                DENOISER.get().and_then(|make| make(strength.clamp(1, 100)))
            }
            AudioEffect::Plugin { slot } => {
                (!plugins_disabled()).then(|| PLUGIN_MAKER.get().and_then(|make| make(slot, rate))).flatten()
            }
            _ => None,
        }
    }

    /// Installs a plugin constructed off-thread into this filter.
    pub fn install_plugin(&mut self, plugin: Box<dyn SoundPlugin>) {
        self.plugin = Some(plugin);
        self.plugin_ran = false;
        self.plugin_gave_up = false;
        self.pending_build = None;
    }

    pub fn new(effect: AudioEffect, rate: u32) -> Self {
        let mut f = Self::new_without_plugin(effect, rate);
        f.plugin = Self::build_plugin(effect, rate);
        f
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
        self.sync_smooth_targets();
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
        self.process_inner(ch, None);
    }

    /// Filters `ch` in place while keying a `Compressor` (or `Limiter`) detector off an external
    /// stereo sidechain `key` (OBS `compressor-filter.c:analyze_sidechain`), zero allocations.
    pub fn process_sidechain(&mut self, ch: [&mut [f32]; 2], key: [&[f32]; 2]) {
        self.process_inner(ch, Some(key));
    }

    fn process_inner(&mut self, ch: [&mut [f32]; 2], key: Option<[&[f32]; 2]>) {
        let [l, r] = ch;
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        if !self.enabled && self.bypass_mix.rem == 0 && self.bypass_mix.cur <= 0.0 {
            return;
        }
        if self.bypass_mix.rem == 0 && self.bypass_mix.cur >= 1.0 {
            self.process_active(l, r, n, key);
            return;
        }
        const XFADE_CHUNK: usize = 256;
        let mut dry_l = [0.0f32; XFADE_CHUNK];
        let mut dry_r = [0.0f32; XFADE_CHUNK];
        let mut pos = 0;
        while pos < n {
            if !self.enabled && self.bypass_mix.rem == 0 && self.bypass_mix.cur <= 0.0 {
                break;
            }
            if self.bypass_mix.rem == 0 && self.bypass_mix.cur >= 1.0 {
                let rem_len = n - pos;
                let sub_key = key.map(|[kl, kr]| {
                    let sl = if pos < kl.len() { &kl[pos..n.min(kl.len())] } else { &[] };
                    let sr = if pos < kr.len() { &kr[pos..n.min(kr.len())] } else { &[] };
                    [sl, sr]
                });
                self.process_active(&mut l[pos..n], &mut r[pos..n], rem_len, sub_key);
                break;
            }
            let len = (n - pos).min(XFADE_CHUNK);
            dry_l[..len].copy_from_slice(&l[pos..pos + len]);
            dry_r[..len].copy_from_slice(&r[pos..pos + len]);
            let sub_key = key.map(|[kl, kr]| {
                let sl = if pos < kl.len() { &kl[pos..(pos + len).min(kl.len())] } else { &[] };
                let sr = if pos < kr.len() { &kr[pos..(pos + len).min(kr.len())] } else { &[] };
                [sl, sr]
            });
            self.process_active(&mut l[pos..pos + len], &mut r[pos..pos + len], len, sub_key);
            for i in 0..len {
                let m = self.bypass_mix.next();
                if m <= 0.0 {
                    l[pos + i] = dry_l[i];
                    r[pos + i] = dry_r[i];
                } else if m < 1.0 {
                    l[pos + i] = dry_l[i] + (l[pos + i] - dry_l[i]) * m;
                    r[pos + i] = dry_r[i] + (r[pos + i] - dry_r[i]) * m;
                }
            }
            pos += len;
        }
    }

    fn process_active(&mut self, l: &mut [f32], r: &mut [f32], n: usize, key: Option<[&[f32]; 2]>) {
        let rate = self.rate;
        if let Some(p) = self.plugin.as_mut() {
            if p.process([&mut l[..n], &mut r[..n]]) {
                self.plugin_ran = true;
                return;
            }
            // A VST that was late or stopped: this block passes as it was; it starts itself again.
            if matches!(self.effect, AudioEffect::Plugin { .. }) {
                return;
            }
            tracing::warn!("the strong noise removal fell behind or stopped: the light one takes over");
            self.plugin_gave_up = true;
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
        self.run_own(l, r, n, rate, key);
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
    fn run_own(&mut self, l: &mut [f32], r: &mut [f32], n: usize, rate: f32, key: Option<[&[f32]; 2]>) {
        self.sync_smooth_targets();
        match self.effect {
            // No VST host in this app, or not running yet: the sound passes as it is.
            AudioEffect::Plugin { .. } => {}
            AudioEffect::Gain { .. } => {
                for i in 0..n {
                    let m = self.smooth_gain.next();
                    l[i] *= m;
                    r[i] *= m;
                }
            }
            AudioEffect::InvertPolarity => {
                for i in 0..n {
                    l[i] = -l[i];
                    r[i] = -r[i];
                }
            }
            AudioEffect::NoiseGate { open_db, close_db, attack_ms, hold_ms, release_ms } => {
                let open_db = if open_db.is_finite() { open_db } else { -26.0 };
                let close_db = if close_db.is_finite() { close_db } else { -32.0 };
                let open = db_to_mul(open_db);
                // Preserve exact OBS hysteresis whenever `close < open` (at least 0.01 dB apart so
                // f32 `open - close` never underflows `decay_rate` to 0). When set equal or
                // backwards (`close >= open - 0.01`, where OBS's gate stalls open), enforce a 3 dB
                // floor under `open_db` so the gate still closes reliably.
                let close_eff = if close_db < open_db - 0.01 { close_db } else { open_db - 3.0 };
                let close = db_to_mul(close_eff);
                let attack_rate =
                    if attack_ms.is_finite() && attack_ms > 0.0 { 1.0 / (attack_ms / 1000.0 * rate) } else { 1.0 };
                let release_rate =
                    if release_ms.is_finite() && release_ms > 0.0 { 1.0 / (release_ms / 1000.0 * rate) } else { 1.0 };
                let decay_rate = (open - close) / ((1.0 / 75.0) * rate);
                let hold = if hold_ms.is_finite() { hold_ms.max(0.0) / 1000.0 } else { 0.0 };
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
                    self.level = (self.level.max(cur) - decay_rate).max(0.0);
                    if self.is_open {
                        self.attenuation = (self.attenuation + attack_rate).min(1.0);
                    } else {
                        self.held = (self.held + rate_i).min(hold + 1.0);
                        if self.held > hold {
                            self.attenuation = (self.attenuation - release_rate).max(0.0);
                        }
                    }
                    l[i] *= self.attenuation;
                    r[i] *= self.attenuation;
                }
                self.level = flush_denormal(self.level);
                self.attenuation = flush_denormal(self.attenuation);
            }
            AudioEffect::Compressor { attack_ms, release_ms, .. } => {
                self.compress(l, r, n, attack_ms, release_ms, key);
            }
            AudioEffect::Limiter { release_ms, .. } => {
                self.compress(l, r, n, 0.001, release_ms, key);
            }
            AudioEffect::Expander { attack_ms, release_ms, rms, .. } => {
                self.expand(l, r, n, false, attack_ms, release_ms, 0.0, rms);
            }
            AudioEffect::UpwardCompressor { attack_ms, release_ms, knee_db, .. } => {
                let knee_db = if knee_db.is_finite() { knee_db.max(0.0) } else { 0.0 };
                self.expand(l, r, n, true, attack_ms, release_ms, knee_db, true);
            }
            AudioEffect::Eq3 { .. } => {
                let lf = 2.0 * (std::f32::consts::PI * 800.0_f32.min(rate * 0.45) / rate).sin();
                let hf = 2.0 * (std::f32::consts::PI * 5000.0_f32.min(rate * 0.45) / rate).sin();
                const EPS: f32 = 1.0 / 4_294_967_295.0;
                for i in 0..n {
                    let lg = self.smooth_eq[0].next();
                    let mg = self.smooth_eq[1].next();
                    let hg = self.smooth_eq[2].next();
                    for (c, sample_ref) in [(0usize, &mut l[i]), (1usize, &mut r[i])] {
                        let st = &mut self.eq[c];
                        let sample = *sample_ref;
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
                        *sample_ref = lo * lg + mid * mg + hi * hg;
                    }
                }
                for st in &mut self.eq {
                    for v in st.lf.iter_mut().chain(st.hf.iter_mut()).chain(st.s.iter_mut()) {
                        *v = flush_denormal(*v);
                    }
                }
            }
            // Without the app's strong noise removal (not in this build, it didn't start, or this
            // computer is too slow for it): the light one.
            AudioEffect::Denoise { strength } => self.light_denoise(&mut l[..n], &mut r[..n], strength),
        }
    }

    /// Whether the block has a voice in it: a pitch (80 to 330 Hz) that repeats, by how alike the
    /// last 40 ms is to itself one period later. Noise and a fan don't repeat; a vowel does.
    /// Read at 8 kHz, mono, with the lows taken down so a lowpassed hiss doesn't look alike. A
    /// pitch that stays the same, at the same level, for two seconds is a hum (100 or 120 Hz of
    /// mains), not a voice, whose pitch and level move.
    fn hears_a_voice(&mut self, l: &[f32], r: &[f32], rms: f32) -> bool {
        let secs = l.len() as f32 / self.rate;
        let found = self.pitch_of(l, r);
        let Some((lag, strength)) = found else {
            self.nr_pitch = (0, 0.0);
            return false;
        };
        let level_moved = self.nr_pitch_rms <= 0.0 || mul_to_db(rms / self.nr_pitch_rms).abs() > 1.0;
        if strength > 0.6 && lag.abs_diff(self.nr_pitch.0) <= 1 && !level_moved {
            self.nr_pitch.1 = (self.nr_pitch.1 + secs).min(4.0);
        } else {
            self.nr_pitch.1 = 0.0;
        }
        self.nr_pitch.0 = lag;
        self.nr_pitch_rms = if self.nr_pitch_rms <= 0.0 || level_moved {
            rms
        } else {
            self.nr_pitch_rms + (rms - self.nr_pitch_rms) * (secs * 3.0).min(1.0)
        };
        self.nr_pitch.1 < 2.0
    }

    /// The repeating pitch in the last 40 ms, if there is one: its period (samples at 8 kHz) and
    /// how alike it repeats (0 to 1). Zero heap allocation (inline ring + stack buffer).
    fn pitch_of(&mut self, l: &[f32], r: &[f32]) -> Option<(usize, f32)> {
        let every = ((self.rate / 8000.0).round() as u32).max(1);
        let (mut acc, mut count, mut prev) = self.nr_dec;
        for (a, b) in l.iter().zip(r) {
            acc += 0.5 * (a + b);
            count += 1;
            if count == every {
                let x = acc / every as f32;
                self.nr_hist[self.nr_hist_pos] = x - 0.9 * prev;
                self.nr_hist_pos = (self.nr_hist_pos + 1) % VOICE_WINDOW;
                if self.nr_hist_len < VOICE_WINDOW {
                    self.nr_hist_len += 1;
                }
                prev = x;
                acc = 0.0;
                count = 0;
            }
        }
        self.nr_dec = (acc, count, prev);
        if self.nr_hist_len < VOICE_WINDOW {
            return None;
        }
        let mut x = [0.0f32; VOICE_WINDOW];
        let split = VOICE_WINDOW - self.nr_hist_pos;
        x[..split].copy_from_slice(&self.nr_hist[self.nr_hist_pos..]);
        x[split..].copy_from_slice(&self.nr_hist[..self.nr_hist_pos]);
        let mut alike = [0.0f32; 97];
        // 3 to 12 ms: 330 down to 83 Hz at 8 kHz.
        for (lag, slot) in alike.iter_mut().enumerate().skip(24) {
            let (mut num, mut e1, mut e2) = (0.0f32, 0.0f32, 0.0f32);
            for i in 0..VOICE_WINDOW - lag {
                num += x[i] * x[i + lag];
                e1 += x[i] * x[i];
                e2 += x[i + lag] * x[i + lag];
            }
            let denom = (e1 * e2).sqrt();
            if denom > 1e-12 {
                *slot = num / denom;
            }
        }
        let best = alike.iter().copied().fold(0.0f32, f32::max);
        // The first period that repeats nearly as well as the best (not its doubles).
        let lag = (24..=96).find(|l| alike[*l] >= 0.95 * best)?;
        (best > 0.5).then_some((lag, best))
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
        // The loudest room noise it will take for the room (-30 dBFS).
        const MAX_FLOOR: f32 = 0.031_622_776;
        // How long a sound must hold still (within 6 dB of itself) before it is learnt as the
        // room: a fan does; a voice never holds that still, and its voiced blocks don't count.
        const STEADY_HOLD_S: f32 = 0.5;
        // A voice keeps the gain open this long after it was last heard (a consonant, the
        // breath between two words).
        const VOICE_HOLD_S: f32 = 0.2;
        if self.floor <= 0.0 {
            self.floor = START;
        }
        let voiced = rms >= SILENCE && self.hears_a_voice(l, r, rms);
        if voiced {
            self.nr_voice_s = VOICE_HOLD_S;
        } else {
            self.nr_voice_s = (self.nr_voice_s - secs).max(0.0);
        }
        let voice_now = self.nr_voice_s > 0.0;
        if rms >= SILENCE {
            if rms < self.floor {
                // Down at once to anything quieter: that is the room.
                self.floor = rms;
                self.nr_steady_rms = rms;
                self.nr_steady_s = 0.0;
            } else if voice_now {
                // A voice is speaking: the floor stays where it was (never rises to meet it).
                self.nr_steady_rms = 0.0;
                self.nr_steady_s = 0.0;
            } else if rms <= MAX_FLOOR {
                // Not a voice: noise or a room. Steady means within 6 dB of what it has been (the level of
                // a few milliseconds of noise wanders by that much).
                let steady_diff =
                    if self.nr_steady_rms > 0.0 { mul_to_db(rms / self.nr_steady_rms).abs() } else { f32::INFINITY };
                if steady_diff <= 6.0 {
                    self.nr_steady_s = (self.nr_steady_s + secs).min(STEADY_HOLD_S + 1.0);
                    self.nr_steady_rms += (rms - self.nr_steady_rms) * (secs * 4.0).min(1.0);
                } else {
                    self.nr_steady_rms = rms;
                    self.nr_steady_s = 0.0;
                }
                if self.nr_steady_s >= STEADY_HOLD_S {
                    // A fan turned on, or the room got louder: learnt within a few seconds.
                    self.floor = (self.floor * db_to_mul(20.0 * secs)).min(self.nr_steady_rms).max(self.floor);
                } else if mul_to_db(rms / self.floor.max(1e-9)) <= 6.0 {
                    // Slowly rising room noise.
                    self.floor = (self.floor * db_to_mul(3.0 * secs)).min(rms);
                }
            } else {
                self.nr_steady_rms = 0.0;
                self.nr_steady_s = 0.0;
            }
        }
        let depth = 24.0 * strength.clamp(1, 100) as f32 / 100.0;
        // Within 6 dB of the floor is noise; 12 dB over is surely not: in between, partly. A
        // voice is never turned down, whatever its level or the room's.
        let over = mul_to_db(rms.max(1e-9) / self.floor.max(1e-9));
        let target = if voice_now { 1.0 } else { db_to_mul(-depth * (1.0 - ((over - 6.0) / 6.0).clamp(0.0, 1.0))) };
        // Opens in 5 ms (a word's start isn't clipped), closes in 120 ms.
        let open = 1.0 - coefficient(self.rate, 0.005);
        let close = 1.0 - coefficient(self.rate, 0.120);
        for i in 0..n {
            let k = if target > self.nr_gain { open } else { close };
            self.nr_gain += (target - self.nr_gain) * k;
            l[i] *= self.nr_gain;
            r[i] *= self.nr_gain;
        }
        self.nr_gain = flush_denormal(self.nr_gain);
    }

    /// libobs's compressor and limiter: stereo-linked peak envelope (`max(|L|, |R|)`) with
    /// optional external sidechain key (`compressor-filter.c:analyze_sidechain`), then gain.
    /// Block-size invariant, zero heap allocation, smoothed parameters, denormal-free (M2-06).
    fn compress(
        &mut self,
        l: &mut [f32],
        r: &mut [f32],
        n: usize,
        attack_ms: f32,
        release_ms: f32,
        key: Option<[&[f32]; 2]>,
    ) {
        if n == 0 {
            return;
        }
        let ag = coefficient(self.rate, attack_ms / 1000.0);
        let rg = coefficient(self.rate, release_ms / 1000.0);
        let mut env = self.envelope;
        for i in 0..n {
            let x = match key {
                Some([kl, kr]) => kl.get(i).copied().unwrap_or(0.0).abs().max(kr.get(i).copied().unwrap_or(0.0).abs()),
                None => l[i].abs().max(r[i].abs()),
            };
            env = flush_denormal(if env < x { x + ag * (env - x) } else { x + rg * (env - x) });
            let threshold = self.smooth_thresh.next();
            let slope = self.smooth_slope.next();
            let out = self.smooth_out.next();
            let gain = if env > 0.0 { db_to_mul((slope * (threshold - mul_to_db(env))).min(0.0)) } else { 1.0 };
            let g = gain * out;
            l[i] *= g;
            r[i] *= g;
        }
        self.envelope = flush_denormal(env);
    }

    /// libobs's expander, gate preset and upward compressor: RMS (10 ms) or peak detection,
    /// stereo-linked (`max(env_l, env_r)` so L/R receive identical gain and stereo image never
    /// shifts — bit-identical to OBS on mono/equal L=R), gain in dB with attack and release.
    /// Fused zero-allocation loop with smoothed output gain and denormal-free state (M2-06).
    #[allow(clippy::too_many_arguments)]
    fn expand(
        &mut self,
        l: &mut [f32],
        r: &mut [f32],
        n: usize,
        upward: bool,
        attack_ms: f32,
        release_ms: f32,
        knee: f32,
        rms: bool,
    ) {
        if n == 0 {
            return;
        }
        let ag = coefficient(self.rate, attack_ms / 1000.0);
        let rg = coefficient(self.rate, release_ms / 1000.0);
        let rmscoef = (-100.0 / self.rate).exp2();
        let half_knee = knee * 0.5;
        let mut run_l = self.runave[0];
        let mut run_r = self.runave[1];
        let mut prev = self.gain_db[0];
        for i in 0..n {
            let sl = l[i];
            let sr = r[i];
            let threshold = self.smooth_thresh.next();
            let slope = self.smooth_slope.next();
            let env = if rms {
                run_l = rmscoef * run_l + (1.0 - rmscoef) * sl * sl;
                if run_l < 1e-25 {
                    run_l = 0.0;
                }
                run_r = rmscoef * run_r + (1.0 - rmscoef) * sr * sr;
                if run_r < 1e-25 {
                    run_r = 0.0;
                }
                run_l.max(run_r).max(0.0).sqrt()
            } else {
                run_l = flush_denormal(sl * sl);
                run_r = flush_denormal(sr * sr);
                sl.abs().max(sr.abs())
            };
            let env_db = mul_to_db(env);
            let mut diff = threshold - env_db;
            if upward && env_db <= (threshold - 60.0) / 2.0 {
                diff = if env_db + 60.0 > 0.0 { env_db + 60.0 } else { 0.0 };
            }
            let p = if upward { prev.max(0.0) } else { prev };
            let gain = if upward {
                if env_db >= threshold + half_knee {
                    0.0
                } else if knee <= 1e-6 || threshold - half_knee >= env_db {
                    slope * diff
                } else {
                    slope * (diff + half_knee).powi(2) / (2.0 * knee)
                }
            } else if diff > 0.0 {
                (slope * diff).max(-60.0)
            } else {
                0.0
            };
            let g = flush_denormal(if gain > p { ag * p + (1.0 - ag) * gain } else { rg * p + (1.0 - rg) * gain });
            prev = g;
            let m = if upward { db_to_mul(g) } else { db_to_mul(g.min(0.0)) };
            let out = self.smooth_out.next();
            let total = m * out;
            l[i] = sl * total;
            r[i] = sr * total;
        }
        self.runave[0] = if run_l < 1e-25 { 0.0 } else { run_l };
        self.runave[1] = if run_r < 1e-25 { 0.0 } else { run_r };
        let clean = flush_denormal(prev);
        self.gain_db = [clean, clean];
    }

    #[cfg(test)]
    fn has_subnormal(&self) -> bool {
        let eq_sub = self.eq.iter().any(|st| st.lf.iter().chain(&st.hf).chain(&st.s).any(|v| v.is_subnormal()));
        let ramp_sub = self.bypass_mix.is_subnormal()
            || self.smooth_gain.is_subnormal()
            || self.smooth_out.is_subnormal()
            || self.smooth_thresh.is_subnormal()
            || self.smooth_slope.is_subnormal()
            || self.smooth_eq.iter().any(SmoothRamp::is_subnormal);
        let nr_sub = self.nr_hist.iter().any(|v| v.is_subnormal())
            || self.nr_dec.0.is_subnormal()
            || self.nr_dec.2.is_subnormal()
            || self.nr_pitch_rms.is_subnormal();
        self.attenuation.is_subnormal()
            || self.level.is_subnormal()
            || self.envelope.is_subnormal()
            || self.runave.iter().any(|v| v.is_subnormal())
            || self.gain_db.iter().any(|v| v.is_subnormal())
            || self.floor.is_subnormal()
            || self.nr_steady_rms.is_subnormal()
            || self.nr_gain.is_subnormal()
            || eq_sub
            || ramp_sub
            || nr_sub
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
        // (Noise that doesn't repeat: the first version of this used a hiss with a 20 ms period,
        // which a voice detector rightly takes for a voice.)
        let mut seed = 21u32;
        let hiss = pink_noise(RATE as usize, db_to_mul(-50.0), &mut seed);
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
            b2 = 0.969_00 * b2 + w * 0.153_852;
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
        let noise = pink_noise(21 * r, noise_rms, &mut seed);
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
            let pause_after = mul_to_db(rms(&l[20 * r..21 * r]) / rms(&noise[20 * r..21 * r]));
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

    /// Harmonic "speech": a pitch (with vibrato), formants, syllables at 3.5 Hz. `f0` is the
    /// voice's pitch; `deep` is how far the syllables dip (0.35: a little, 0.8: words nearly apart).
    fn speech_of(n: usize, target_rms: f32, f0_base: f64, deep: f64) -> Vec<f32> {
        let mut out = Vec::with_capacity(n);
        let mut phase = 0.0f64;
        let rate = f64::from(RATE);
        for i in 0..n {
            let t = i as f64 / rate;
            let f0 = f0_base + 0.09 * f0_base * (std::f64::consts::TAU * 0.65 * t).sin();
            phase += std::f64::consts::TAU * f0 / rate;
            let f1 = 550.0 + 150.0 * (std::f64::consts::TAU * 0.9 * t).sin();
            let f2 = 1650.0 + 350.0 * (std::f64::consts::TAU * 0.7 * t).cos();
            let mut s = 0.0f64;
            for k in 1..=24 {
                let fk = f0 * f64::from(k);
                let g1 = 1.0 / ((fk - f1).powi(2) + 45.0f64.powi(2)).sqrt();
                let g2 = 0.6 / ((fk - f2).powi(2) + 65.0f64.powi(2)).sqrt();
                s += (phase * f64::from(k)).sin() * (g1 + g2) / f64::from(k).powf(0.5);
            }
            let syll = 1.0 - deep * (0.5 - 0.5 * (std::f64::consts::TAU * 3.5 * t).cos());
            out.push((s * syll) as f32);
        }
        let r = rms(&out).max(1e-9);
        for v in &mut out {
            *v *= target_rms / r;
        }
        out
    }

    /// A mains hum (`hz` is 50 or 60, or a rectified 100 or 120) with its harmonics, at `target_rms`.
    fn hum(n: usize, hz: f32, target_rms: f32) -> Vec<f32> {
        let mut out: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                (1..=6).map(|h| (std::f32::consts::TAU * hz * h as f32 * t).sin() / h as f32).sum::<f32>()
            })
            .collect();
        let r = rms(&out).max(1e-9);
        for v in &mut out {
            *v *= target_rms / r;
        }
        out
    }

    fn through_light(input: &[f32], strength: u32) -> Vec<f32> {
        let mut f = AudioFilter::new(AudioEffect::Denoise { strength }, RATE);
        f.plugin = None;
        let (mut l, mut rr) = (input.to_vec(), input.to_vec());
        for (a, b) in l.chunks_mut(1024).zip(rr.chunks_mut(1024)) {
            f.process([a, b]);
        }
        l
    }

    /// M2-01 (S16): speech is never ducked, at any level, over any room noise it is above; the
    /// noise between words still goes down. Speech at -50 to -12 dBFS over pink noise at -60 and
    /// -45 dBFS, a deep and a shallow voice, a syllable pattern with deep and shallow dips, at strength
    /// 100 and 50: the speech stays within 1 dB, the noise after it falls (at least 10 dB at 100,
    /// 5 dB at 50). (The first version of this filter ducked a -40 dBFS voice by 20 dB in a
    /// second, and the "frozen floor" fix only held above -36 dBFS.)
    #[test]
    fn light_denoise_never_ducks_speech_at_any_level_and_still_quiets_the_noise() {
        let r = RATE as usize;
        let mut worst: Vec<String> = Vec::new();
        for noise_db in [-60.0f32, -45.0] {
            for speech_db in [-50.0f32, -44.0, -40.0, -30.0, -20.0, -12.0] {
                if speech_db < noise_db - 1.0 {
                    continue;
                }
                for (f0, deep) in [(110.0, 0.35), (210.0, 0.35), (130.0, 0.7)] {
                    let mut seed = 7u32;
                    // 4 s of the room, 10 s of speech over it, 3 s of the room again.
                    let noise = pink_noise(17 * r, db_to_mul(noise_db), &mut seed);
                    let speech = speech_of(10 * r, db_to_mul(speech_db), f0, deep);
                    let mut input = noise.clone();
                    for i in 0..10 * r {
                        input[4 * r + i] += speech[i];
                    }
                    for strength in [100u32, 50] {
                        let out = through_light(&input, strength);
                        // The speech, from its second second to the end: never more than 1 dB
                        // down, in any second (no pumping), and the pause after it quieter.
                        let mut worst_sec = f32::INFINITY;
                        for sec in 5..14 {
                            let g = mul_to_db(rms(&out[sec * r..(sec + 1) * r]) / rms(&input[sec * r..(sec + 1) * r]));
                            worst_sec = worst_sec.min(g);
                        }
                        let pause = mul_to_db(rms(&out[16 * r..17 * r]) / rms(&noise[16 * r..17 * r]));
                        let need = if strength == 100 { -10.0 } else { -5.0 };
                        if worst_sec < -1.0 || pause > need {
                            worst.push(format!(
                                "speech {speech_db} over noise {noise_db}, f0 {f0}, deep {deep}, strength {strength}: speech {worst_sec:+.1} dB, pause {pause:+.1} dB"
                            ));
                        }
                    }
                }
            }
        }
        assert!(worst.is_empty(), "{} cases:\n{}", worst.len(), worst.join("\n"));
    }

    /// A fan (or a hum) that turns on in the middle of a stream is learnt in a few seconds, not
    /// by eating the voice over it: the voice keeps its level (1 dB) through the change and the
    /// noise between the words after it goes down by 10 dB.
    #[test]
    fn a_fan_turning_on_mid_stream_is_learnt_without_eating_speech() {
        let r = RATE as usize;
        for (name, make) in [
            (
                "a pink fan",
                Box::new(|n: usize, seed: &mut u32| pink_noise(n, db_to_mul(-45.0), seed))
                    as Box<dyn Fn(usize, &mut u32) -> Vec<f32>>,
            ),
            ("a 60 Hz hum", Box::new(|n: usize, _: &mut u32| hum(n, 60.0, db_to_mul(-45.0)))),
            ("a 100 Hz hum", Box::new(|n: usize, _: &mut u32| hum(n, 100.0, db_to_mul(-45.0)))),
            ("a 120 Hz hum", Box::new(|n: usize, _: &mut u32| hum(n, 120.0, db_to_mul(-45.0)))),
        ] {
            let mut seed = 11u32;
            // 4 s of a quiet room, then the noise for 20 s; speech at -30 dBFS from 2 s to 18 s.
            let room = pink_noise(28 * r, db_to_mul(-62.0), &mut seed);
            let fan = make(24 * r, &mut seed);
            let speech = speech_of(16 * r, db_to_mul(-40.0), 130.0, 0.7);
            let mut input = room.clone();
            for i in 0..24 * r {
                input[4 * r + i] += fan[i];
            }
            for i in 0..16 * r {
                input[2 * r + i] += speech[i];
            }
            let out = through_light(&input, 100);
            // The voice through the turn-on and after it (seconds 3..18 against what went in).
            for sec in 3..18 {
                let g = mul_to_db(rms(&out[sec * r..(sec + 1) * r]) / rms(&input[sec * r..(sec + 1) * r]));
                assert!(g > -1.0, "{name}: the voice was down {g:+.1} dB in second {sec}");
            }
            // After the speech ends (at 18 s): the noise is down by 10 dB from 24 s on (a hum's
            // never-changing pitch takes two seconds to be told from a voice, then it is learnt).
            let fan_only = &input[24 * r..28 * r];
            let cleaned = mul_to_db(rms(&out[24 * r..28 * r]) / rms(fan_only));
            assert!(cleaned < -10.0, "{name}: the noise after the speech is down only {cleaned:+.1} dB");
        }
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

    #[test]
    fn safe_mode_disables_plugin_filters_without_affecting_built_in_afx() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                set_plugins_disabled(false);
            }
        }
        let _guard = Reset;

        set_plugins_disabled(true);
        assert!(plugins_disabled());
        assert!(!plugins_available());

        // Safe Mode suppresses third-party SoundPlugin instances: AudioFilter passes audio cleanly.
        let mut f = AudioFilter::new(AudioEffect::Plugin { slot: 0 }, RATE);
        let (mut l, mut r) = (vec![0.25f32; 256], vec![-0.25f32; 256]);
        f.process([&mut l, &mut r]);
        assert!(l.iter().all(|v| (*v - 0.25).abs() < 1e-6));

        set_plugins_disabled(false);
        assert!(!plugins_disabled());
    }

    /// After an impulse followed by 10 s of digital silence, no filter leaves IEEE 754 subnormal
    /// floats in its internal state (which would stall x86 CPUs on silence, M2-06).
    #[test]
    fn filters_never_enter_subnormal_state_on_silence() {
        enable_ftz_daz();
        let effects = [
            AudioEffect::Gain { db: 6.0 },
            AudioEffect::InvertPolarity,
            AudioEffect::NoiseGate { open_db: -26.0, close_db: -32.0, attack_ms: 5.0, hold_ms: 10.0, release_ms: 10.0 },
            AudioEffect::Compressor {
                ratio: 10.0,
                threshold_db: -18.0,
                attack_ms: 1.0,
                release_ms: 5.0,
                output_db: 0.0,
            },
            AudioEffect::Limiter { threshold_db: -6.0, release_ms: 5.0 },
            AudioEffect::Expander {
                ratio: 4.0,
                threshold_db: -40.0,
                attack_ms: 2.0,
                release_ms: 5.0,
                output_db: 0.0,
                rms: true,
            },
            AudioEffect::Expander {
                ratio: 10.0,
                threshold_db: -40.0,
                attack_ms: 2.0,
                release_ms: 5.0,
                output_db: 0.0,
                rms: false,
            },
            AudioEffect::UpwardCompressor {
                ratio: 0.5,
                threshold_db: -20.0,
                attack_ms: 2.0,
                release_ms: 5.0,
                output_db: 0.0,
                knee_db: 10.0,
            },
            AudioEffect::Eq3 { low_db: 3.0, mid_db: -2.0, high_db: 4.0 },
            AudioEffect::Denoise { strength: 65 },
        ];
        for effect in effects {
            let mut f = AudioFilter::new(effect, RATE);
            f.plugin = None;
            // Impulse block.
            let mut l = vec![0.0f32; 1024];
            let mut r = vec![0.0f32; 1024];
            l[0] = 0.8;
            r[0] = -0.8;
            f.process([&mut l, &mut r]);
            // 10 seconds of silence (470 blocks of 1024 samples).
            for _ in 0..470 {
                l.fill(0.0);
                r.fill(0.0);
                f.process([&mut l, &mut r]);
                assert!(!f.has_subnormal(), "{effect:?} entered subnormal state during silence");
                assert!(l.iter().chain(&r).all(|v| v.is_finite() && !v.is_subnormal()), "{effect:?} emitted subnormal");
            }
        }
    }

    /// Malformed negative/zero/NaN filter settings (e.g. from hand-edited OBS JSON) never cause
    /// `exp(+inf)` or `0/0` NaN poisoning in any filter (M2-07).
    #[test]
    fn malformed_filter_params_produce_finite_output() {
        let bad = [
            AudioEffect::Compressor {
                ratio: -4.0,
                threshold_db: -18.0,
                attack_ms: -5.0,
                release_ms: 0.0,
                output_db: 0.0,
            },
            AudioEffect::Limiter { threshold_db: -6.0, release_ms: -10.0 },
            AudioEffect::Expander {
                ratio: 0.2,
                threshold_db: -40.0,
                attack_ms: -1.0,
                release_ms: -2.0,
                output_db: 0.0,
                rms: true,
            },
            AudioEffect::UpwardCompressor {
                ratio: 0.5,
                threshold_db: -20.0,
                attack_ms: 0.0,
                release_ms: -5.0,
                output_db: 0.0,
                knee_db: 0.0,
            },
            AudioEffect::NoiseGate { open_db: -26.0, close_db: -32.0, attack_ms: -1.0, hold_ms: -5.0, release_ms: 0.0 },
        ];
        let inp = sine(440.0, 0.4, 0.1);
        for effect in bad {
            let out = run(effect, &inp);
            assert!(out.iter().all(|v| v.is_finite()), "{effect:?} produced non-finite output");
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    #[allow(unsafe_code)]
    fn enable_ftz_daz_sets_mxcsr_bits_on_x86() {
        let got = std::thread::spawn(|| {
            enable_ftz_daz();
            let mut mxcsr: u32 = 0;
            // SAFETY: SSE exists on x86_64 (and x86 test runners supporting SSE); reads MXCSR into local u32.
            unsafe {
                core::arch::asm!(
                    "stmxcsr [{ptr}]",
                    ptr = in(reg) &raw mut mxcsr,
                    options(nostack, preserves_flags),
                );
            }
            mxcsr
        })
        .join()
        .unwrap_or(0);
        assert_ne!(got & 0x8000, 0, "FTZ bit must be set in MXCSR: {got:#x}");
        if std::arch::is_x86_feature_detected!("sse3") {
            assert_ne!(got & 0x0040, 0, "DAZ bit must be set in MXCSR when SSE3 is present: {got:#x}");
        }
    }

    /// Independent OBS `eq-filter.c` reference (for pinning `Eq3` transfer curves).
    fn obs_eq3_ref(input: &[f32], rate: f32, low_db: f32, mid_db: f32, high_db: f32) -> Vec<f32> {
        let lf = 2.0 * (std::f32::consts::PI * 800.0 / rate).sin();
        let hf = 2.0 * (std::f32::consts::PI * 5000.0 / rate).sin();
        let (lg, mg, hg) = (db_to_mul(low_db), db_to_mul(mid_db), db_to_mul(high_db));
        const VSA: f32 = 1.0 / 4_294_967_295.0;
        let (mut f1p, mut f2p, mut s) = ([0.0f32; 4], [0.0f32; 4], [0.0f32; 3]);
        let mut out = Vec::with_capacity(input.len());
        for &sample in input {
            f1p[0] += lf * (sample - f1p[0]) + VSA;
            f1p[1] += lf * (f1p[0] - f1p[1]);
            f1p[2] += lf * (f1p[1] - f1p[2]);
            f1p[3] += lf * (f1p[2] - f1p[3]);
            let l = f1p[3];
            f2p[0] += hf * (sample - f2p[0]) + VSA;
            f2p[1] += hf * (f2p[0] - f2p[1]);
            f2p[2] += hf * (f2p[1] - f2p[2]);
            f2p[3] += hf * (f2p[2] - f2p[3]);
            let h = s[2] - f2p[3];
            let m = s[2] - (h + l);
            s[2] = s[1];
            s[1] = s[0];
            s[0] = sample;
            out.push(l * lg + m * mg + h * hg);
        }
        out
    }

    fn run_at_rate(effect: AudioEffect, rate: u32, input: &[f32]) -> Vec<f32> {
        let mut f = AudioFilter::new_without_plugin(effect, rate);
        let (mut l, mut r) = (input.to_vec(), input.to_vec());
        for (a, b) in l.chunks_mut(1024).zip(r.chunks_mut(1024)) {
            f.process([a, b]);
        }
        l
    }

    /// Pin static transfer curves at 5 levels for every gain/dynamics/EQ filter against OBS math.
    #[test]
    fn static_gain_curves_at_five_levels() {
        let n = RATE as usize; // 1 s steady state
        let tail = n - 4800;

        // 1. Gain: 5 settings on -12 dBFS input.
        for gain_db in [-18.0f32, -6.0, 0.0, 6.0, 12.0] {
            let inp = vec![db_to_mul(-12.0); 256];
            let out = run(AudioEffect::Gain { db: gain_db }, &inp);
            let got = mul_to_db(out[255].abs());
            assert!((got - (-12.0 + gain_db)).abs() < 1e-3, "Gain({gain_db}): got {got:.4}");
        }

        // 2. Compressor: 4:1 @ -18 dB (0 makeup) and 10:1 @ -18 dB (+4 makeup) at 5 levels.
        for (ratio, out_db) in [(4.0f32, 0.0f32), (10.0, 4.0)] {
            let thresh = -18.0f32;
            let slope = 1.0 - 1.0 / ratio;
            let eff = AudioEffect::Compressor {
                ratio,
                threshold_db: thresh,
                attack_ms: 6.0,
                release_ms: 60.0,
                output_db: out_db,
            };
            for in_db in [-30.0f32, -18.0, -12.0, -6.0, 0.0] {
                let inp = vec![db_to_mul(in_db); n];
                let out = run(eff, &inp);
                let got = mul_to_db(out[n - 1].abs());
                let expected = in_db + (slope * (thresh - in_db)).min(0.0) + out_db;
                assert!(
                    (got - expected).abs() < 0.05,
                    "Compressor({ratio}:1, out={out_db}) @ {in_db} dB: got {got:.3}, expected {expected:.3}"
                );
            }
        }

        // 3. Limiter: -6 dB ceiling at 5 levels.
        let lim = AudioEffect::Limiter { threshold_db: -6.0, release_ms: 60.0 };
        for in_db in [-18.0f32, -12.0, -6.0, -3.0, 0.0] {
            let inp = vec![db_to_mul(in_db); n];
            let out = run(lim, &inp);
            let got = mul_to_db(out[n - 1].abs());
            let expected = in_db.min(-6.0);
            assert!((got - expected).abs() < 0.05, "Limiter(-6) @ {in_db} dB: got {got:.3}, expected {expected:.3}");
        }

        // 4. Expander: RMS 2:1 @ -40 dB and Peak 10:1 (gate preset) @ -40 dB at 5 levels.
        for (ratio, rms_mode, levels) in
            [(2.0f32, true, [-60.0f32, -50.0, -45.0, -40.0, -20.0]), (10.0, false, [-50.0, -44.0, -42.0, -40.0, -20.0])]
        {
            let thresh = -40.0f32;
            let eff = AudioEffect::Expander {
                ratio,
                threshold_db: thresh,
                attack_ms: 10.0,
                release_ms: 50.0,
                output_db: 0.0,
                rms: rms_mode,
            };
            for in_db in levels {
                let inp = vec![db_to_mul(in_db); n];
                let out = run(eff, &inp);
                let got = mul_to_db(out[n - 1].abs());
                let diff = (thresh - in_db).max(0.0);
                let expected = in_db + ((1.0 - ratio) * diff).max(-60.0);
                assert!(
                    (got - expected).abs() < 0.05,
                    "Expander(ratio={ratio}, rms={rms_mode}) @ {in_db} dB: got {got:.3}, expected {expected:.3}"
                );
            }
        }

        // 5. UpwardCompressor: 0.5 @ -20 dB, knee 10 dB — covers above knee, in knee, linear,
        //    noise-floor taper (below (thresh-60)/2 = -40 dB), and below -60 dB floor.
        let up = AudioEffect::UpwardCompressor {
            ratio: 0.5,
            threshold_db: -20.0,
            attack_ms: 10.0,
            release_ms: 50.0,
            output_db: 0.0,
            knee_db: 10.0,
        };
        for (in_db, expected_boost) in [(-65.0f32, 0.0f32), (-50.0, 5.0), (-35.0, 7.5), (-20.0, 0.625), (-10.0, 0.0)] {
            let inp = vec![db_to_mul(in_db); n];
            let out = run(up, &inp);
            let got_boost = mul_to_db(out[n - 1].abs()) - in_db;
            assert!(
                (got_boost - expected_boost).abs() < 0.05,
                "UpwardCompressor @ {in_db} dB: boost {got_boost:+.3}, expected {expected_boost:+.3}"
            );
        }

        // 6. Eq3: 5 gain levels per band against OBS eq-filter.c reference.
        for &(hz, band) in &[(100.0f32, 0usize), (1600.0, 1), (12000.0, 2)] {
            let inp = sine(hz, 0.25, 0.5);
            for db in [-12.0f32, -6.0, 0.0, 6.0, 12.0] {
                let mut g = [0.0f32; 3];
                g[band] = db;
                let eff = AudioEffect::Eq3 { low_db: g[0], mid_db: g[1], high_db: g[2] };
                let out = run(eff, &inp);
                let obs = obs_eq3_ref(&inp, RATE as f32, g[0], g[1], g[2]);
                let got_db = mul_to_db(rms(&out[tail / 2..]));
                let ref_db = mul_to_db(rms(&obs[tail / 2..]));
                assert!(
                    (got_db - ref_db).abs() < 0.05,
                    "Eq3 band={band} {db:+} dB @ {hz} Hz: got {got_db:.3}, obs {ref_db:.3}"
                );
            }
        }
    }

    /// Attack and release times reach 63.2% (1 - 1/e) within 10% of nominal ms across dynamics.
    #[test]
    fn attack_and_release_time_to_63_percent_within_10_percent() {
        let r = RATE as usize;
        let ms_of = |idx: usize| idx as f32 * 1000.0 / RATE as f32;

        // 1. Compressor: attack 20 ms, release 100 ms (recover envelope directly from gain).
        {
            let thresh = -20.0f32;
            let slope = 0.9f32; // 10:1
            let thresh_lin = db_to_mul(thresh);
            let mut f = AudioFilter::new(
                AudioEffect::Compressor {
                    ratio: 10.0,
                    threshold_db: thresh,
                    attack_ms: 20.0,
                    release_ms: 100.0,
                    output_db: 0.0,
                },
                RATE,
            );
            let mut l = vec![1.0f32; r / 2]; // 500 ms at 0 dBFS
            let mut rr = l.clone();
            f.process([&mut l, &mut rr]);
            // env(t) = thresh_lin * out(t)^(-1/slope) while env > thresh_lin.
            let atk_idx = l
                .iter()
                .position(|&y| {
                    let env = thresh_lin * y.max(1e-9).powf(-1.0 / slope);
                    env >= 1.0 - (-1.0f32).exp()
                })
                .expect("compressor attack crossed 63.2%");
            let atk_ms = ms_of(atk_idx);
            assert!((atk_ms - 20.0).abs() <= 2.0, "Compressor attack 63%: {atk_ms:.2} ms (want 20±2)");

            // Release step: drop to -40 dBFS (0.01); 63.2% decay of env from 1.0 -> 0.01 is 0.3742 > 0.1.
            let lo = db_to_mul(-40.0);
            let target_env = lo + (1.0 - lo) * (-1.0f32).exp();
            let mut l2 = vec![lo; r];
            let mut r2 = l2.clone();
            f.process([&mut l2, &mut r2]);
            let rel_idx = l2
                .iter()
                .position(|&y| {
                    let g = (y / lo).clamp(1e-9, 1.0);
                    let env = thresh_lin * g.powf(-1.0 / slope);
                    env <= target_env
                })
                .expect("compressor release crossed 63.2%");
            let rel_ms = ms_of(rel_idx);
            assert!((rel_ms - 100.0).abs() <= 10.0, "Compressor release 63%: {rel_ms:.2} ms (want 100±10)");
        }

        // 2. Limiter: instant attack (< 0.1 ms), release 80 ms to 63.2%.
        {
            let thresh = -20.0f32;
            let thresh_lin = db_to_mul(thresh);
            let mut f = AudioFilter::new(AudioEffect::Limiter { threshold_db: thresh, release_ms: 80.0 }, RATE);
            let mut l = vec![1.0f32; r / 4];
            let mut rr = l.clone();
            f.process([&mut l, &mut rr]);
            assert!(mul_to_db(l[0]) <= thresh + 0.05, "Limiter must clamp on sample 0: {}", mul_to_db(l[0]));

            let lo = db_to_mul(-40.0);
            let target_env = lo + (1.0 - lo) * (-1.0f32).exp();
            let mut l2 = vec![lo; r];
            let mut r2 = l2.clone();
            f.process([&mut l2, &mut r2]);
            let rel_idx = l2
                .iter()
                .position(|&y| {
                    let env = thresh_lin / (y / lo).max(1e-9);
                    env <= target_env
                })
                .expect("limiter release crossed 63.2%");
            let rel_ms = ms_of(rel_idx);
            assert!((rel_ms - 80.0).abs() <= 8.0, "Limiter release 63%: {rel_ms:.2} ms (want 80±8)");
        }

        // 3. Expander (Peak detector): attack 20 ms (opening), release 80 ms (closing) in dB domain.
        {
            let mut f = AudioFilter::new(
                AudioEffect::Expander {
                    ratio: 4.0,
                    threshold_db: -20.0,
                    attack_ms: 20.0,
                    release_ms: 80.0,
                    output_db: 0.0,
                    rms: false,
                },
                RATE,
            );
            let quiet = db_to_mul(-30.0); // steady gain = -30 dB
            let loud = db_to_mul(-10.0); // steady gain = 0 dB
            let (mut lw, mut rw) = (vec![quiet; r], vec![quiet; r]);
            f.process([&mut lw, &mut rw]);

            // Attack (opening): -30 dB -> 0 dB, 63.2% point = -30 * exp(-1) = -11.036 dB.
            let (mut la, mut ra) = (vec![loud; r / 2], vec![loud; r / 2]);
            f.process([&mut la, &mut ra]);
            let atk_target = -30.0 * (-1.0f32).exp();
            let atk_idx =
                la.iter().position(|&y| mul_to_db(y / loud) >= atk_target).expect("expander attack crossed 63.2%");
            let atk_ms = ms_of(atk_idx);
            assert!((atk_ms - 20.0).abs() <= 2.0, "Expander attack 63%: {atk_ms:.2} ms (want 20±2)");

            // Release (closing): 0 dB -> -30 dB, 63.2% point = -30 * (1 - exp(-1)) = -18.964 dB.
            let (mut lr, mut rr) = (vec![quiet; r], vec![quiet; r]);
            f.process([&mut lr, &mut rr]);
            let rel_target = -30.0 * (1.0 - (-1.0f32).exp());
            let rel_idx =
                lr.iter().position(|&y| mul_to_db(y / quiet) <= rel_target).expect("expander release crossed 63.2%");
            let rel_ms = ms_of(rel_idx);
            assert!((rel_ms - 80.0).abs() <= 8.0, "Expander release 63%: {rel_ms:.2} ms (want 80±8)");
        }

        // 4. UpwardCompressor: attack 200 ms (boost rise), release 100 ms (boost decay).
        //    Small downward step (-20 -> -21 dBFS) keeps the 10 ms half-life RMS detector lag
        //    (~14 ms) within 10% of 200 ms; also verify sample-exact match against OBS math.
        {
            let mut f = AudioFilter::new(
                AudioEffect::UpwardCompressor {
                    ratio: 0.5,
                    threshold_db: -20.0,
                    attack_ms: 200.0,
                    release_ms: 100.0,
                    output_db: 0.0,
                    knee_db: 0.0,
                },
                RATE,
            );
            let above = db_to_mul(-20.0);
            let below = db_to_mul(-21.0); // steady boost = +0.5 dB
            let loud = db_to_mul(-6.0);
            let (mut lw, mut rw) = (vec![above; r / 2], vec![above; r / 2]);
            f.process([&mut lw, &mut rw]);

            let (mut la, mut ra) = (vec![below; r * 2], vec![below; r * 2]);
            f.process([&mut la, &mut ra]);
            let atk_target = 0.5 * (1.0 - (-1.0f32).exp());
            let atk_idx =
                la.iter().position(|&y| mul_to_db(y / below) >= atk_target).expect("upward attack crossed 63.2%");
            let atk_ms = ms_of(atk_idx);
            assert!((atk_ms - 200.0).abs() <= 20.0, "UpwardCompressor attack 63%: {atk_ms:.2} ms (want 200±20)");

            let (mut lr, mut rr) = (vec![loud; r], vec![loud; r]);
            f.process([&mut lr, &mut rr]);
            let rel_target = 0.5 * (-1.0f32).exp();
            let rel_idx =
                lr.iter().position(|&y| mul_to_db(y / loud) <= rel_target).expect("upward release crossed 63.2%");
            let rel_ms = ms_of(rel_idx);
            assert!((rel_ms - 100.0).abs() <= 10.0, "UpwardCompressor release 63%: {rel_ms:.2} ms (want 100±10)");
        }
    }

    /// NoiseGate: exact OBS open/close hysteresis (including narrow -26/-28 gap), linear attack,
    /// hold, linear release, and safety floor when configured backwards (`close >= open`).
    #[test]
    fn noise_gate_obs_hysteresis_and_hold() {
        let r = RATE as usize;
        let ms = |m: usize| m * r / 1000;

        // 1. Standard OBS defaults: -26 open / -32 close, 25 ms attack, 200 ms hold, 150 ms release.
        {
            let mut f = AudioFilter::new(
                AudioEffect::NoiseGate {
                    open_db: -26.0,
                    close_db: -32.0,
                    attack_ms: 25.0,
                    hold_ms: 200.0,
                    release_ms: 150.0,
                },
                RATE,
            );
            // Mid-band (-29 dBFS) while closed stays closed.
            let mid = db_to_mul(-29.0);
            let (mut l, mut rr) = (vec![mid; ms(100)], vec![mid; ms(100)]);
            f.process([&mut l, &mut rr]);
            assert!(peak(&l) < 1e-6, "gate must not open in hysteresis band when closed");

            // Loud (-20 dBFS) opens linearly over 25 ms (63.2% around 15.8 ms, 100% by 25 ms).
            let loud = db_to_mul(-20.0);
            let (mut l, mut rr) = (vec![loud; ms(60)], vec![loud; ms(60)]);
            f.process([&mut l, &mut rr]);
            let g_63 = l.iter().position(|&v| v / loud >= 0.632).expect("gate attack reached 63.2%");
            let g_63_ms = g_63 as f32 * 1000.0 / RATE as f32;
            assert!((g_63_ms - 25.0 * 0.632).abs() <= 1.5, "gate linear attack 63%: {g_63_ms:.2} ms");
            assert!((l[ms(26)] / loud - 1.0).abs() < 1e-4, "gate fully open after 25 ms attack");

            // Drop into hysteresis band (-29 dBFS, above -32 close): must stay 100% open.
            let (mut l, mut rr) = (vec![mid; ms(400)], vec![mid; ms(400)]);
            f.process([&mut l, &mut rr]);
            assert!((l[ms(399)] / mid - 1.0).abs() < 1e-4, "gate must stay open inside hysteresis band");

            // Drop below close (-40 dBFS): level decays from -29 to -32 (<14 ms), holds 200 ms,
            // then releases linearly over 150 ms.
            let quiet = db_to_mul(-40.0);
            let (mut l, mut rr) = (vec![quiet; ms(500)], vec![quiet; ms(500)]);
            f.process([&mut l, &mut rr]);
            assert!((l[ms(190)] / quiet - 1.0).abs() < 1e-4, "gate must stay open through hold");
            let rel_start = l.iter().position(|&v| v / quiet < 0.999).expect("gate starts releasing after hold");
            let rel_start_ms = rel_start as f32 * 1000.0 / RATE as f32;
            assert!(
                (200.0..=218.0).contains(&rel_start_ms),
                "gate release should start after decay + 200 ms hold, started at {rel_start_ms:.1} ms"
            );
            let rel_63 = l[rel_start..].iter().position(|&v| v / quiet <= 1.0 - 0.632).expect("gate 63% release");
            let rel_63_ms = rel_63 as f32 * 1000.0 / RATE as f32;
            assert!((rel_63_ms - 150.0 * 0.632).abs() <= 3.0, "gate release 63%: {rel_63_ms:.1} ms");
            assert!(l[ms(400)].abs() < 1e-6, "gate fully shut after hold + release");
        }

        // 2. Narrow OBS hysteresis (-26/-28): must preserve exact -28 close (not clamp to -29).
        {
            let mut f = AudioFilter::new(
                AudioEffect::NoiseGate {
                    open_db: -26.0,
                    close_db: -28.0,
                    attack_ms: 5.0,
                    hold_ms: 20.0,
                    release_ms: 20.0,
                },
                RATE,
            );
            let (mut l, mut rr) = (vec![db_to_mul(-20.0); ms(40)], vec![db_to_mul(-20.0); ms(40)]);
            f.process([&mut l, &mut rr]);
            // -27 dBFS is above -28 close -> stays open.
            let s27 = db_to_mul(-27.0);
            let (mut l, mut rr) = (vec![s27; ms(100)], vec![s27; ms(100)]);
            f.process([&mut l, &mut rr]);
            assert!((l[ms(99)] / s27 - 1.0).abs() < 1e-4, "-27 dBFS must keep -26/-28 gate open");
            // -28.5 dBFS is below -28 close (but above -29) -> must close.
            let s285 = db_to_mul(-28.5);
            let (mut l, mut rr) = (vec![s285; ms(150)], vec![s285; ms(150)]);
            f.process([&mut l, &mut rr]);
            assert!(l[ms(149)].abs() < 1e-6, "-28.5 dBFS must close -26/-28 gate (got {})", l[ms(149)]);
        }

        // 3. Backwards (`close >= open`) or sub-0.01 dB near-equal: never stalls open forever.
        for close_db in [-20.0f32, -26.0, -26.000_002] {
            let mut f = AudioFilter::new(
                AudioEffect::NoiseGate { open_db: -26.0, close_db, attack_ms: 5.0, hold_ms: 10.0, release_ms: 10.0 },
                RATE,
            );
            let (mut l, mut rr) = (vec![db_to_mul(-12.0); ms(30)], vec![db_to_mul(-12.0); ms(30)]);
            f.process([&mut l, &mut rr]);
            // Linear decay from -12 dB (0.251) to -29 dB (0.035) at (open-close)/(1/75 s) takes ~200 ms + hold + release.
            let (mut l, mut rr) = (vec![0.0f32; ms(350)], vec![0.0f32; ms(350)]);
            f.process([&mut l, &mut rr]);
            assert!(!f.is_open && f.attenuation == 0.0, "gate close_db={close_db} must still close on silence");
        }
    }

    /// Dragging a parameter live (`retune`) produces no step click / zipper noise on any filter.
    #[test]
    fn parameter_drag_produces_no_zipper_noise() {
        // 440 Hz sine at 0.25 amp: natural max sample-to-sample delta is ~0.0144 (at +12 dB: ~0.058).
        let sig = sine(440.0, 0.25, 0.2);
        let mid = sig.len() / 2;
        let cases: [(&str, AudioEffect, AudioEffect); 8] = [
            ("Gain", AudioEffect::Gain { db: 0.0 }, AudioEffect::Gain { db: 12.0 }),
            (
                "Compressor",
                AudioEffect::Compressor {
                    ratio: 2.0,
                    threshold_db: -12.0,
                    attack_ms: 6.0,
                    release_ms: 60.0,
                    output_db: 0.0,
                },
                AudioEffect::Compressor {
                    ratio: 12.0,
                    threshold_db: -36.0,
                    attack_ms: 6.0,
                    release_ms: 60.0,
                    output_db: 6.0,
                },
            ),
            (
                "Limiter",
                AudioEffect::Limiter { threshold_db: -1.0, release_ms: 60.0 },
                AudioEffect::Limiter { threshold_db: -18.0, release_ms: 60.0 },
            ),
            (
                "Expander",
                AudioEffect::Expander {
                    ratio: 2.0,
                    threshold_db: -40.0,
                    attack_ms: 0.0,
                    release_ms: 0.0,
                    output_db: 0.0,
                    rms: false,
                },
                AudioEffect::Expander {
                    ratio: 6.0,
                    threshold_db: -6.0,
                    attack_ms: 0.0,
                    release_ms: 0.0,
                    output_db: 6.0,
                    rms: false,
                },
            ),
            (
                "UpwardCompressor",
                AudioEffect::UpwardCompressor {
                    ratio: 0.5,
                    threshold_db: -20.0,
                    attack_ms: 10.0,
                    release_ms: 50.0,
                    output_db: 0.0,
                    knee_db: 10.0,
                },
                AudioEffect::UpwardCompressor {
                    ratio: 0.2,
                    threshold_db: -6.0,
                    attack_ms: 10.0,
                    release_ms: 50.0,
                    output_db: 6.0,
                    knee_db: 10.0,
                },
            ),
            (
                "Eq3",
                AudioEffect::Eq3 { low_db: 0.0, mid_db: 0.0, high_db: 0.0 },
                AudioEffect::Eq3 { low_db: 12.0, mid_db: -12.0, high_db: 12.0 },
            ),
            (
                "NoiseGate",
                AudioEffect::NoiseGate {
                    open_db: -26.0,
                    close_db: -32.0,
                    attack_ms: 25.0,
                    hold_ms: 200.0,
                    release_ms: 150.0,
                },
                AudioEffect::NoiseGate {
                    open_db: -6.0,
                    close_db: -12.0,
                    attack_ms: 25.0,
                    hold_ms: 200.0,
                    release_ms: 150.0,
                },
            ),
            ("Denoise", AudioEffect::Denoise { strength: 20 }, AudioEffect::Denoise { strength: 100 }),
        ];

        for (name, before, after) in cases {
            let mut f = AudioFilter::new_without_plugin(before, RATE);
            let (mut l, mut r) = (sig.clone(), sig.clone());
            // Split right near a sine peak so an unsmoothed jump produces the worst possible step.
            let split = (mid..mid + 120).max_by(|&a, &b| sig[a].abs().total_cmp(&sig[b].abs())).unwrap();
            f.process([&mut l[..split], &mut r[..split]]);
            assert!(f.retune(after), "{name} must retune in place");
            f.process([&mut l[split..], &mut r[split..]]);

            let max_step = l[split - 8..split + 512].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            assert!(max_step < 0.08, "{name} zipper click on retune: max delta = {max_step:.4}");
        }
    }

    /// Enabling and disabling any filter mid-signal crossfades cleanly with no click.
    #[test]
    fn enable_and_disable_produce_no_clicks() {
        let sig = sine(440.0, 0.3, 0.3);
        let t1 = (RATE as usize / 10..RATE as usize / 10 + 120)
            .max_by(|&a, &b| sig[a].abs().total_cmp(&sig[b].abs()))
            .unwrap();
        let t2 = (2 * RATE as usize / 10..2 * RATE as usize / 10 + 120)
            .max_by(|&a, &b| sig[a].abs().total_cmp(&sig[b].abs()))
            .unwrap();
        let effects = [
            AudioEffect::Gain { db: 10.0 },
            AudioEffect::InvertPolarity,
            AudioEffect::NoiseGate {
                open_db: -26.0,
                close_db: -32.0,
                attack_ms: 10.0,
                hold_ms: 50.0,
                release_ms: 50.0,
            },
            AudioEffect::Compressor {
                ratio: 10.0,
                threshold_db: -24.0,
                attack_ms: 2.0,
                release_ms: 50.0,
                output_db: 0.0,
            },
            AudioEffect::Limiter { threshold_db: -18.0, release_ms: 50.0 },
            AudioEffect::Expander {
                ratio: 4.0,
                threshold_db: -6.0,
                attack_ms: 10.0,
                release_ms: 50.0,
                output_db: 0.0,
                rms: true,
            },
            AudioEffect::UpwardCompressor {
                ratio: 0.5,
                threshold_db: -6.0,
                attack_ms: 10.0,
                release_ms: 50.0,
                output_db: 0.0,
                knee_db: 10.0,
            },
            AudioEffect::Eq3 { low_db: 12.0, mid_db: -12.0, high_db: 12.0 },
            AudioEffect::Denoise { strength: 100 },
        ];
        for eff in effects {
            let mut f = AudioFilter::new_without_plugin(eff, RATE);
            let (mut l, mut r) = (sig.clone(), sig.clone());
            f.process([&mut l[..t1], &mut r[..t1]]);
            f.set_enabled(false);
            assert!(!f.is_enabled());
            f.process([&mut l[t1..t2], &mut r[t1..t2]]);
            // After 8 ms (384 samples) fade-out, output is exact dry input.
            let dry_err =
                l[t1 + 500..t2].iter().zip(&sig[t1 + 500..t2]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(dry_err < 1e-6, "{eff:?} disabled must pass exact dry signal: err={dry_err}");

            f.set_enabled(true);
            assert!(f.is_enabled());
            f.process([&mut l[t2..], &mut r[t2..]]);

            let step_off = l[t1 - 4..t1 + 420].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            let step_on = l[t2 - 4..t2 + 420].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            assert!(step_off < 0.08, "{eff:?} click on disable: max delta = {step_off:.4}");
            assert!(step_on < 0.08, "{eff:?} click on enable: max delta = {step_on:.4}");
        }
    }

    /// Stereo linking: asymmetric L/R inputs receive identical gain on both channels (no image shift).
    #[test]
    fn stereo_linked_filters_apply_identical_lr_gain() {
        let in_l = sine(500.0, db_to_mul(-10.0), 0.5);
        let in_r = sine(730.0, db_to_mul(-35.0), 0.5);
        let effects = [
            AudioEffect::NoiseGate {
                open_db: -26.0,
                close_db: -32.0,
                attack_ms: 10.0,
                hold_ms: 100.0,
                release_ms: 50.0,
            },
            AudioEffect::Compressor {
                ratio: 8.0,
                threshold_db: -24.0,
                attack_ms: 5.0,
                release_ms: 50.0,
                output_db: 2.0,
            },
            AudioEffect::Limiter { threshold_db: -18.0, release_ms: 50.0 },
            AudioEffect::Expander {
                ratio: 4.0,
                threshold_db: -20.0,
                attack_ms: 10.0,
                release_ms: 50.0,
                output_db: 0.0,
                rms: true,
            },
            AudioEffect::Expander {
                ratio: 4.0,
                threshold_db: -20.0,
                attack_ms: 10.0,
                release_ms: 50.0,
                output_db: 0.0,
                rms: false,
            },
            AudioEffect::UpwardCompressor {
                ratio: 0.5,
                threshold_db: -15.0,
                attack_ms: 10.0,
                release_ms: 50.0,
                output_db: 0.0,
                knee_db: 10.0,
            },
            AudioEffect::Denoise { strength: 80 },
        ];
        for eff in effects {
            let mut f = AudioFilter::new_without_plugin(eff, RATE);
            let (mut l, mut r) = (in_l.clone(), in_r.clone());
            for (a, b) in l.chunks_mut(512).zip(r.chunks_mut(512)) {
                f.process([a, b]);
            }
            let mut checked = 0usize;
            for i in 4800..in_l.len() {
                if in_l[i].abs() > 0.05 && in_r[i].abs() > 0.005 {
                    let gl = l[i] / in_l[i];
                    let gr = r[i] / in_r[i];
                    assert!((gl - gr).abs() < 1e-5, "{eff:?} stereo unlinked at {i}: gl={gl:.6}, gr={gr:.6}");
                    checked += 1;
                }
            }
            assert!(checked > 1000, "{eff:?} checked too few samples: {checked}");
        }
    }

    /// A mono mic on one channel (the other silent) comes out of the stereo-linked Expander and
    /// Upward compressor exactly as the same mic on both channels did: with nothing on the other
    /// side the linked level is the mic's own, so saved scenes sound as they did.
    #[test]
    fn a_mono_mic_on_one_channel_sounds_as_it_did_before_linking() {
        let mic: Vec<f32> = sine(300.0, db_to_mul(-12.0), 1.0)
            .iter()
            .zip(sine(3.0, 1.0, 1.0))
            .map(|(a, e)| a * (0.3 + 0.7 * e.abs()))
            .collect();
        let effects = [
            AudioEffect::Expander {
                ratio: 4.0,
                threshold_db: -30.0,
                attack_ms: 10.0,
                release_ms: 50.0,
                output_db: 0.0,
                rms: true,
            },
            AudioEffect::Expander {
                ratio: 10.0,
                threshold_db: -30.0,
                attack_ms: 10.0,
                release_ms: 125.0,
                output_db: 0.0,
                rms: false,
            },
            AudioEffect::UpwardCompressor {
                ratio: 0.5,
                threshold_db: -20.0,
                attack_ms: 10.0,
                release_ms: 50.0,
                output_db: 0.0,
                knee_db: 10.0,
            },
        ];
        for eff in effects {
            // On both channels (how a mono mic reaches the filters).
            let mut both = AudioFilter::new_without_plugin(eff, RATE);
            let (mut bl, mut br) = (mic.clone(), mic.clone());
            for (a, b) in bl.chunks_mut(480).zip(br.chunks_mut(480)) {
                both.process([a, b]);
            }
            // On the left only.
            let mut left = AudioFilter::new_without_plugin(eff, RATE);
            let (mut ll, mut lr) = (mic.clone(), vec![0.0f32; mic.len()]);
            for (a, b) in ll.chunks_mut(480).zip(lr.chunks_mut(480)) {
                left.process([a, b]);
            }
            let worst = bl.iter().zip(&ll).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(worst < 1e-6, "{eff:?}: the left-only mic differs from the dual mono one by {worst}");
            assert!(lr.iter().all(|v| v.abs() < 1e-6), "{eff:?}: the empty side stays empty");
        }
    }

    /// Sample-rate scaling across 44.1 kHz, 48 kHz, and 96 kHz preserves timings and EQ crossovers.
    #[test]
    fn sample_rates_44100_48000_96000_preserve_timings_and_eq() {
        let mut eq_cuts = Vec::new();
        for rate in [44_100u32, 48_000, 96_000] {
            // Compressor 20 ms attack at each rate.
            let n = rate as usize / 2;
            let comp = AudioEffect::Compressor {
                ratio: 10.0,
                threshold_db: -20.0,
                attack_ms: 20.0,
                release_ms: 80.0,
                output_db: 0.0,
            };
            let out = run_at_rate(comp, rate, &vec![1.0f32; n]);
            let thresh_lin = db_to_mul(-20.0);
            let atk_idx = out
                .iter()
                .position(|&y| thresh_lin * y.max(1e-9).powf(-1.0 / 0.9) >= 1.0 - (-1.0f32).exp())
                .expect("attack reached 63.2%");
            let atk_ms = atk_idx as f32 * 1000.0 / rate as f32;
            assert!((atk_ms - 20.0).abs() <= 2.0, "rate={rate}: compressor attack {atk_ms:.2} ms");

            // NoiseGate 25 ms attack at each rate.
            let gate = AudioEffect::NoiseGate {
                open_db: -26.0,
                close_db: -32.0,
                attack_ms: 25.0,
                hold_ms: 100.0,
                release_ms: 100.0,
            };
            let out_g = run_at_rate(gate, rate, &vec![0.5f32; rate as usize / 5]);
            let full_idx = out_g.iter().position(|&v| v / 0.5 >= 0.999).expect("gate opened");
            let full_ms = full_idx as f32 * 1000.0 / rate as f32;
            assert!((full_ms - 25.0).abs() <= 1.0, "rate={rate}: gate attack {full_ms:.2} ms");

            // Eq3 -12 dB low cut at 100 Hz matches across rates.
            let sig: Vec<f32> = (0..rate as usize / 2)
                .map(|i| 0.5 * (std::f32::consts::TAU * 100.0 * i as f32 / rate as f32).sin())
                .collect();
            let out_e = run_at_rate(AudioEffect::Eq3 { low_db: -12.0, mid_db: 0.0, high_db: 0.0 }, rate, &sig);
            let cut_db = mul_to_db(rms(&out_e[rate as usize / 4..]) / rms(&sig[rate as usize / 4..]));
            eq_cuts.push(cut_db);
        }
        let spread = eq_cuts.iter().copied().fold(f32::NEG_INFINITY, f32::max)
            - eq_cuts.iter().copied().fold(f32::INFINITY, f32::min);
        assert!(spread < 1.0, "Eq3 low cut drifted across 44.1/48/96 kHz: {eq_cuts:?}");
    }

    /// Sidechain compression (`process_sidechain`) matches OBS `compressor-filter.c:analyze_sidechain`.
    #[test]
    fn compressor_sidechain_matches_obs_reference() {
        let r = RATE as usize;
        let music = sine(300.0, db_to_mul(-12.0), 1.0);
        let mut voice = vec![0.0f32; r];
        let burst = sine(440.0, db_to_mul(-6.0), 0.4);
        voice[r / 10..r / 10 + burst.len()].copy_from_slice(&burst);

        let eff = AudioEffect::Compressor {
            ratio: 10.0,
            threshold_db: -24.0,
            attack_ms: 10.0,
            release_ms: 150.0,
            output_db: 0.0,
        };
        let mut f = AudioFilter::new(eff, RATE);
        let (mut l, mut rr) = (music.clone(), music.clone());
        for i in (0..r).step_by(512) {
            let end = (i + 512).min(r);
            f.process_sidechain([&mut l[i..end], &mut rr[i..end]], [&voice[i..end], &voice[i..end]]);
        }

        // Independent OBS compressor-filter.c sidechain reference.
        let ag = (-1.0f32 / (RATE as f32 * 0.010)).exp();
        let rg = (-1.0f32 / (RATE as f32 * 0.150)).exp();
        let slope = 1.0 - 1.0 / 10.0;
        let mut env = 0.0f32;
        let mut max_diff = 0.0f32;
        for i in 0..r {
            let x = voice[i].abs();
            env = if env < x { x + ag * (env - x) } else { x + rg * (env - x) };
            let g = if env > 0.0 { db_to_mul((slope * (-24.0 - mul_to_db(env))).min(0.0)) } else { 1.0 };
            let expected = music[i] * g;
            max_diff = max_diff.max((l[i] - expected).abs());
        }
        assert!(max_diff < 1e-5, "sidechain diverged from OBS reference: max_diff={max_diff}");
        // Verify real ducking occurred during the voice burst and recovered afterward.
        let ducked = mul_to_db(rms(&l[r / 5..r / 2]) / rms(&music[r / 5..r / 2]));
        let recovered = mul_to_db(rms(&l[9 * r / 10..]) / rms(&music[9 * r / 10..]));
        assert!(ducked < -10.0, "music should duck during sidechain burst: {ducked:+.2} dB");
        assert!(recovered > -0.5, "music should recover after burst: {recovered:+.2} dB");
    }

    /// Typical OBS voice chain (gain +6, gate -32/-26, compressor 10:1 @ -18, limiter -1) on a
    /// speech-like signal with pauses: loudness, peak, and crest factor match an independent
    /// OBS C-formula reference within 0.05 dB.
    #[test]
    fn obs_voice_chain_matches_obs_reference_loudness_peak_crest() {
        let r = RATE as usize;
        // Build 6.5 s speech-like stereo signal: 0.6 s room noise, 1.8 s speech, 0.8 s pause,
        // 1.8 s louder speech, 1.5 s trailing pause (allows OBS's linear level decay + 200 ms hold + 150 ms release).
        let mut seed = 42u32;
        let mut sig = pink_noise(65 * r / 10, db_to_mul(-50.0), &mut seed);
        let phrase1 = speech_of((1.8 * r as f32) as usize, db_to_mul(-20.0), 125.0, 0.6);
        let phrase2 = speech_of((1.8 * r as f32) as usize, db_to_mul(-14.0), 155.0, 0.5);
        for (i, &s) in phrase1.iter().enumerate() {
            sig[6 * r / 10 + i] += s;
        }
        for (i, &s) in phrase2.iter().enumerate() {
            sig[32 * r / 10 + i] += s;
        }

        // Run through Ferrocast's AudioFilter chain in 480-sample (10 ms) real-world blocks.
        let mut chain = [
            AudioFilter::new(AudioEffect::Gain { db: 6.0 }, RATE),
            AudioFilter::new(
                AudioEffect::NoiseGate {
                    open_db: -26.0,
                    close_db: -32.0,
                    attack_ms: 25.0,
                    hold_ms: 200.0,
                    release_ms: 150.0,
                },
                RATE,
            ),
            AudioFilter::new(
                AudioEffect::Compressor {
                    ratio: 10.0,
                    threshold_db: -18.0,
                    attack_ms: 6.0,
                    release_ms: 60.0,
                    output_db: 0.0,
                },
                RATE,
            ),
            AudioFilter::new(AudioEffect::Limiter { threshold_db: -1.0, release_ms: 60.0 }, RATE),
        ];
        let (mut l, mut rr) = (sig.clone(), sig.clone());
        for (a, b) in l.chunks_mut(480).zip(rr.chunks_mut(480)) {
            for f in &mut chain {
                f.process([a, b]);
            }
        }

        // Independent OBS C-formula reference (gain-filter.c, noise-gate-filter.c,
        // compressor-filter.c, limiter-filter.c).
        let obs = {
            let rate = RATE as f32;
            let mut x = sig.clone();
            // 1. gain-filter.c (+6 dB)
            let gm = 10f32.powf(6.0 / 20.0);
            for v in &mut x {
                *v *= gm;
            }
            // 2. noise-gate-filter.c (-26/-32, 25/200/150)
            let open = 10f32.powf(-26.0 / 20.0);
            let close = 10f32.powf(-32.0 / 20.0);
            let atk = 1.0 / (0.025 * rate);
            let rel = 1.0 / (0.150 * rate);
            let dec = (open - close) / ((1.0 / 75.0) * rate);
            let mut is_open = false;
            let (mut att, mut lvl, mut held) = (0.0f32, 0.0f32, 0.0f32);
            for v in &mut x {
                let c = v.abs();
                if c > open && !is_open {
                    is_open = true;
                }
                if lvl < close && is_open {
                    held = 0.0;
                    is_open = false;
                }
                lvl = (lvl.max(c) - dec).max(0.0);
                if is_open {
                    att = (att + atk).min(1.0);
                } else {
                    held += 1.0 / rate;
                    if held > 0.200 {
                        att = (att - rel).max(0.0);
                    }
                }
                *v *= att;
            }
            // 3. compressor-filter.c (10:1 @ -18, 6/60 ms)
            let cag = (-1.0 / (rate * 0.006)).exp();
            let crg = (-1.0 / (rate * 0.060)).exp();
            let cslope = 1.0 - 1.0 / 10.0;
            let mut cenv = 0.0f32;
            for v in &mut x {
                let c = v.abs();
                cenv = if cenv < c { c + cag * (cenv - c) } else { c + crg * (cenv - c) };
                let g =
                    if cenv > 0.0 { 10f32.powf((cslope * (-18.0 - 20.0 * cenv.log10())).min(0.0) / 20.0) } else { 1.0 };
                *v *= g;
            }
            // 4. limiter-filter.c (-1 dB, 60 ms)
            let lag = (-1.0 / (rate * 0.000001)).exp();
            let lrg = (-1.0 / (rate * 0.060)).exp();
            let mut lenv = 0.0f32;
            for v in &mut x {
                let c = v.abs();
                lenv = if lenv < c { c + lag * (lenv - c) } else { c + lrg * (lenv - c) };
                let g = if lenv > 0.0 { 10f32.powf((-1.0 - 20.0 * lenv.log10()).min(0.0) / 20.0) } else { 1.0 };
                *v *= g;
            }
            x
        };

        let ours_rms = mul_to_db(rms(&l));
        let obs_rms = mul_to_db(rms(&obs));
        let ours_active = mul_to_db(rms(&l[32 * r / 10..50 * r / 10]));
        let obs_active = mul_to_db(rms(&obs[32 * r / 10..50 * r / 10]));
        let ours_peak = mul_to_db(peak(&l));
        let obs_peak = mul_to_db(peak(&obs));
        let ours_crest = ours_peak - ours_active;
        let obs_crest = obs_peak - obs_active;

        assert!((ours_rms - obs_rms).abs() < 0.05, "overall RMS: ours={ours_rms:.3}, obs={obs_rms:.3}");
        assert!((ours_active - obs_active).abs() < 0.05, "active RMS: ours={ours_active:.3}, obs={obs_active:.3}");
        assert!((ours_peak - obs_peak).abs() < 0.05, "peak: ours={ours_peak:.3}, obs={obs_peak:.3}");
        assert!((ours_crest - obs_crest).abs() < 0.05, "crest: ours={ours_crest:.3}, obs={obs_crest:.3}");
        assert!(ours_peak <= -0.95, "limiter ceiling must hold <= -0.95 dBFS: {ours_peak:.3}");
        // Pause tail (final 0.3 s, well past linear decay + hold + release) is gated silent in both.
        assert!(peak(&l[62 * r / 10..]) < 1e-5, "pause tail must be gated silent");
        assert!(peak(&obs[62 * r / 10..]) < 1e-5, "OBS ref pause tail must be gated silent");
    }

    #[test]
    fn noise_gate_held_accumulator_stays_bounded_over_hours_of_silence() {
        let mut f = AudioFilter::new(
            AudioEffect::NoiseGate {
                close_db: -40.0,
                open_db: -30.0,
                attack_ms: 10.0,
                hold_ms: 200.0,
                release_ms: 100.0,
            },
            48_000,
        );
        // Simulate 100 seconds of silent blocks (~4.8M samples): `held` must stay clamped near hold + 1s.
        let mut l = vec![0.0f32; 1024];
        let mut r = vec![0.0f32; 1024];
        for _ in 0..4_700 {
            f.process([&mut l, &mut r]);
        }
        assert!(f.held <= 1.25, "held accumulator stayed bounded: {}", f.held);
    }
}
