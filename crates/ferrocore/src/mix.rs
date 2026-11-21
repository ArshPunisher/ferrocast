//! The audio mixer (ENGINE.md E3): sources push sound with timestamps (nanoseconds, the same
//! clock as video), at any rate; the mixer turns each to the output rate, places it by its time
//! (jitter under 70 ms smoothed over, as libobs does), applies volume, balance (sine law, as
//! libobs), mono and sync offset, and mixes blocks of 1024 frames into up to 6 tracks. Each
//! source's levels (peak, RMS) are measured after its volume, as OBS's meters show them.
//!
//! The caller drives the clock: `mix_until(now)` makes every block that ends `latency` before
//! `now`, so sound that arrives a little late still makes its block.

use std::collections::VecDeque;

use crate::afx::{AudioEffect, AudioFilter};
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};

/// Frames per block (libobs's `AUDIO_OUTPUT_FRAMES`).
pub const BLOCK: usize = 1024;

/// A volume, fader or mute change ramps over this long (the same 8 ms as the filters' parameters).
const GAIN_RAMP_S: f32 = 0.008;
/// Tracks (libobs's `MAX_AUDIO_MIXES`).
pub const TRACKS: usize = 6;
/// Timestamps closer than this to where a source's sound would continue are taken as continuing
/// (libobs's `TS_SMOOTHING_THRESHOLD`).
const SMOOTHING_NS: u64 = 70_000_000;
/// The longest gap ahead filled with silence; beyond it, the queue starts again.
const MAX_GAP_NS: u64 = 2_000_000_000;
/// The most the mixer waits for late sound (libobs's own limit is near a second).
const MAX_LATENCY_NS: u64 = 1_000_000_000;
/// Room added over the lateness seen when the mixer waits longer.
const LATENCY_ROOM_NS: u64 = 20_000_000;
/// Sound later than this is one source's trouble, not the mix's: that source is timed by when
/// its sound comes (19 Nov: a USB capture card's sound came over a second late, the mixer
/// waited a second for it, and every source, the mic too, came out late and broken).
const RETIME_NS: u64 = 300_000_000;
/// Drift tracking parameters:
/// - `PHASE_AVERAGE_S`: 2 s EMA length for residual placement phase (`ts - expected`) so burst
///   jitter averages out.
/// - `REG_TAU_S` / `MIN_REG_S`: EWMA OLS window (20 s, exact unweighted OLS while `< 20 s`) and
///   minimum continuous span (10 s) before trusting a slope estimate.
/// - `MIN_DRIFT_PPM` / `FOLLOW_SKEW_S`: minimum estimated rate offset (40 ppm) and accumulated
///   skew (4 ms) before engaging the resampler, so 0-ppm bursty devices never pay resampler CPU.
/// - `PHASE_TRIM_S` / `MAX_DRIFT`: pulls residual phase error to 0 over ~10 s, clamped to ±0.2%.
const PHASE_AVERAGE_S: f64 = 2.0;
const REG_TAU_S: f64 = 20.0;
const MIN_REG_S: f64 = 10.0;
const MIN_DRIFT_PPM: f64 = 40e-6;
const FOLLOW_SKEW_S: f64 = 0.004;
const PHASE_TRIM_S: f64 = 10.0;
const MAX_DRIFT: f64 = 0.002;
/// Frames faded out and in where a source's sample rate changes (2 ms at 48 kHz).
const SWITCH_FADE: usize = 96;
/// The most the mix catches up on in one call (about 26 MB of blocks): a longer gap is a clock
/// jump (sleep), and the mix starts again from now. A stall of a few seconds is still mixed.
const MAX_CATCH_UP_NS: u64 = 10_000_000_000;
/// After this long with sound on time, the mixer waits only as long as the latest sound in it
/// needed (a moment of trouble no longer keeps everything late for the rest of the stream).
const SHRINK_AFTER_NS: u64 = 20_000_000_000;
/// A source never holds more than this much sound waiting (a stalled mixer can't grow memory).
const MAX_QUEUE_SECS: usize = 5;
/// Input frames the resampler takes at a time.
const RESAMPLE_CHUNK: usize = 480;

/// Frames of room in a source's drift tail: half a second at 48 kHz.
const TAIL_ROOM: usize = 24_000;

/// Whether a source is heard here (OBS's Audio Monitoring).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Monitoring {
    #[default]
    Off,
    /// Heard here only: left out of the tracks.
    Only,
    /// Heard here and sent.
    AndOutput,
}

/// One source's settings, as OBS's mixer shows them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceSettings {
    /// Linear gain (1 is as is; OBS's fader in dB is `10^(dB/20)`).
    pub volume: f32,
    pub muted: bool,
    /// 0 is all left, 0.5 the middle, 1 all right.
    pub balance: f32,
    /// Both sides get the average (OBS's "Mono").
    pub mono: bool,
    /// Plays this much later (positive) or earlier (OBS's "Sync Offset").
    pub sync_offset_ns: i64,
    /// Which tracks it's in: bit 0 is track 1.
    pub tracks: u8,
    pub monitoring: Monitoring,
    /// Push-to-talk (OBS): the sound is muted unless the key is held, and for this many ms after
    /// it is let go. None: off.
    pub push_to_talk: Option<u32>,
    /// Push-to-mute (OBS): the sound is muted while the key is held, and for this many ms after.
    pub push_to_mute: Option<u32>,
}

impl Default for SourceSettings {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
            balance: 0.5,
            mono: false,
            sync_offset_ns: 0,
            tracks: 1,
            monitoring: Monitoring::Off,
            push_to_talk: None,
            push_to_mute: None,
        }
    }
}

/// A source's levels per channel (left, right), in dBFS (-inf is silence): over the last block
/// (what the ducker hears), or over [`METER_BLOCKS`] for a meter ([`Mixer::levels`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Levels {
    pub peak: [f32; 2],
    pub rms: [f32; 2],
    /// The highest the sound came in at, before its filters and its fader (a noise gate's
    /// threshold is set against this: the meter after the gate shows nothing while it's shut).
    pub input_peak: [f32; 2],
    /// Muted by a push-to-talk or push-to-mute key right now (the meter shows it dim).
    pub gated: bool,
}

impl Default for Levels {
    fn default() -> Self {
        Self {
            peak: [f32::NEG_INFINITY; 2],
            rms: [f32::NEG_INFINITY; 2],
            input_peak: [f32::NEG_INFINITY; 2],
            gated: false,
        }
    }
}

/// Blocks a meter reads over (about 300 ms): a meter read ten times a second saw one block in
/// five, so peaks between reads never showed and the bars jumped (18 Nov).
pub const METER_BLOCKS: usize = 14;

/// Blocks the fast meter reads over (about 43 ms): the dashboard's bars, polled at 30 Hz, see
/// every block (a window shorter than the poll gap would miss peaks) and draw the release
/// themselves, so the engine's 300 ms window no longer smears them.
pub const METER_FAST_BLOCKS: usize = 2;

/// One block's linear measures, kept for meters.
#[derive(Debug, Clone, Copy, Default)]
struct Measured {
    peak: [f32; 2],
    sq: [f32; 2],
    input: [f32; 2],
}

/// One mixed block: its start time and each track's interleaved stereo frames.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub ts_ns: u64,
    pub tracks: Vec<Vec<f32>>,
    /// What's heard here: the monitored sources mixed (interleaved stereo).
    pub monitor: Vec<f32>,
}

/// Marks a source capturing an audio output device (e.g. Desktop Audio / Screen loopback) so
/// the mixer can suppress self-monitoring (`OBS_SOURCE_DO_NOT_SELF_MONITOR`) and deduplicate
/// monitored sources (`should_silence_monitored_source`) when it shares the monitoring device
/// and does not already exclude Ferrocast's own playback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputCapture {
    pub device: String,
    pub excludes_self: bool,
}

/// Resampler for one source: `SincFixedIn` with reusable planar scratch buffers and exact
/// delay/warmup accounting so engaging drift following or resetting never injects a zero-pad click.
struct SourceResampler {
    in_rate: u32,
    out_rate: u32,
    sinc_len: usize,
    rs: SincFixedIn<f32>,
    pending: [Vec<f32>; 2],
    in_scratch: [Vec<f32>; 2],
    out_scratch: [Vec<f32>; 2],
    primed: bool,
    skip_out: usize,
    /// Room for what `flush_into` collects (so it makes nothing).
    flush_buf: [Vec<f32>; 2],
}

impl SourceResampler {
    fn make_sinc(in_rate: u32, out_rate: u32) -> Option<(usize, SincFixedIn<f32>)> {
        // Same-rate drift tracking only shifts rate by <= 0.2%, so a cache-resident 32-tap
        // linear-interpolated sinc filter achieves >92 dB SNR at 15 kHz at ~1/8 the CPU cost
        // of the 128-tap cubic filter used for arbitrary rate conversions (e.g. 44.1 -> 48 kHz).
        let (sinc_len, oversampling, interpolation) = if in_rate == out_rate {
            (32, 256, SincInterpolationType::Linear)
        } else {
            (128, 128, SincInterpolationType::Cubic)
        };
        let params = SincInterpolationParameters {
            sinc_len,
            f_cutoff: 0.95,
            oversampling_factor: oversampling,
            interpolation,
            window: WindowFunction::BlackmanHarris2,
        };
        let rs =
            SincFixedIn::<f32>::new(f64::from(out_rate) / f64::from(in_rate), 1.1, params, RESAMPLE_CHUNK, 2).ok()?;
        Some((sinc_len, rs))
    }

    fn new_cold(in_rate: u32, out_rate: u32) -> Option<Self> {
        let (sinc_len, rs) = Self::make_sinc(in_rate, out_rate)?;
        let out_max = rs.output_frames_max();
        Some(Self {
            in_rate,
            out_rate,
            sinc_len,
            rs,
            // Room for a long piece after a stall (drained a chunk at a time).
            pending: [Vec::with_capacity(TAIL_ROOM), Vec::with_capacity(TAIL_ROOM)],
            in_scratch: [vec![0.0; RESAMPLE_CHUNK], vec![0.0; RESAMPLE_CHUNK]],
            out_scratch: [vec![0.0; out_max], vec![0.0; out_max]],
            primed: false,
            skip_out: 0,
            flush_buf: [Vec::with_capacity(out_max * 4), Vec::with_capacity(out_max * 4)],
        })
    }

    /// Runs the already-filled `in_scratch` through a fresh `1:1` resampler so its internal
    /// lookahead matches `queue`'s end, then trims the overlapping tail from `queue`.
    fn prime_from_scratch(&mut self, queue: &mut VecDeque<f32>) -> bool {
        if let Ok((_, w_out)) = self.rs.process_into_buffer(&self.in_scratch, &mut self.out_scratch, None) {
            // SincFixedIn starts last_index at -(sinc_len / 2), so w_out covers input 0..w_out
            // and the remaining (RESAMPLE_CHUNK - w_out) frames are buffered inside `rs`.
            let overlap = RESAMPLE_CHUNK.saturating_sub(w_out);
            let queued = queue.len() / 2;
            if queued >= overlap {
                queue.truncate((queued - overlap) * 2);
                self.skip_out = 0;
            } else {
                queue.clear();
                self.skip_out = overlap - queued;
            }
            self.primed = true;
            return true;
        }
        false
    }

    /// Primes a newly created same-rate resampler with the last `RESAMPLE_CHUNK` frames already
    /// pushed to `queue`, trimming the overlapping tail from `queue` so output is continuous.
    fn new_warm(in_rate: u32, out_rate: u32, tail: &[Vec<f32>; 2], queue: &mut VecDeque<f32>) -> Option<Self> {
        let mut s = Self::new_cold(in_rate, out_rate)?;
        s.prime_warm(tail, queue);
        Some(s)
    }

    /// As [`Self::new_warm`] for one made already (no allocation here).
    fn prime_warm(&mut self, tail: &[Vec<f32>; 2], queue: &mut VecDeque<f32>) {
        if self.in_rate == self.out_rate && tail[0].len() >= RESAMPLE_CHUNK {
            let start = tail[0].len() - RESAMPLE_CHUNK;
            self.in_scratch[0].copy_from_slice(&tail[0][start..]);
            self.in_scratch[1].copy_from_slice(&tail[1][start..]);
            self.prime_from_scratch(queue);
        }
    }

    fn reset(&mut self, queue: &mut VecDeque<f32>) {
        self.rs.reset();
        self.pending[0].clear();
        self.pending[1].clear();
        let queued = queue.len() / 2;
        if self.in_rate == self.out_rate && queued >= RESAMPLE_CHUNK {
            let base = (queued - RESAMPLE_CHUNK) * 2;
            for i in 0..RESAMPLE_CHUNK {
                self.in_scratch[0][i] = queue[base + i * 2];
                self.in_scratch[1][i] = queue[base + i * 2 + 1];
            }
            if self.prime_from_scratch(queue) {
                return;
            }
        }
        self.primed = false;
        self.skip_out = 0;
    }

    fn effective_ratio(&self, drift_ratio: f64) -> f64 {
        (f64::from(self.out_rate) / f64::from(self.in_rate)) * drift_ratio
    }

    fn held_in_frames(&self, ratio: f64) -> f64 {
        let rs_in = if self.primed {
            // SincFixedIn stops when idx >= chunk - (sinc_len + 1) - ceil(1/r); center is idx + sinc_len/2.
            (self.sinc_len / 2 + 1) as f64 + (1.0 / ratio).ceil() - 0.5 / ratio
        } else {
            0.0
        };
        self.pending[0].len() as f64 + rs_in
    }

    /// Output-equivalent frames currently held in `pending` plus inside `rs`'s delay window.
    fn held_out_frames(&self, drift_ratio: f64) -> f64 {
        let r = self.effective_ratio(drift_ratio);
        (self.held_in_frames(r) * r - self.skip_out as f64).max(0.0)
    }

    fn process_ready(&mut self, queue: &mut VecDeque<f32>, follows: bool, drift_ratio: f64) {
        while self.pending[0].len() >= RESAMPLE_CHUNK {
            if follows {
                let _ = self.rs.set_resample_ratio_relative(drift_ratio, true);
            }
            self.in_scratch[0].copy_from_slice(&self.pending[0][..RESAMPLE_CHUNK]);
            self.in_scratch[1].copy_from_slice(&self.pending[1][..RESAMPLE_CHUNK]);
            self.pending[0].drain(..RESAMPLE_CHUNK);
            self.pending[1].drain(..RESAMPLE_CHUNK);
            if let Ok((_, n_out)) = self.rs.process_into_buffer(&self.in_scratch, &mut self.out_scratch, None) {
                self.primed = true;
                let skip = self.skip_out.min(n_out);
                self.skip_out -= skip;
                for i in skip..n_out {
                    queue.push_back(self.out_scratch[0][i]);
                    queue.push_back(self.out_scratch[1][i]);
                }
            }
        }
    }

    /// Plays out what it still holds into `queue` (the resampler is kept, to be dropped
    /// elsewhere). Nothing is allocated: input is zero-padded in `in_scratch` and the output
    /// lands in `out_scratch` and `flush_buf`.
    fn flush_into(&mut self, queue: &mut VecDeque<f32>, drift_ratio: f64) {
        let r = self.effective_ratio(drift_ratio);
        let owed = (self.held_in_frames(r) * r).round() as usize;
        let mut out = std::mem::take(&mut self.flush_buf);
        out[0].clear();
        out[1].clear();
        let mut first = !self.pending[0].is_empty();
        for _ in 0..16 {
            if !first && out[0].len() >= owed + self.skip_out {
                break;
            }
            for ch in 0..2 {
                let n = if first { self.pending[ch].len().min(RESAMPLE_CHUNK) } else { 0 };
                self.in_scratch[ch][..n].copy_from_slice(&self.pending[ch][..n]);
                self.in_scratch[ch][n..].fill(0.0);
            }
            let n_out = match self.rs.process_into_buffer(&self.in_scratch, &mut self.out_scratch, None) {
                Ok((_, n)) => n,
                Err(_) => 0,
            };
            if n_out == 0 && !first {
                break;
            }
            out[0].extend_from_slice(&self.out_scratch[0][..n_out]);
            out[1].extend_from_slice(&self.out_scratch[1][..n_out]);
            first = false;
        }
        let skip = self.skip_out.min(out[0].len());
        for (a, b) in out[0][skip..].iter().zip(&out[1][skip..]).take(owed) {
            queue.push_back(*a);
            queue.push_back(*b);
        }
        self.pending[0].clear();
        self.pending[1].clear();
        self.flush_buf = out;
    }
}

/// Adaptive clock-drift state: online rate regression from stamps vs device frames plus a
/// phase trim loop that pulls steady-state placement error to zero without pitch wobble.
struct DriftTrack {
    seg_ts0: Option<u64>,
    seg_dev_s: f64,
    mx: f64,
    my: f64,
    vxx: f64,
    vxy: f64,
    slope: f64,
    phase_err_ns: f64,
    ratio: f64,
    follows: bool,
    tail: [Vec<f32>; 2],
}

impl Default for DriftTrack {
    fn default() -> Self {
        Self {
            seg_ts0: None,
            seg_dev_s: 0.0,
            mx: 0.0,
            my: 0.0,
            vxx: 0.0,
            vxy: 0.0,
            slope: 0.0,
            phase_err_ns: 0.0,
            ratio: 1.0,
            follows: false,
            // Room for a long piece after a stall (it is trimmed to RESAMPLE_CHUNK after each).
            tail: [Vec::with_capacity(TAIL_ROOM), Vec::with_capacity(TAIL_ROOM)],
        }
    }
}

impl DriftTrack {
    fn reset_segment(&mut self) {
        self.seg_ts0 = None;
        self.seg_dev_s = 0.0;
        self.mx = 0.0;
        self.my = 0.0;
        self.vxx = 0.0;
        self.vxy = 0.0;
        self.phase_err_ns = 0.0;
        if self.follows {
            self.ratio = (1.0 + self.slope).clamp(1.0 - MAX_DRIFT, 1.0 + MAX_DRIFT);
        }
    }

    fn observe(&mut self, id: u64, ts: u64, expected: u64, piece_s: f64, can_warm: bool) {
        // 1. Update placement phase EMA (`ts - expected`).
        let err = ts as f64 - expected as f64;
        let a_phase = (piece_s / PHASE_AVERAGE_S).min(1.0);
        self.phase_err_ns += (err - self.phase_err_ns) * a_phase;

        // 2. Update Welford-EWMA OLS of clock skew `y = (ts - seg_ts0)/1e9 - dev_s` vs `x = dev_s`.
        let ts0 = *self.seg_ts0.get_or_insert(ts);
        let x = self.seg_dev_s;
        let y = (ts as f64 - ts0 as f64) / 1e9 - x;
        let next_dev_s = x + piece_s;
        let w = (piece_s / next_dev_s.min(REG_TAU_S)).min(1.0);
        let dx = x - self.mx;
        let dy = y - self.my;
        self.mx += w * dx;
        self.my += w * dy;
        self.vxx = (1.0 - w) * (self.vxx + w * dx * dx);
        self.vxy = (1.0 - w) * (self.vxy + w * dx * dy);
        self.seg_dev_s = next_dev_s;

        if self.seg_dev_s >= MIN_REG_S && self.vxx >= 4.0 {
            let est = (self.vxy / self.vxx).clamp(-MAX_DRIFT, MAX_DRIFT);
            self.slope = est;
            if !self.follows
                && can_warm
                && est.abs() >= MIN_DRIFT_PPM
                && ((est * self.seg_dev_s).abs() > FOLLOW_SKEW_S || self.phase_err_ns.abs() > 5_000_000.0)
            {
                self.follows = true;
                tracing::info!(
                    source = id,
                    drift_ppm = -(est * 1e6).round(),
                    off_ms = self.phase_err_ns / 1e6,
                    "a source's clock drifts: the mixer follows it"
                );
            }
        }
        if self.follows {
            self.ratio =
                (1.0 + self.slope + self.phase_err_ns / 1e9 / PHASE_TRIM_S).clamp(1.0 - MAX_DRIFT, 1.0 + MAX_DRIFT);
        }
    }

    fn push_tail(&mut self, l: &[f32], r: &[f32]) {
        if self.follows {
            return;
        }
        self.tail[0].extend_from_slice(l);
        self.tail[1].extend_from_slice(r);
        if self.tail[0].len() > RESAMPLE_CHUNK {
            let drop_n = self.tail[0].len() - RESAMPLE_CHUNK;
            self.tail[0].drain(..drop_n);
            self.tail[1].drain(..drop_n);
        }
    }
}

struct Source {
    settings: SourceSettings,
    output_capture: Option<OutputCapture>,
    active: bool,
    /// Sound waiting, interleaved stereo at the mixer's rate, starting at `start_ns`.
    queue: VecDeque<f32>,
    start_ns: Option<u64>,
    /// Turning its rate into the mixer's, when they differ or when following clock drift.
    resampler: Option<SourceResampler>,
    /// Its filters, in order (after mono and balance, before volume, as OBS runs them).
    filters: Vec<AudioFilter>,
    /// Pre-resolved sidechain key routing per filter (parallel to `filters`), updated on topology edits.
    key_routes: Vec<KeyRoute>,
    /// Pre-allocated key buffer slot in `Mixer::key_bufs` when another source ducks under this one.
    key_slot: Option<usize>,
    /// Last post-filter (pre-volume) sample, and whether a structural filter add/remove/reorder
    /// armed a 4 ms raised-cosine step de-click so toggling filters live never pops.
    last_post_filter: [f32; 2],
    filter_declick: bool,
    /// The gain (volume or mute) the last block ended on; a change ramps from it. None: the
    /// source's first block, which starts at its gain.
    gain_applied: Option<f32>,
    /// Push-to-talk / push-to-mute: the keys held, and the samples each stays in effect after its
    /// key was let go (the release delay, counted down per block).
    ptt_down: bool,
    ptm_down: bool,
    ptt_hold: u32,
    ptm_hold: u32,
    /// Muted by one of them as of the last block.
    gated: bool,
    levels: Levels,
    /// The last [`METER_BLOCKS`] blocks' measures, newest last.
    recent: VecDeque<Measured>,
    /// True peak's oversampling memory (the last samples of each side).
    tp: [[f32; TP_TAPS]; 2],
    /// Added to its times: a source whose sound came later than [`RETIME_NS`] is timed by when
    /// it comes from then on, so it never holds the whole mix back (libobs's timing_adjust).
    adjust_ns: u64,
    /// The offset its times were placed with last (sync offset less what its filters hold
    /// back): a change starts its timing again, as libobs does on a sync-offset change, else a
    /// change under the smoothing (70 ms) was smoothed away and did nothing.
    offset_ns: Option<i64>,
    drift: DriftTrack,
    /// While a filter that holds sound back is priming (its first output is silence), the dry
    /// sound it would have replaced (interleaved, from `start_ns`): played in its place, then
    /// eased into the filter's output. `bypass_pure` frames are dry alone, the rest overlap the
    /// filter's first sound; `bypass_done` counts frames played.
    bypass: VecDeque<f32>,
    bypass_pure: usize,
    bypass_done: usize,
    /// What the queue didn't have yet when the filter went on: the next frames pushed go to the
    /// bypass (`bypass_owed`), then a few more go to both (`ease_owed`, the overlap to ease over).
    bypass_owed: usize,
    ease_owed: usize,
    /// Sound of another rate that came before its resampler did, as it came (interleaved, at
    /// `hold_rate`, `hold_ch` channels, the first piece stamped `hold_ts`): run through the
    /// resampler when it is here, so the sound starts complete.
    hold: Vec<f32>,
    hold_ts: u64,
    hold_rate: u32,
    hold_ch: usize,
}

impl Source {
    fn clear_hold(&mut self) {
        self.hold.clear();
        self.hold_ts = 0;
        self.hold_rate = 0;
        self.hold_ch = 0;
    }
}

/// The longest filter delay a dry bypass covers (frames); the strong noise removal holds 3968.
const MAX_BYPASS_FRAMES: usize = 8192;
/// The ease from dry to the filter's own sound, in seconds.
const BYPASS_EASE_S: f64 = 0.0045;

/// Taps of each phase of the true-peak oversampler.
const TP_TAPS: usize = 12;

/// The 4x oversampler's phases (a windowed sinc, as ITU-R BS.1770's true peak asks): phase k
/// gives the point k/4 of the way past the newest-but-five sample.
fn tp_phases() -> &'static [[f32; TP_TAPS]; 4] {
    static P: std::sync::OnceLock<[[f32; TP_TAPS]; 4]> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let mut p = [[0f32; TP_TAPS]; 4];
        let mid = (TP_TAPS / 2 - 1) as f32;
        let half = (TP_TAPS / 2) as f32;
        for (k, ph) in p.iter_mut().enumerate() {
            let frac = k as f32 / 4.0;
            let mut sum = 0.0;
            for (j, c) in ph.iter_mut().enumerate() {
                let x = j as f32 - mid - frac;
                let sinc =
                    if x.abs() < 1e-6 { 1.0 } else { (std::f32::consts::PI * x).sin() / (std::f32::consts::PI * x) };
                // A Hann window centred on the point asked for (peer: it sat half a tap off).
                let w = if x.abs() < half { 0.5 + 0.5 * (std::f32::consts::PI * x / half).cos() } else { 0.0 };
                *c = sinc * w;
                sum += *c;
            }
            for c in ph.iter_mut() {
                *c /= sum;
            }
        }
        p
    })
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source").field("settings", &self.settings).field("queued", &(self.queue.len() / 2)).finish()
    }
}

/// Outcome of [`Mixer::attach_plugins`].
#[derive(Debug, Default)]
pub struct AttachOutcome {
    /// Plugins that became stale or incompatible while building, to drop off-thread.
    pub rejected: Vec<Box<dyn crate::afx::SoundPlugin>>,
    /// Filters whose live settings changed incompatibly with retune and need a fresh build.
    pub rebuild: Vec<(u64, AudioEffect)>,
}

/// The mixer.
#[derive(Debug)]
pub struct Mixer {
    rate: u32,
    latency_ns: u64,
    /// The wait it started with: it never goes under this.
    floor_ns: u64,
    /// The wait the last `mix_until` used: lateness is measured against it, so a burst of late
    /// pieces asks for the wait once, not once each (15 Nov: 40 ms grew to a second in 2 s).
    mixed_with_ns: u64,
    /// The latest any sound came since `window_ns` (ns after its time), and when that began.
    worst_late_ns: u64,
    window_ns: Option<u64>,
    sources: crate::rt::PreMap<Box<Source>>,
    /// Entries made ahead for sources that appear, and requests used/in-flight.
    spares: Vec<SpareSource>,
    spares_wanted: usize,
    spares_pending: usize,
    /// Resamplers made ahead for a source that starts following drift, and how many to make.
    rs_spares: Vec<SpareResampler>,
    /// Input rates to make a resampler for, the ones asked for and not here yet, and failed rates.
    rs_wanted: Vec<u32>,
    rs_pending: Vec<u32>,
    rs_failed: Vec<u32>,
    /// Another rate's resampler comes from a background thread (the audio thread's mixer); else
    /// it is made on the spot (a mixer with no one to ask).
    async_resamplers: bool,
    /// Resamplers a source no longer uses, to be dropped elsewhere.
    rs_retired: Vec<SpareResampler>,
    /// Entries of removed sources, waiting to be dropped elsewhere.
    #[allow(clippy::vec_box)] // boxed as they were in the map: moving one out must not allocate
    retired: Vec<Box<Source>>,
    /// The next block's start; set by the first `mix_until`.
    next_ns: Option<u64>,
    /// Listeners to one source's sound (captions), by source; gone when their tap is dropped.
    taps: Vec<(u64, std::sync::Weak<std::sync::Mutex<std::collections::VecDeque<f32>>>)>,
    /// Music quieter while the voices talk, and each track steered to a loudness.
    ducker: Option<crate::level::Ducker>,
    loudness: Option<Vec<crate::level::Loudness>>,
    loudness_target: Option<f32>,
    /// Meters read true peak (between samples) instead of sample peak (OBS's Peak Meter Type).
    true_peak: bool,
    /// Low latency sound (Settings): the wait starts at 20 ms and never goes past 60 ms plus
    /// what a source's filters hold its sound back by (a late device's sound is cut instead).
    low_latency: bool,
    /// The wait it starts with in the usual mode.
    usual_floor_ns: u64,
    /// The active monitoring output device id (`None` when monitoring is off or disconnected).
    monitor_device: Option<String>,
    /// Working space kept between blocks, so a steady mix allocates nothing (D1).
    scratch: Scratch,
    /// Topologically sorted source IDs for `mix_block` (key sources before dependents).
    mix_order: Vec<u64>,
    /// Pre-allocated planar stereo post-filter, pre-fader key buffers for active sidechain targets.
    key_bufs: Vec<Box<([f32; BLOCK], [f32; BLOCK])>>,
    /// Sidechain dependency edges `(dependent, key)` severed to break cycles.
    broken_cycles: Vec<(u64, u64)>,
    /// Reusable topology planner scratch buffers (zero steady-state allocs).
    plan_scratch: PlanScratch,
    /// Reusable effect-diff scratch buffers (zero steady-state allocs).
    effect_scratch: EffectScratch,
    /// Blocks given back by [`Mixer::recycle`], made again instead of allocated.
    pool: Vec<Block>,
    /// Blocks allocated new (not found in the pool) so far.
    allocated: usize,
}

#[derive(Debug, Default)]
struct EffectScratch {
    old: Vec<(usize, Option<AudioFilter>)>,
    made: Vec<Option<(usize, AudioFilter)>>,
}

impl EffectScratch {
    fn with_room() -> Self {
        Self { old: Vec::with_capacity(16), made: Vec::with_capacity(16) }
    }
}

/// Silence key fed to a sidechained compressor whose key source is missing, muted, or cycle-broken.
static SILENCE_KEY: [f32; BLOCK] = [0.0; BLOCK];

/// Pre-resolved sidechain routing for one filter in a source's chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyRoute {
    /// Normal self-keyed filter.
    SelfKey,
    /// Sidechained compressor reading `Mixer::key_bufs[slot]`.
    Slot(usize),
    /// Sidechained compressor whose key source is missing, self, or cycle-broken: feed silence.
    Silence,
}

/// Reusable topological-sort scratch buffers for [`resolve_sidechain_order`].
#[derive(Debug, Default)]
pub(crate) struct TopoScratch {
    in_deg: Vec<usize>,
    placed: Vec<bool>,
    edges: Vec<(usize, usize, bool)>,
    visit_step: Vec<usize>,
    path: Vec<usize>,
}

/// Reusable scratch buffers for `Mixer::rebuild_mix_plan`.
#[derive(Debug, Default)]
struct PlanScratch {
    nodes: Vec<u64>,
    deps: Vec<(u64, u64)>,
    prev_broken: Vec<(u64, u64)>,
    slot_by_node: Vec<Option<usize>>,
    topo: TopoScratch,
}

impl PlanScratch {
    /// Room for this many sources, so adding one doesn't allocate.
    fn with_room() -> Self {
        let n = crate::rt::PreMap::<Box<Source>>::ROOM;
        Self {
            nodes: Vec::with_capacity(n),
            deps: Vec::with_capacity(n),
            prev_broken: Vec::with_capacity(16),
            slot_by_node: Vec::with_capacity(n),
            topo: TopoScratch::with_room(n),
        }
    }
}

impl TopoScratch {
    fn with_room(n: usize) -> Self {
        Self {
            in_deg: Vec::with_capacity(n),
            placed: Vec::with_capacity(n),
            edges: Vec::with_capacity(n),
            visit_step: Vec::with_capacity(n),
            path: Vec::with_capacity(n),
        }
    }
}

/// Resolves a deterministic topological processing order (dependencies before dependents) over
/// sorted unique `nodes` and directed dependency edges `(dependent, dependency)`.
///
/// Self-loops `(u, u)` and multi-node cycles are detected and broken deterministically by severing
/// the cycle-closing edge on the highest-ID node in each cycle. Edges targeting a missing node
/// (not in `nodes`) are ignored here and routed to silence by the caller.
pub(crate) fn resolve_sidechain_order(
    nodes: &[u64],
    deps: &[(u64, u64)],
    order: &mut Vec<u64>,
    broken: &mut Vec<(u64, u64)>,
    scratch: &mut TopoScratch,
) {
    order.clear();
    broken.clear();
    let n = nodes.len();
    if n == 0 {
        return;
    }
    if deps.is_empty() {
        order.extend_from_slice(nodes);
        return;
    }
    scratch.in_deg.clear();
    scratch.in_deg.resize(n, 0);
    scratch.placed.clear();
    scratch.placed.resize(n, false);
    scratch.edges.clear();
    for &(u, v) in deps {
        let Ok(ui) = nodes.binary_search(&u) else { continue };
        if u == v {
            broken.push((u, v));
            continue;
        }
        let Ok(vi) = nodes.binary_search(&v) else { continue };
        if !scratch.edges.iter().any(|&(eu, ev, _)| eu == ui && ev == vi) {
            scratch.edges.push((ui, vi, true));
            scratch.in_deg[ui] += 1;
        }
    }
    if scratch.edges.is_empty() {
        order.extend_from_slice(nodes);
        broken.sort_unstable();
        broken.dedup();
        return;
    }
    scratch.visit_step.clear();
    scratch.visit_step.resize(n, 0);
    while order.len() < n {
        let mut progress = true;
        while progress {
            progress = false;
            for (i, &node) in nodes.iter().enumerate() {
                if !scratch.placed[i] && scratch.in_deg[i] == 0 {
                    scratch.placed[i] = true;
                    order.push(node);
                    progress = true;
                    for &(eu, ev, active) in &scratch.edges {
                        if active && ev == i {
                            scratch.in_deg[eu] -= 1;
                        }
                    }
                }
            }
        }
        if order.len() == n {
            break;
        }
        // Every remaining unplaced node has `in_deg > 0`, so walking active unplaced dependency
        // edges is guaranteed to revisit a node on the path and isolate a true cycle without
        // disturbing downstream acyclic dependents.
        let Some(mut cur) = (0..n).find(|&i| !scratch.placed[i]) else { break };
        scratch.visit_step.fill(0);
        scratch.path.clear();
        let cycle_start = loop {
            if scratch.visit_step[cur] != 0 {
                break scratch.visit_step[cur] - 1;
            }
            scratch.path.push(cur);
            scratch.visit_step[cur] = scratch.path.len();
            let next = scratch
                .edges
                .iter()
                .filter(|&&(eu, ev, active)| active && eu == cur && !scratch.placed[ev])
                .map(|&(_, ev, _)| ev)
                .min();
            match next {
                Some(v) => cur = v,
                None => break 0,
            }
        };
        let cycle = &scratch.path[cycle_start..];
        if cycle.is_empty() {
            break;
        }
        let best_k = (0..cycle.len()).max_by_key(|&k| nodes[cycle[k]]).unwrap_or(0);
        let u_idx = cycle[best_k];
        let v_idx = cycle[(best_k + 1) % cycle.len()];
        for (eu, ev, active) in &mut scratch.edges {
            if *active && *eu == u_idx && *ev == v_idx {
                *active = false;
                scratch.in_deg[u_idx] = scratch.in_deg[u_idx].saturating_sub(1);
                broken.push((nodes[u_idx], nodes[v_idx]));
                break;
            }
        }
    }
    broken.sort_unstable();
    broken.dedup();
}

/// Working buffers of the mixer, made once and grown only when a bigger piece comes.
#[derive(Debug, Default)]
struct Scratch {
    /// The planar copy of a pushed piece.
    l: Vec<f32>,
    r: Vec<f32>,
    /// One source's block while it is mixed, and the planar halves its filters work on.
    buf: Vec<f32>,
    fl: Vec<f32>,
    fr: Vec<f32>,
    /// The dry bypass's block while a source's filter is priming (see `Source::bypass`).
    bbuf: Vec<f32>,
}

/// How many given-back blocks are kept (the most a mix ahead of its reader needs, a second's).
const MAX_POOL: usize = 64;

/// Source entries kept made ahead, and removed ones kept for dropping elsewhere.
const SPARES: usize = 4;
const RETIRED_ROOM: usize = 64;

/// Key buffers made ahead for sidechained compressors (more are made when needed).
const KEY_BUFS_AHEAD: usize = 4;

/// Resamplers kept made ahead (same rate in and out: what following a clock's drift uses), and
/// used ones kept for dropping elsewhere.
/// Samples a source can hold while its resampler is made (half a second of stereo at 48 kHz).
const HOLD_ROOM: usize = 48_000;

const RS_SPARES: usize = 2;
/// Room for resamplers made ahead (same rate and others a source asked for).
const RS_SPARES_ROOM: usize = 8;
const RS_RETIRED_ROOM: usize = 32;

/// A resampler made ahead (its filter tables are built when it is made: not on the audio thread).
pub struct SpareResampler(SourceResampler);

impl std::fmt::Debug for SpareResampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpareResampler")
    }
}

/// A source's mixer entry made ahead of the source (see [`Mixer::make_spare`]).
pub struct SpareSource(Box<Source>);

impl std::fmt::Debug for SpareSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpareSource")
    }
}

/// Blocks made when the mixer is (a mix in steady state needs a dozen or so in flight).
const PREFILL_POOL: usize = 32;

/// Low latency sound: where the wait starts, and the most it waits beyond a filter's delay.
const LOW_FLOOR_NS: u64 = 20_000_000;
const LOW_CAP_NS: u64 = 60_000_000;

/// One source's sound as it comes in (mono, the mixer's rate, before its volume and filters),
/// for something that listens (live captions). At most `TAP_SECONDS` are kept unread; nothing
/// while the source is muted. Dropping it stops listening.
#[derive(Debug, Clone)]
pub struct Tap(pub std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<f32>>>);

/// The most a tap keeps unread.
pub const TAP_SECONDS: usize = 30;

impl Tap {
    /// Everything heard since the last take, oldest first.
    pub fn take(&self) -> Vec<f32> {
        self.0.lock().map(|mut s| s.drain(..).collect()).unwrap_or_default()
    }
}

fn block_ns(rate: u32) -> u64 {
    BLOCK as u64 * 1_000_000_000 / u64::from(rate)
}

fn frames_in(ns: u64, rate: u32) -> usize {
    (u128::from(ns) * u128::from(rate) / 1_000_000_000) as usize
}

fn db(v: f32) -> f32 {
    if v <= 0.0 { f32::NEG_INFINITY } else { 20.0 * v.log10() }
}

/// Pushes mono-summed stereo audio `(l, r)` into a tap buffer capped at `TAP_SECONDS`.
pub(crate) fn push_tap_planar(buf: &std::sync::Mutex<VecDeque<f32>>, l: &[f32], r: &[f32], rate: u32) {
    let max = TAP_SECONDS * rate as usize;
    if let Ok(mut b) = buf.lock() {
        b.extend(l.iter().zip(r).map(|(a, b)| (a + b) * 0.5));
        let over = b.len().saturating_sub(max);
        b.drain(..over);
    }
}

/// Pushes mono-summed interleaved stereo audio into a tap buffer capped at `TAP_SECONDS`.
pub(crate) fn push_tap_interleaved(buf: &std::sync::Mutex<VecDeque<f32>>, stereo: &[f32], rate: u32) {
    let max = TAP_SECONDS * rate as usize;
    if let Ok(mut b) = buf.lock() {
        b.extend(stereo.as_chunks::<2>().0.iter().map(|f| (f[0] + f[1]) * 0.5));
        let over = b.len().saturating_sub(max);
        b.drain(..over);
    }
}

/// Gives source `id`'s sound to its taps (at the mixer's rate only: every capture here gives
/// it; other rates aren't tapped) and prunes dropped taps so dead `Weak`s don't accumulate.
#[allow(clippy::type_complexity)]
fn feed_taps(
    taps: &mut Vec<(u64, std::sync::Weak<std::sync::Mutex<std::collections::VecDeque<f32>>>)>,
    id: u64,
    (l, r): (&[f32], &[f32]),
    rate: u32,
    mixer_rate: u32,
) {
    if rate != mixer_rate {
        return;
    }
    let mut any_dead = false;
    for (i, t) in taps.iter() {
        if *i == id {
            if let Some(buf) = t.upgrade() {
                push_tap_planar(&buf, l, r, mixer_rate);
            } else {
                any_dead = true;
            }
        } else if t.strong_count() == 0 {
            any_dead = true;
        }
    }
    if any_dead {
        taps.retain(|(_, t)| t.strong_count() > 0);
    }
}

/// A sample into -1..1, NaN made silent (Rust's clamp keeps NaN as NaN; libobs's
/// `clamp_audio_output` checks `val == val` for the same reason).
pub(crate) fn clip(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(-1.0, 1.0) }
}

/// After the volume: an overflow to infinity (a huge gain) becomes the loudest real sample
/// it could be, not infinity in the sum. Far above 1 on purpose: the sum is clipped later.
fn clip_wide(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else if v.is_nan() {
        0.0
    } else {
        v.signum() * 1.0e6
    }
}

/// Makes samples that aren't finite numbers silent.
fn finite(buf: &mut [f32]) {
    for v in buf {
        if !v.is_finite() {
            *v = 0.0;
        }
    }
}

impl Mixer {
    /// `rate` Hz stereo out; blocks are made `latency_ns` behind the clock.
    pub fn new(rate: u32, latency_ns: u64) -> Self {
        Self {
            rate,
            latency_ns,
            floor_ns: latency_ns,
            mixed_with_ns: latency_ns,
            worst_late_ns: 0,
            window_ns: None,
            sources: crate::rt::PreMap::new(),
            spares: (0..SPARES).map(|_| SpareSource(Self::fresh_source(rate))).collect(),
            spares_wanted: 0,
            spares_pending: 0,
            rs_spares: {
                let mut v = Vec::with_capacity(RS_SPARES_ROOM);
                v.extend((0..RS_SPARES).map(|_| SpareResampler(Self::cold_rs(rate))));
                v
            },
            rs_wanted: Vec::with_capacity(RS_SPARES_ROOM),
            rs_pending: Vec::with_capacity(RS_SPARES_ROOM),
            rs_failed: Vec::with_capacity(RS_SPARES_ROOM),
            async_resamplers: false,
            rs_retired: Vec::with_capacity(RS_RETIRED_ROOM),
            retired: Vec::with_capacity(RETIRED_ROOM),
            next_ns: None,
            taps: Vec::new(),
            ducker: None,
            loudness: None,
            loudness_target: None,
            true_peak: false,
            low_latency: false,
            usual_floor_ns: latency_ns,
            monitor_device: None,
            scratch: Scratch {
                // A second of frames: silence filled in after a stall mustn't grow them mid-mix.
                l: Vec::with_capacity(rate as usize),
                r: Vec::with_capacity(rate as usize),
                buf: Vec::with_capacity(BLOCK * 2),
                fl: Vec::with_capacity(BLOCK),
                fr: Vec::with_capacity(BLOCK),
                bbuf: Vec::with_capacity(BLOCK * 2),
            },
            mix_order: Vec::with_capacity(crate::rt::PreMap::<Box<Source>>::ROOM),
            key_bufs: {
                let mut v = Vec::with_capacity(crate::rt::PreMap::<Box<Source>>::ROOM);
                v.extend((0..KEY_BUFS_AHEAD).map(|_| Box::new(([0.0; BLOCK], [0.0; BLOCK]))));
                v
            },
            broken_cycles: Vec::new(),
            plan_scratch: PlanScratch::with_room(),
            effect_scratch: EffectScratch::with_room(),
            // Made now, not on the first blocks: a block the pool lacks is allocated mid-mix.
            pool: Self::prefilled_pool(),
            allocated: 0,
        }
    }

    /// Sidechain dependency edges `(dependent, key)` currently severed to break cycles.
    #[must_use]
    pub fn broken_sidechains(&self) -> &[(u64, u64)] {
        &self.broken_cycles
    }

    /// Recomputes topological mix order, cycle breaks, and pre-allocated key buffer slots after a
    /// source or filter topology edit. Never called during steady-state `mix_block`.
    fn rebuild_mix_plan(&mut self) {
        let mut scratch = std::mem::take(&mut self.plan_scratch);
        scratch.nodes.clear();
        scratch.deps.clear();
        scratch.prev_broken.clear();
        scratch.prev_broken.extend_from_slice(&self.broken_cycles);

        for (&id, src) in &self.sources {
            scratch.nodes.push(id);
            for f in &src.filters {
                if let AudioEffect::Compressor { sidechain: Some(key_id), .. } = f.effect {
                    scratch.deps.push((id, key_id));
                }
            }
        }
        resolve_sidechain_order(
            &scratch.nodes,
            &scratch.deps,
            &mut self.mix_order,
            &mut self.broken_cycles,
            &mut scratch.topo,
        );
        for &(u, v) in &self.broken_cycles {
            if !scratch.prev_broken.contains(&(u, v)) {
                tracing::warn!(
                    source = u,
                    sidechain = v,
                    "sidechain cycle detected and broken; compressor receives silence"
                );
            }
        }

        scratch.slot_by_node.clear();
        scratch.slot_by_node.resize(scratch.nodes.len(), None);
        let mut next_slot = 0usize;
        for (&id, src) in &self.sources {
            for f in &src.filters {
                if let AudioEffect::Compressor { sidechain: Some(key_id), .. } = f.effect
                    && !self.broken_cycles.contains(&(id, key_id))
                    && let Ok(ki) = scratch.nodes.binary_search(&key_id)
                    && scratch.slot_by_node[ki].is_none()
                {
                    scratch.slot_by_node[ki] = Some(next_slot);
                    next_slot += 1;
                }
            }
        }
        while self.key_bufs.len() < next_slot {
            self.key_bufs.push(Box::new(([0.0; BLOCK], [0.0; BLOCK])));
        }
        for (idx, &id) in scratch.nodes.iter().enumerate() {
            let Some(src) = self.sources.get_mut(&id) else { continue };
            src.key_slot = scratch.slot_by_node[idx];
            src.key_routes.clear();
            src.key_routes.reserve(src.filters.len());
            for f in &src.filters {
                let route = match f.effect {
                    AudioEffect::Compressor { sidechain: Some(key_id), .. } => {
                        if self.broken_cycles.contains(&(id, key_id)) {
                            KeyRoute::Silence
                        } else if let Ok(ki) = scratch.nodes.binary_search(&key_id)
                            && let Some(slot) = scratch.slot_by_node[ki]
                        {
                            KeyRoute::Slot(slot)
                        } else {
                            KeyRoute::Silence
                        }
                    }
                    _ => KeyRoute::SelfKey,
                };
                src.key_routes.push(route);
            }
        }
        self.plan_scratch = scratch;
    }

    /// Gives a block back once it has been used, to be made again by a later mix. Blocks of
    /// another shape, and those beyond [`MAX_POOL`], are simply dropped.
    pub fn recycle(&mut self, b: Block) {
        let whole =
            b.tracks.len() == TRACKS && b.tracks.iter().all(|t| t.len() == BLOCK * 2) && b.monitor.len() == BLOCK * 2;
        if whole && self.pool.len() < MAX_POOL {
            self.pool.push(b);
        }
    }

    /// How many blocks were allocated new, not taken from the pool (a steady mix that gives them
    /// back stops adding to it).
    #[must_use]
    pub fn blocks_allocated(&self) -> usize {
        self.allocated
    }

    /// A silent block: from the pool when there is one.
    fn take_block(&mut self, ts_ns: u64) -> Block {
        match self.pool.pop() {
            Some(mut b) => {
                b.ts_ns = ts_ns;
                for t in &mut b.tracks {
                    t.fill(0.0);
                }
                b.monitor.fill(0.0);
                b
            }
            None => {
                self.allocated += 1;
                Self::new_block(ts_ns)
            }
        }
    }

    fn prefilled_pool() -> Vec<Block> {
        let mut pool = Vec::with_capacity(MAX_POOL);
        pool.extend((0..PREFILL_POOL).map(|_| Self::new_block(0)));
        pool
    }

    fn new_block(ts_ns: u64) -> Block {
        Block { ts_ns, tracks: vec![vec![0f32; BLOCK * 2]; TRACKS], monitor: vec![0f32; BLOCK * 2] }
    }

    /// Tells the mixer which output device monitoring currently plays on (`None` when off or
    /// disconnected), so non-self-excluding loopback sources can avoid howling and double audio.
    pub fn set_monitor_device(&mut self, device: Option<String>) {
        self.monitor_device = device;
    }

    /// Replaces the monitoring device id, returning any old string to drop off the audio thread.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub(crate) fn replace_monitor_device(&mut self, device: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.monitor_device, device)
    }

    /// Marks source `id` as an audio output capture (or clears it with `None`).
    pub fn set_output_capture(&mut self, id: u64, capture: Option<OutputCapture>) {
        if let Some(s) = self.sources.get_mut(&id) {
            s.output_capture = capture;
        }
    }

    /// Replaces source `id`'s output-capture metadata, returning any old entry to drop off-thread.
    pub(crate) fn replace_output_capture(&mut self, id: u64, capture: Option<OutputCapture>) -> Option<OutputCapture> {
        let s = self.sources.get_mut(&id)?;
        std::mem::replace(&mut s.output_capture, capture)
    }

    /// Updates whether source `id` is currently active/on air (`obs_source_active` parity for
    /// deduplicating monitored sources against an output capture).
    pub fn set_active(&mut self, id: u64, active: bool) {
        if let Some(s) = self.sources.get_mut(&id) {
            s.active = active;
        }
    }

    /// Updates every source's active flag against `active` without allocating.
    pub fn sync_active(&mut self, active: &std::collections::HashSet<u64>) {
        for (id, s) in &mut self.sources {
            s.active = active.contains(id);
        }
    }

    /// Updates every source's active flag against a sorted slice `active` without allocating.
    pub fn sync_active_sorted(&mut self, active: &[u64]) {
        for (id, s) in &mut self.sources {
            s.active = active.binary_search(id).is_ok();
        }
    }

    /// Whether any active, unmuted source is routed to monitoring.
    pub fn has_monitored(&self) -> bool {
        let mon_dev = self.monitor_device.as_deref();
        self.sources.values().any(|s| {
            s.active
                && s.settings.monitoring != Monitoring::Off
                && !s.settings.muted
                && !s.gated
                && s.settings.volume > 0.0
                && !s.output_capture.as_ref().is_some_and(|oc| !oc.excludes_self && mon_dev == Some(oc.device.as_str()))
        })
    }

    /// Low latency sound on or off (applies at once: the wait goes to its new start).
    pub fn set_low_latency(&mut self, on: bool) {
        if on == self.low_latency {
            return;
        }
        self.low_latency = on;
        self.floor_ns = if on { LOW_FLOOR_NS.min(self.usual_floor_ns) } else { self.usual_floor_ns };
        let held = self.held_most_ns();
        self.latency_ns = (self.floor_ns + held).min(self.cap_ns());
        self.window_ns = None;
    }

    /// The most any source's filters hold its sound back (ns).
    fn held_most_ns(&self) -> u64 {
        let frames = self
            .sources
            .values()
            .map(|s| s.filters.iter().map(AudioFilter::latency_frames).sum::<usize>())
            .max()
            .unwrap_or(0);
        frames as u64 * 1_000_000_000 / u64::from(self.rate)
    }

    /// The most the mixer waits.
    fn cap_ns(&self) -> u64 {
        if self.low_latency { (LOW_CAP_NS + self.held_most_ns()).min(MAX_LATENCY_NS) } else { MAX_LATENCY_NS }
    }

    /// Meters read true peak (4x oversampled, between samples) or sample peak.
    pub fn set_true_peak(&mut self, on: bool) {
        self.true_peak = on;
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// How far behind the clock blocks are made now (it grows when sound comes late).
    pub fn latency_ns(&self) -> u64 {
        self.latency_ns
    }

    /// Starts listening to source `id`'s sound (see [`Tap`]).
    pub fn tap(&mut self, id: u64) -> Tap {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        self.attach_tap(id, std::sync::Arc::downgrade(&buf));
        Tap(buf)
    }

    /// Registers a pre-created tap handle for source `id`.
    pub fn attach_tap(&mut self, id: u64, tap: std::sync::Weak<std::sync::Mutex<std::collections::VecDeque<f32>>>) {
        self.taps.retain(|(_, t)| t.strong_count() > 0);
        self.taps.push((id, tap));
    }

    /// A source's sound settings, if it's in the mix.
    pub fn settings(&self, id: u64) -> Option<SourceSettings> {
        self.sources.get(&id).map(|s| s.settings)
    }

    /// Puts `id` in the mix with the default settings unless it's there already (then it's
    /// left as it is). A slow source that finished opening after its settings were loaded used
    /// `set` with the defaults and lost them: muted came back unmuted, at full volume (found
    /// 20 Nov in the OBS comparison).
    pub fn ensure(&mut self, id: u64) {
        if !self.sources.contains_key(&id) {
            self.set(id, SourceSettings::default());
        }
    }

    /// A source's mixer entry, new (what `set` makes when none was made ahead).
    fn fresh_source(rate: u32) -> Box<Source> {
        let settings = SourceSettings::default();
        Box::new(Source {
            settings,
            output_capture: None,
            active: true,
            // Room for a second of stereo: a stall that lets sound pile up must not grow it
            // mid-mix (the mixer's cap on waiting is a second).
            queue: VecDeque::with_capacity(rate as usize * 2),
            start_ns: None,
            resampler: None,
            filters: Vec::with_capacity(16),
            key_routes: Vec::new(),
            key_slot: None,
            last_post_filter: [0.0; 2],
            filter_declick: false,
            gain_applied: None,
            ptt_down: false,
            ptm_down: false,
            ptt_hold: 0,
            ptm_hold: 0,
            gated: false,
            levels: Levels::default(),
            recent: VecDeque::with_capacity(METER_BLOCKS + 1),
            tp: [[0.0; TP_TAPS]; 2],
            adjust_ns: 0,
            offset_ns: None,
            drift: DriftTrack::default(),
            bypass: VecDeque::new(),
            bypass_pure: 0,
            bypass_done: 0,
            bypass_owed: 0,
            ease_owed: 0,
            hold: Vec::with_capacity(HOLD_ROOM),
            hold_ts: 0,
            hold_rate: 0,
            hold_ch: 0,
        })
    }

    /// A source entry made ahead, to be handed to [`Mixer::add_spare`]: building one allocates,
    /// so the audio thread asks for it ([`Mixer::take_spare_request`]) and a background thread
    /// makes it.
    pub fn make_spare(rate: u32) -> SpareSource {
        SpareSource(Self::fresh_source(rate))
    }

    fn cold_rs(rate: u32) -> SourceResampler {
        // Same rate in and out: the filter the drift follower uses. (Can't fail for these rates.)
        SourceResampler::new_cold(rate, rate).unwrap_or_else(|| panic!("a {rate} Hz resampler"))
    }

    /// A resampler made ahead for `in_rate` to `out_rate` (None for rates it can't do), to be
    /// handed to [`Mixer::add_spare_resampler`].
    pub fn make_spare_resampler(in_rate: u32, out_rate: u32) -> Option<SpareResampler> {
        SourceResampler::new_cold(in_rate, out_rate).map(SpareResampler)
    }

    pub fn add_spare_resampler(&mut self, r: SpareResampler) {
        if let Some(i) = self.rs_pending.iter().position(|p| *p == r.0.in_rate) {
            self.rs_pending.swap_remove(i);
        }
        if self.rs_spares.len() >= RS_SPARES_ROOM {
            // Evict an older unused spare off-thread instead of dropping the arrival on the RT
            // thread and causing an infinite request/build/drop loop.
            let same_rate_full = r.0.in_rate == self.rate
                && self.rs_spares.iter().filter(|s| s.0.in_rate == self.rate).count() >= RS_SPARES;
            let evict_idx = if same_rate_full {
                self.rs_spares.iter().position(|s| s.0.in_rate == self.rate).unwrap_or(0)
            } else {
                self.rs_spares
                    .iter()
                    .position(|s| {
                        s.0.in_rate != self.rate
                            && !self.sources.values().any(|src| !src.hold.is_empty() && src.hold_rate == s.0.in_rate)
                    })
                    .or_else(|| self.rs_spares.iter().position(|s| s.0.in_rate != self.rate))
                    .unwrap_or(0)
            };
            let evicted = self.rs_spares.remove(evict_idx);
            if self.rs_retired.len() < RS_RETIRED_ROOM {
                self.rs_retired.push(evicted);
            }
        }
        self.rs_spares.push(r);
    }

    /// The next input rate a resampler is asked for (the audio thread sends it to be made).
    /// Resamplers for other rates are asked for ([`Mixer::pop_resampler_request`]) and waited
    /// for, not made here (set where something answers the requests).
    pub fn set_background_resamplers(&mut self, on: bool) {
        self.async_resamplers = on;
    }

    pub fn pop_resampler_request(&mut self) -> Option<u32> {
        self.rs_wanted.pop()
    }

    /// Puts an unsent request back when the background queue is full so `rs_pending` never wedges.
    pub(crate) fn requeue_resampler_request(&mut self, in_rate: u32) {
        if self.rs_wanted.len() < RS_SPARES_ROOM {
            self.rs_wanted.push(in_rate);
        } else if let Some(i) = self.rs_pending.iter().position(|p| *p == in_rate) {
            self.rs_pending.swap_remove(i);
        }
    }

    /// Marks `in_rate` as unbuildable when `make_spare_resampler` returned `None`, clearing
    /// pending requests and held audio for that rate so it neither wedges nor loops.
    pub(crate) fn fail_resampler(&mut self, in_rate: u32) {
        self.rs_pending.retain(|&r| r != in_rate);
        self.rs_wanted.retain(|&r| r != in_rate);
        if !self.rs_failed.contains(&in_rate) && self.rs_failed.len() < RS_SPARES_ROOM {
            self.rs_failed.push(in_rate);
        }
        for s in self.sources.values_mut() {
            if s.hold_rate == in_rate {
                s.clear_hold();
            }
        }
    }

    pub fn resampler_out_rate(&self) -> u32 {
        self.rate
    }

    /// Whether a resampler for sound at `rate` is ready for source `id` (it has one, or one
    /// made ahead waits).
    fn resampler_ready(&self, id: u64, rate: u32) -> bool {
        self.sources.get(&id).is_some_and(|s| s.resampler.as_ref().is_some_and(|r| r.in_rate == rate))
            || self.rs_spares.iter().any(|s| s.0.in_rate == rate && s.0.out_rate == self.rate)
    }

    /// Ensures enough spares are in flight or queued for `in_rate` to cover all sources currently
    /// holding `in_rate` (or at least 1 for a `WantResampler` pre-hint).
    fn request_resampler_for(&mut self, in_rate: u32, min_needed: usize) {
        if !self.async_resamplers || in_rate == self.rate || in_rate == 0 || self.rs_failed.contains(&in_rate) {
            return;
        }
        let holding = self
            .sources
            .values()
            .filter(|s| {
                !s.hold.is_empty()
                    && s.hold_rate == in_rate
                    && s.resampler.as_ref().is_none_or(|r| r.in_rate != in_rate)
            })
            .count();
        let needed = holding.max(min_needed);
        let mut have = self.rs_spares.iter().filter(|s| s.0.in_rate == in_rate && s.0.out_rate == self.rate).count()
            + self.rs_pending.iter().filter(|&&r| r == in_rate).count();
        while have < needed && self.rs_pending.len() < RS_SPARES_ROOM && self.rs_wanted.len() < RS_SPARES_ROOM {
            self.rs_pending.push(in_rate);
            self.rs_wanted.push(in_rate);
            have += 1;
        }
    }

    /// Asks (once) for a resampler for sound at `in_rate`.
    pub fn want_resampler(&mut self, in_rate: u32) {
        self.request_resampler_for(in_rate, 1);
    }

    /// True when `pcm` was held (not pushed now): it's another rate and no resampler for it is
    /// ready, or earlier sound of the source is still held (order is kept).
    fn hold_for_resampler(&mut self, id: u64, pcm: &[f32], channels: usize, rate: u32, ts_ns: u64) -> bool {
        if channels == 0 || rate == 0 {
            return false;
        }
        if self.rs_failed.contains(&rate) {
            if let Some(src) = self.sources.get_mut(&id) {
                src.clear_hold();
            }
            return true;
        }
        let out_rate = self.rate;
        let next_ns = self.next_ns;
        let mixed_with_ns = self.mixed_with_ns;
        let Some(src) = self.sources.get_mut(&id) else { return false };
        if !src.hold.is_empty() {
            // Format changed or timeline jumped: drop stale hold so returning to `out_rate` or
            // switching rates/channels never drops new audio.
            let format_changed = src.hold_rate != rate || src.hold_ch != channels;
            let gap_jumped = ts_ns.saturating_sub(src.hold_ts) > MAX_GAP_NS;
            let stale_reanchored = next_ns.is_some_and(|next| {
                let due = next + mixed_with_ns;
                due.saturating_sub(src.hold_ts.saturating_add(src.adjust_ns)) > RETIME_NS
                    && due.saturating_sub(ts_ns.saturating_add(src.adjust_ns)) <= RETIME_NS
            });
            if format_changed || gap_jumped || stale_reanchored {
                src.clear_hold();
            }
        }
        let still = !src.hold.is_empty();
        if !still && (rate == out_rate || self.resampler_ready(id, rate)) {
            return false;
        }
        let Some(src) = self.sources.get_mut(&id) else { return false };
        let frames = pcm.len() / channels;
        let usable = frames * channels;
        let max_frames = TAIL_ROOM.min(HOLD_ROOM / channels);
        let max_samples = max_frames * channels;
        if src.hold.is_empty() {
            (src.hold_ts, src.hold_rate, src.hold_ch) = (ts_ns, rate, channels);
        }
        // Same format as what is held and room inside both `HOLD_ROOM` and `TAIL_ROOM`.
        if src.hold_rate == rate && src.hold_ch == channels && src.hold.len() + usable <= max_samples {
            src.hold.extend_from_slice(&pcm[..usable]);
        } else if src.hold.is_empty() && usable > max_samples && max_samples > 0 {
            let skip_frames = frames - max_frames;
            src.hold_ts = ts_ns.saturating_add((skip_frames as u64 * 1_000_000_000) / u64::from(rate));
            src.hold.extend_from_slice(&pcm[skip_frames * channels..usable]);
        }
        if rate != out_rate && !self.resampler_ready(id, rate) {
            self.request_resampler_for(rate, 1);
        }
        true
    }

    fn release_held_one(&mut self, id: u64) {
        let Some(src) = self.sources.get_mut(&id) else { return };
        if src.hold.is_empty() {
            return;
        }
        let stale = self.next_ns.is_some_and(|next| {
            let due = next + self.mixed_with_ns;
            due.saturating_sub(src.hold_ts.saturating_add(src.adjust_ns)) > RETIME_NS
        });
        if stale {
            src.clear_hold();
            return;
        }
        let (held, ts, rate, ch) = (std::mem::take(&mut src.hold), src.hold_ts, src.hold_rate, src.hold_ch);
        src.hold_ts = 0;
        src.hold_rate = 0;
        src.hold_ch = 0;
        self.push(id, &held, ch, rate, ts);
        // Kept for next time (with its room), unless the push held it again (then it is the
        // source's own and this one is dropped).
        if let Some(src) = self.sources.get_mut(&id)
            && src.hold.is_empty()
        {
            src.hold = held;
            src.hold.clear();
        }
    }

    /// Runs held sound through the resamplers that have arrived (after commands are handled), and
    /// ensures any remaining held sources still have a resampler request in flight.
    pub fn release_held(&mut self) {
        loop {
            let due = self.sources.iter().find_map(|(&id, s)| {
                (!s.hold.is_empty() && (s.hold_rate == self.rate || self.resampler_ready(id, s.hold_rate)))
                    .then_some(id)
            });
            let Some(id) = due else { break };
            self.release_held_one(id);
        }
        let mut rem_rates = [0u32; RS_SPARES_ROOM];
        let mut n = 0usize;
        for s in self.sources.values() {
            if !s.hold.is_empty()
                && s.hold_rate != self.rate
                && !rem_rates[..n].contains(&s.hold_rate)
                && n < RS_SPARES_ROOM
            {
                rem_rates[n] = s.hold_rate;
                n += 1;
            }
        }
        for &rate in &rem_rates[..n] {
            self.request_resampler_for(rate, 1);
        }
    }

    /// A resampler a source left, to be dropped off the audio thread.
    pub fn pop_retired_resampler(&mut self) -> Option<SpareResampler> {
        self.rs_retired.pop()
    }

    /// Keeps a spare entry for the next source that appears.
    pub fn add_spare(&mut self, s: SpareSource) {
        self.spares_pending = self.spares_pending.saturating_sub(1);
        if self.spares.len() < SPARES {
            self.spares.push(s);
        } else if self.retired.len() < RETIRED_ROOM {
            self.retired.push(s.0);
        }
    }

    /// How many spares to make to replace the ones used since the last call.
    pub fn take_spare_request(&mut self) -> usize {
        let n = std::mem::take(&mut self.spares_wanted);
        self.spares_pending += n;
        n
    }

    /// Restores an unsent spare request if the background queue was full.
    pub(crate) fn restore_spare_request(&mut self, n: usize) {
        self.spares_pending = self.spares_pending.saturating_sub(n);
        let room = SPARES.saturating_sub(self.spares.len() + self.spares_pending);
        self.spares_wanted = (self.spares_wanted + n).min(room);
    }

    /// An entry a removed source left, to be dropped off the audio thread (its buffers free
    /// there otherwise).
    pub fn pop_retired(&mut self) -> Option<SpareSource> {
        self.retired.pop().map(SpareSource)
    }

    /// Adds a source (or changes its settings).
    pub fn set(&mut self, id: u64, settings: SourceSettings) {
        match self.sources.get_mut(&id) {
            Some(s) => s.settings = settings,
            None => {
                // A source made ahead (off the audio thread) when there is one.
                let mut src = self.spares.pop().map_or_else(|| Self::fresh_source(self.rate), |s| s.0);
                src.settings = settings;
                self.sources.insert(id, src);
                if self.spares.len() + self.spares_wanted + self.spares_pending < SPARES {
                    self.spares_wanted += 1;
                }
                self.rebuild_mix_plan();
            }
        }
    }

    /// A push-to-talk (`talk`) or push-to-mute key went down or up for source `id`: while it is
    /// held, and for the source's release delay after, it gates the sound (in samples, from the
    /// next block; the gain moves over 8 ms, no click).
    pub fn push_key(&mut self, id: u64, talk: bool, down: bool) {
        let rate = self.rate;
        let Some(src) = self.sources.get_mut(&id) else { return };
        let delay_ms = if talk { src.settings.push_to_talk } else { src.settings.push_to_mute }.unwrap_or(0);
        let held =
            if down { 0 } else { (u64::from(delay_ms) * u64::from(rate) / 1000).min(u64::from(u32::MAX)) as u32 };
        if talk {
            src.ptt_down = down;
            src.ptt_hold = held;
        } else {
            src.ptm_down = down;
            src.ptm_hold = held;
        }
    }

    /// A source's filters, in order (replacing any before). Every edit of the collection sets
    /// them all again: a filter whose settings didn't change keeps running as it was (its
    /// envelope, the noise removal's model, a VST's helper), one of the same kind with new
    /// settings takes them and keeps what it remembers (a slider dragged on a gate or a
    /// compressor made it anew each step: a click each time), and only the rest are made new.
    pub fn set_effects(&mut self, id: u64, effects: &[AudioEffect]) {
        let rate = self.rate;
        let _ = self.apply_effects_core(id, effects, |e| AudioFilter::new(*e, rate));
    }

    /// Updates `id`'s filters without constructing or dropping heavy [`SoundPlugin`]s on the
    /// calling thread. Returns evicted filters (for off-thread drop) and `(build_id, effect)` pairs
    /// whose plugins should be constructed off-thread and installed via [`Self::attach_plugins`].
    pub fn set_effects_nonblocking(
        &mut self,
        id: u64,
        effects: &[AudioEffect],
        next_build_id: &mut impl FnMut() -> u64,
        needs_plugin: &dyn Fn(AudioEffect, u32) -> bool,
    ) -> (Vec<AudioFilter>, Vec<(u64, AudioEffect)>) {
        let rate = self.rate;
        let mut missing = Vec::new();
        let evicted = self.apply_effects_core(id, effects, |e| {
            let mut f = AudioFilter::new_without_plugin(*e, rate);
            if needs_plugin(*e, rate) {
                let bid = next_build_id();
                f.pending_build = Some(bid);
                missing.push((bid, *e));
            }
            f
        });
        if !missing.is_empty() {
            self.reserve_bypass(id);
        }
        (evicted, missing)
    }

    fn apply_effects_core(
        &mut self,
        id: u64,
        effects: &[AudioEffect],
        mut make_new: impl FnMut(&AudioEffect) -> AudioFilter,
    ) -> Vec<AudioFilter> {
        // Filters set before the source is in the mix (a slow file still opening) are kept: it's
        // put in with the defaults, its settings come when they're set (they were dropped).
        if !effects.is_empty() {
            self.ensure(id);
        }
        let Some(s) = self.sources.get_mut(&id) else { return Vec::new() };
        let held = |fs: &[AudioFilter]| fs.iter().map(AudioFilter::latency_frames).sum::<usize>();
        let before = held(&s.filters);
        let old_len = s.filters.len();
        let mut scratch = std::mem::take(&mut self.effect_scratch);
        scratch.old.clear();
        for (i, f) in s.filters.drain(..).enumerate() {
            scratch.old.push((i, Some(f)));
        }
        // The same settings first, so a moved filter is found as it was…
        scratch.made.clear();
        for e in effects {
            let found = scratch
                .old
                .iter_mut()
                .find(|(_, o)| o.as_ref().is_some_and(|f| f.effect == *e))
                .and_then(|(idx, o)| o.take().map(|f| (*idx, f)));
            scratch.made.push(found);
        }
        // …then one of the same kind takes the new settings…
        for (slot, e) in scratch.made.iter_mut().zip(effects) {
            if slot.is_some() {
                continue;
            }
            for (idx, o) in scratch.old.iter_mut() {
                if let Some(f) = o.as_mut()
                    && f.retune(*e)
                {
                    *slot = o.take().map(|f| (*idx, f));
                    break;
                }
            }
        }
        let pure_in_place = effects.len() == old_len
            && scratch.made.iter().enumerate().all(|(i, slot)| matches!(slot, Some((idx, _)) if *idx == i));
        // …and the rest are made.
        for (slot, e) in scratch.made.drain(..).zip(effects) {
            s.filters.push(slot.map_or_else(|| make_new(e), |(_, f)| f));
        }
        if !pure_in_place {
            s.filter_declick = true;
        }
        let mut evicted = Vec::new();
        for (_, o) in scratch.old.drain(..) {
            if let Some(f) = o {
                evicted.push(f);
            }
        }
        self.effect_scratch = scratch;
        // A filter that held sound back and is taken out: what it still held plays first. Its
        // sound is filed that much later from now on, so without this the source goes quiet for
        // the filter's delay (82.7 ms of silence, then a step) every time it is switched off.
        // The held sound goes in front of what is queued, so the sound that comes next
        // continues from it, nothing lost and nothing doubled.
        let now_held = held(&s.filters);
        if now_held == 0 {
            s.bypass.clear();
            s.bypass_pure = 0;
            s.bypass_done = 0;
            s.bypass_owed = 0;
            s.ease_owed = 0;
        }
        if now_held < before && s.resampler.is_none() {
            let mut latent = evicted.iter_mut().filter(|f| f.latency_frames() > 0);
            if let (Some(f), None) = (latent.next(), latent.next()) {
                let lost = before - now_held;
                if f.latency_frames() == lost
                    && let Some([mut l, mut r]) = f.take_tail()
                {
                    // Padded or cut to the delay: silence only for what it never had.
                    for c in [&mut l, &mut r] {
                        if c.len() < lost {
                            c.splice(0..0, std::iter::repeat_n(0.0, lost - c.len()));
                        }
                        c.drain(..c.len() - lost);
                    }
                    for (a, b) in l.iter().zip(&r).rev() {
                        s.queue.push_front(*b);
                        s.queue.push_front(*a);
                    }
                    // The tail is the filter's input (the dry sound), and the last sound out was
                    // the filter's output, which is at another level (the denoiser's is about
                    // 8 dB under): the join steps. `filter_declick` (set above) eases it over
                    // 4 ms from the last sample out, so it is not cleared here (S57: a step of
                    // 0.6 at every switch-off). It also needs no new buffer.
                }
            }
        }
        // A filter that holds the sound back (the strong noise removal) files the source's sound
        // that much earlier: the mixer waits that much longer at once, rather than finding the
        // sound late and cutting its start.
        // Each source is held back by its own filters only: the wait covers the most any one
        // needs (its usual wait plus that), not the sum of them all.
        let now = held(&s.filters);
        self.raise_latency_for_filters(before, now);
        if before == 0 && now > 0 {
            self.reserve_bypass(id);
            self.begin_bypass(id, now);
        }
        self.rebuild_mix_plan();
        evicted
    }

    /// Makes room for a dry bypass on `id` (done when its filter is asked for, so that putting
    /// the filter on later allocates nothing).
    fn reserve_bypass(&mut self, id: u64) {
        let ease = (BYPASS_EASE_S * f64::from(self.rate)).ceil() as usize;
        if let Some(s) = self.sources.get_mut(&id) {
            s.bypass.reserve((MAX_BYPASS_FRAMES + ease) * 2);
        }
    }

    /// A filter that holds `lat` frames back went on a source that had none: its first `lat`
    /// frames of output are silence, and its sound is filed that much earlier. The queue's first
    /// `lat` frames are set aside as a dry bypass that plays meanwhile (and a little more of
    /// it, to ease into the filter's sound), and the filter's input is what comes after them:
    /// nothing cut, nothing skipped, no hole. Allocation-free once `reserve_bypass` has run.
    fn begin_bypass(&mut self, id: u64, lat: usize) {
        let ease = (BYPASS_EASE_S * f64::from(self.rate)).ceil() as usize;
        let Some(s) = self.sources.get_mut(&id) else { return };
        if lat == 0 || lat > MAX_BYPASS_FRAMES || s.resampler.is_some() || s.bypass.capacity() < (lat + ease) * 2 {
            return;
        }
        let avail = s.queue.len() / 2;
        let pure = lat.min(avail);
        let take = (lat + ease).min(avail);
        s.bypass.clear();
        s.bypass.extend(s.queue.iter().take(take * 2).copied());
        s.queue.drain(..pure * 2);
        s.bypass_pure = lat;
        s.bypass_done = 0;
        // The rest comes with the next pushes (the mixer waits longer now, so it arrives
        // before it is needed).
        s.bypass_owed = lat - pure;
        s.ease_owed = if s.bypass_owed > 0 { ease } else { ease - (take - pure) };
        s.filter_declick = false;
    }

    fn raise_latency_for_filters(&mut self, before: usize, now: usize) {
        let rate = self.rate;
        let need = (self.floor_ns + now as u64 * 1_000_000_000 / u64::from(rate)).min(self.cap_ns());
        if now > before && need > self.latency_ns {
            let was = self.latency_ns;
            self.latency_ns = need;
            self.window_ns = None;
            tracing::info!(
                from_ms = was / 1_000_000,
                to_ms = self.latency_ns / 1_000_000,
                "a filter holds sound back: the mixer waits longer for it"
            );
        }
    }

    /// Attaches off-thread built [`SoundPlugin`]s matching their `build_id`s, returning any stale
    /// or incompatible plugins (to drop off-thread) and any `(build_id, effect)` pairs that need a
    /// fresh build because the filter changed incompatibly or declined `retune`.
    pub fn attach_plugins(
        &mut self,
        id: u64,
        plugins: Vec<(u64, AudioEffect, Box<dyn crate::afx::SoundPlugin>)>,
    ) -> AttachOutcome {
        let mut out = AttachOutcome::default();
        let Some(s) = self.sources.get_mut(&id) else {
            for (.., p) in plugins {
                out.rejected.push(p);
            }
            return out;
        };
        let held = |fs: &[AudioFilter]| fs.iter().map(AudioFilter::latency_frames).sum::<usize>();
        let before = held(&s.filters);
        for (build_id, effect, mut plugin) in plugins {
            let Some(slot) = s.filters.iter_mut().find(|f| f.pending_build == Some(build_id)) else {
                out.rejected.push(plugin);
                continue;
            };
            if slot.effect != effect {
                let compatible = std::mem::discriminant(&slot.effect) == std::mem::discriminant(&effect)
                    && !matches!(
                        (slot.effect, effect),
                        (AudioEffect::Plugin { slot: a }, AudioEffect::Plugin { slot: b }) if a != b
                    );
                if !compatible {
                    slot.pending_build = None;
                    out.rejected.push(plugin);
                    continue;
                }
                if !plugin.retune(&slot.effect) {
                    out.rejected.push(plugin);
                    out.rebuild.push((build_id, slot.effect));
                    continue;
                }
            }
            slot.install_plugin(plugin);
        }
        let now = held(&s.filters);
        self.raise_latency_for_filters(before, now);
        if before == 0 && now > 0 {
            self.begin_bypass(id, now);
        }
        out
    }

    /// Clears queued audio and timing state for source `id` (e.g. on media pause, stop, or seek),
    /// while preserving settings, filters, and learned clock-drift slope.
    pub fn flush_source(&mut self, id: u64) {
        let Some(s) = self.sources.get_mut(&id) else { return };
        s.queue.clear();
        s.clear_hold();
        s.start_ns = None;
        s.adjust_ns = 0;
        s.drift.reset_segment();
        s.drift.tail[0].clear();
        s.drift.tail[1].clear();
        if let Some(rs) = &mut s.resampler {
            rs.reset(&mut s.queue);
        }
    }

    pub fn remove(&mut self, id: u64) -> Option<Vec<AudioFilter>> {
        self.taps.retain(|(tid, t)| *tid != id && t.strong_count() > 0);
        let removed = self.sources.remove(&id).map(|mut s| {
            let filters = if s.filters.is_empty() { Vec::new() } else { std::mem::take(&mut s.filters) };
            // Kept (room is made ahead) for the caller to drop elsewhere; if full, dropped here.
            if self.retired.len() < RETIRED_ROOM {
                self.retired.push(s);
            }
            filters
        });
        if removed.is_some() {
            self.rebuild_mix_plan();
        }
        removed
    }

    /// Whether source `id` is following its clock's drift now (a resampler is at work).
    #[must_use]
    pub fn follows_drift(&self, id: u64) -> bool {
        self.sources.get(&id).is_some_and(|s| s.drift.follows)
    }

    /// Number of sources currently registered in the mixer.
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    /// Number of registered tap entries currently held by the mixer.
    pub fn tap_count(&self) -> usize {
        self.taps.len()
    }

    /// Music quieter while the voices talk (None: off). Kept going when only its settings change.
    pub fn set_ducking(&mut self, ducking: Option<crate::level::Ducking>) {
        match (ducking, self.ducker.as_mut()) {
            (Some(d), Some(on)) => on.cfg = d,
            (Some(d), None) => self.ducker = Some(crate::level::Ducker::new(d)),
            (None, _) => self.ducker = None,
        }
    }

    /// Each track steered to `target_lufs` (-14 for most platforms) and kept under -1 dBFS
    /// (None: off).
    pub fn set_loudness(&mut self, target_lufs: Option<f32>) {
        // The same target again (a scene edit reloads it): kept going, its gain where it is.
        if target_lufs == self.loudness_target {
            return;
        }
        self.loudness_target = target_lufs;
        self.loudness = target_lufs.map(|t| (0..TRACKS).map(|_| crate::level::Loudness::new(t, self.rate)).collect());
    }

    /// A source's levels for a meter: the highest peak and the RMS over the last
    /// [`METER_BLOCKS`] blocks (every reader sees the same; nothing is reset by reading).
    pub fn levels(&self, id: u64) -> Option<Levels> {
        self.levels_over(id, METER_BLOCKS)
    }

    /// Like [`Mixer::levels`] over only the newest `blocks` blocks (allocation-free).
    pub fn levels_over(&self, id: u64, blocks: usize) -> Option<Levels> {
        let s = self.sources.get(&id)?;
        if s.recent.is_empty() {
            return Some(s.levels);
        }
        let take = blocks.min(s.recent.len());
        let n = take as f32 * BLOCK as f32;
        let mut m = Measured::default();
        for r in s.recent.iter().rev().take(take) {
            for c in 0..2 {
                m.peak[c] = m.peak[c].max(r.peak[c]);
                m.input[c] = m.input[c].max(r.input[c]);
                m.sq[c] += r.sq[c];
            }
        }
        Some(Levels {
            peak: m.peak.map(db),
            rms: [db((m.sq[0] / n).sqrt()), db((m.sq[1] / n).sqrt())],
            input_peak: m.input.map(db),
            gated: s.gated,
        })
    }

    /// A source's sound: `channels` interleaved (1 or 2; more are folded to their first two) at
    /// `rate`, its first frame at `ts_ns`. Unknown sources are ignored.
    pub fn push(&mut self, id: u64, pcm: &[f32], channels: usize, rate: u32, ts_ns: u64) {
        // Earlier sound held for its resampler goes first, once that is here.
        if self.async_resamplers
            && let Some(s) = self.sources.get(&id)
            && !s.hold.is_empty()
            && self.resampler_ready(id, s.hold_rate)
        {
            self.release_held_one(id);
        }
        // The planar copy lives in buffers kept between pushes (taken out, so the rest of
        // `self` can be borrowed meanwhile): a steady source allocates nothing.
        let (mut l, mut r) = (std::mem::take(&mut self.scratch.l), std::mem::take(&mut self.scratch.r));
        self.push_with(id, pcm, channels, rate, ts_ns, &mut l, &mut r);
        self.scratch.l = l;
        self.scratch.r = r;
    }

    #[allow(clippy::too_many_arguments)]
    fn push_with(
        &mut self,
        id: u64,
        pcm: &[f32],
        channels: usize,
        rate: u32,
        ts_ns: u64,
        l: &mut Vec<f32>,
        r: &mut Vec<f32>,
    ) {
        let out_rate = self.rate;
        if channels == 0 || rate == 0 || pcm.is_empty() {
            return;
        }
        // Another rate with its resampler not here yet: the sound is held as it came, and run
        // through when the resampler arrives (see `release_held`), so it starts complete.
        if self.async_resamplers && self.hold_for_resampler(id, pcm, channels, rate, ts_ns) {
            return;
        }
        // A filter that holds sound back just went on and the queue didn't hold all of the dry
        // sound it needs: the first frames pushed now are that sound (see `begin_bypass`).
        let (pcm, ts_ns) = match self.sources.get_mut(&id) {
            Some(src) if src.bypass_owed + src.ease_owed > 0 => {
                if rate != out_rate || src.resampler.is_some() || src.drift.follows {
                    src.bypass.clear();
                    (src.bypass_owed, src.ease_owed) = (0, 0);
                    (pcm, ts_ns)
                } else {
                    let frames = pcm.len() / channels;
                    let mut put = |range: std::ops::Range<usize>| {
                        for f in pcm[range.start * channels..range.end * channels].chunks_exact(channels) {
                            src.bypass.push_back(f[0]);
                            src.bypass.push_back(if channels > 1 { f[1] } else { f[0] });
                        }
                    };
                    let k = src.bypass_owed.min(frames);
                    put(0..k);
                    src.bypass_owed -= k;
                    if src.bypass_owed == 0 {
                        let e = src.ease_owed.min(frames - k);
                        put(k..k + e);
                        src.ease_owed -= e;
                    }
                    if k == frames {
                        return;
                    }
                    (&pcm[k * channels..], ts_ns + k as u64 * 1_000_000_000 / u64::from(rate))
                }
            }
            _ => (pcm, ts_ns),
        };
        // Its sync offset, less what its filters hold it back by (the strong noise removal
        // gives a voice back 40 ms late): filed that much earlier, it comes out on time.
        let Some(offset) = self.sources.get(&id).map(|s| {
            let held: usize = s.filters.iter().map(AudioFilter::latency_frames).sum();
            s.settings.sync_offset_ns - (held as u64 * 1_000_000_000 / u64::from(out_rate)) as i64
        }) else {
            return;
        };
        let mut ts = (ts_ns as i64 + offset).max(0) as u64;
        let mixed_to = self.next_ns;
        if let (Some(next), Some(src)) = (self.next_ns, self.sources.get_mut(&id)) {
            // Judged on the device's own time, not on the sync offset or what filters hold
            // back (those are meant: moved earlier on purpose, they aren't lateness; CodeRabbit).
            let due = next + self.mixed_with_ns;
            let late_raw = due.saturating_sub(ts_ns);
            if src.adjust_ns > 0 && late_raw < RETIME_NS / 2 {
                // Its times came right again: no correction any more (else it would stay late
                // by the old one, with silence put in; CodeRabbit).
                src.adjust_ns = 0;
                src.queue.clear();
                src.start_ns = None;
                tracing::info!(source = id, "a source's sound comes on time again");
            }
            let late = due.saturating_sub(ts_ns + src.adjust_ns);
            if late > RETIME_NS {
                // Timed by when it comes: just in time for the next mix (libobs does the same
                // with a source whose times are too far off).
                src.adjust_ns += late;
                src.queue.clear();
                src.start_ns = None;
                tracing::warn!(
                    source = id,
                    late_ms = late / 1_000_000,
                    "a source's sound came far too late: it's timed by when it comes now"
                );
            }
            ts += src.adjust_ns;
        }
        // Sound for a time already mixed came too late: its start would be cut off, and with a
        // source that's always this late (ScreenCaptureKit hands sound over 20-40 ms after it
        // plays, and the engine takes it once a frame) every piece lost its start, a break
        // several times a second. As libobs does, the mixer waits that much longer from now on
        // (at most a second). How late it came is measured from the last mix: the blocks up to
        // `next` were made `mixed_with_ns` behind the clock.
        if let Some(next) = self.next_ns {
            let late = (next + self.mixed_with_ns).saturating_sub(ts);
            self.worst_late_ns = self.worst_late_ns.max(late);
            let needed = (late + LATENCY_ROOM_NS).min(self.cap_ns());
            if ts < next && needed > self.latency_ns {
                let was = self.latency_ns;
                self.latency_ns = needed;
                // A calm stretch starts again from here before it may wait less.
                self.window_ns = None;
                tracing::info!(
                    from_ms = was / 1_000_000,
                    to_ms = self.latency_ns / 1_000_000,
                    "sound came late: the mixer waits longer for it"
                );
            }
        }
        let Some(src) = self.sources.get_mut(&id) else { return };
        let moved = src.offset_ns.is_some_and(|o| o != offset);
        src.offset_ns = Some(offset);
        // Where this sound would continue what's queued: jitter is smoothed over; a gap ahead
        // (a source that went quiet, beeps scheduled apart) is filled with silence, as libobs
        // does, so nothing queued is lost; a jump back or a long jump (a seek) restarts the queue.
        // What its resampler still holds counts at its actual input rate and active drift ratio.
        let q_frames = src.queue.len() / 2;
        let held_frames = src.resampler.as_ref().map_or(0.0, |rs| rs.held_out_frames(src.drift.ratio));
        let queued_ns = ((q_frames as f64 + held_frames) * 1_000_000_000.0 / f64::from(out_rate)).round() as u64;
        let held_ns = (held_frames * 1_000_000_000.0 / f64::from(out_rate)).round() as u64;
        let piece_s = (pcm.len() / channels) as f64 / f64::from(rate);
        let mut gap = 0usize;
        let mut on_course = false;
        let at = match src.start_ns {
            // Where it would continue is already mixed (it stalled a moment and the mix went on;
            // what a resampler still holds counts too): placed at its own time. Smoothed back to
            // the old spot, every piece would land in the past and lose its start, a choppy
            // sound for good when the stall was longer than the mixer's wait (20 Nov, Neon).
            // (Its resampler keeps running: only where the sound goes moves, so a bursty device
            // that runs dry between bursts never has its sound broken by a reset.)
            Some(start) if mixed_to.is_some_and(|m| start + queued_ns < m) => {
                src.queue.clear();
                src.start_ns = Some(ts.saturating_sub(held_ns));
                None
            }
            Some(start) => {
                let expected = start + queued_ns;
                // Its offset changed (a sync offset dialled in, a filter that holds sound back
                // switched on): placed exactly at its new time (a gap filled or what overlaps
                // replaced), not smoothed back to where it was, and not by clearing the queue.
                if ts == expected || (!moved && ts.abs_diff(expected) < SMOOTHING_NS) {
                    let can_warm = rate != out_rate || src.drift.tail[0].len() >= RESAMPLE_CHUNK;
                    src.drift.observe(id, ts, expected, piece_s, can_warm);
                    on_course = true;
                    None
                } else if ts > expected && ts - expected <= MAX_GAP_NS {
                    gap = ((ts - expected) * u64::from(rate) / 1_000_000_000) as usize;
                    None
                } else if ts < expected && expected - ts <= MAX_GAP_NS && ts >= start {
                    // Earlier than what's queued (a device handing over a burst late): it takes
                    // the place of what's queued from its time on, as libobs places it; the
                    // rest of the queue stays (clearing it all was the tearing, 19 Nov).
                    let keep = frames_in(ts - start, out_rate).min(src.queue.len() / 2);
                    src.queue.truncate(keep * 2);
                    if let Some(rs) = &mut src.resampler {
                        rs.reset(&mut src.queue);
                    }
                    None
                } else {
                    Some(ts)
                }
            }
            None => Some(ts),
        };
        if !on_course {
            // A jump (a gap filled, a burst placed, a new start): restart the regression segment
            // and phase EMA, while keeping the learned clock drift (`follows` and `slope`).
            src.drift.reset_segment();
            if !src.drift.follows {
                src.drift.tail[0].clear();
                src.drift.tail[1].clear();
            }
        }
        // Stereo, planar, after the silence that fills a gap.
        let frames = pcm.len() / channels;
        l.clear();
        r.clear();
        l.resize(gap, 0.0);
        r.resize(gap, 0.0);
        l.reserve(frames);
        r.reserve(frames);
        for f in pcm.chunks_exact(channels) {
            l.push(f[0]);
            r.push(if channels > 1 { f[1] } else { f[0] });
        }
        if !self.taps.is_empty() && !src.settings.muted && !src.gated {
            feed_taps(&mut self.taps, id, (&l[gap..], &r[gap..]), rate, out_rate);
        }
        if let Some(t) = at {
            src.queue.clear();
            src.start_ns = Some(t);
            src.drift.tail[0].clear();
            src.drift.tail[1].clear();
            if let Some(rs) = &mut src.resampler {
                rs.reset(&mut src.queue);
            }
        }
        // Its rate changed (a device or file at another rate now): what the old resampler still
        // holds is played out first, not dropped (a click of ~11 ms; a soak test, 20 Nov).
        let mut switched_rate = false;
        if src.resampler.as_ref().is_some_and(|rs| rs.in_rate != rate)
            && let Some(mut rs) = src.resampler.take()
        {
            switched_rate = true;
            rs.flush_into(&mut src.queue, src.drift.ratio);
            if self.rs_retired.len() < RS_RETIRED_ROOM {
                self.rs_retired.push(SpareResampler(rs));
            }
            // The tail's last frames were filtered with silence for the input that never came at
            // the old rate: no exact join exists, so the old sound fades out and the new fades in
            // over 2 ms (a dip, not a click).
            let n = (src.queue.len() / 2).min(SWITCH_FADE);
            let len = src.queue.len();
            for k in 0..n {
                let g = k as f32 / n as f32;
                src.queue[len - 2 * (k + 1)] *= g;
                src.queue[len - 2 * (k + 1) + 1] *= g;
            }
            for (k, (a, b)) in l.iter_mut().zip(r.iter_mut()).skip(gap).take(SWITCH_FADE).enumerate() {
                let g = k as f32 / SWITCH_FADE as f32;
                *a *= g;
                *b *= g;
            }
        }
        if rate == out_rate && !src.drift.follows {
            // Off the audio thread when dropped (its filter tables are large).
            if let Some(old) = src.resampler.take()
                && self.rs_retired.len() < RS_RETIRED_ROOM
            {
                self.rs_retired.push(SpareResampler(old));
            }
            src.drift.push_tail(l, r);
            for (a, b) in l.iter().zip(r.iter()) {
                src.queue.push_back(*a);
                src.queue.push_back(*b);
            }
        } else {
            if src.resampler.as_ref().is_none_or(|rs| rs.in_rate != rate) {
                let warm = rate == out_rate && !switched_rate && src.drift.tail[0].len() >= RESAMPLE_CHUNK;
                // One made ahead (off the audio thread) when there is one.
                let spare = {
                    let out = self.rate;
                    match self.rs_spares.iter().position(|s| s.0.in_rate == rate && s.0.out_rate == out) {
                        Some(i) => {
                            let got = self.rs_spares.swap_remove(i).0;
                            if rate == out
                                && self.rs_spares.iter().filter(|s| s.0.in_rate == out).count()
                                    + self.rs_wanted.iter().filter(|&&r| r == out).count()
                                    < RS_SPARES
                                && self.rs_wanted.len() < RS_SPARES_ROOM
                            {
                                self.rs_wanted.push(rate);
                            }
                            Some(got)
                        }
                        None => None,
                    }
                };
                let made = match spare {
                    Some(mut rs) => {
                        if warm {
                            rs.prime_warm(&src.drift.tail, &mut src.queue);
                            src.drift.tail[0].clear();
                            src.drift.tail[1].clear();
                        }
                        Some(rs)
                    }
                    // Same rate with none ready (rare): made here, and more asked for.
                    None if rate == out_rate => {
                        if self.rs_spares.iter().filter(|s| s.0.in_rate == out_rate).count()
                            + self.rs_wanted.iter().filter(|&&r| r == out_rate).count()
                            < RS_SPARES
                            && self.rs_wanted.len() < RS_SPARES_ROOM
                        {
                            self.rs_wanted.push(rate);
                        }
                        let rs = if warm {
                            SourceResampler::new_warm(rate, out_rate, &src.drift.tail, &mut src.queue)
                        } else {
                            SourceResampler::new_cold(rate, out_rate)
                        };
                        if warm {
                            src.drift.tail[0].clear();
                            src.drift.tail[1].clear();
                        }
                        rs
                    }
                    // Another rate: asked for (once) and made on the background thread; until it
                    // is here the sound waits (it is never played at the wrong rate).
                    None if !self.async_resamplers => SourceResampler::new_cold(rate, out_rate),
                    None => {
                        self.request_resampler_for(rate, 1);
                        return;
                    }
                };
                match made {
                    Some(rs) => src.resampler = Some(rs),
                    None => return,
                }
            }
            let Some(rs) = &mut src.resampler else { return };
            rs.pending[0].extend_from_slice(l);
            rs.pending[1].extend_from_slice(r);
            rs.process_ready(&mut src.queue, src.drift.follows, src.drift.ratio);
        }
        let max = MAX_QUEUE_SECS * out_rate as usize * 2;
        if src.queue.len() > max {
            let over = (src.queue.len() - max) / 2 * 2;
            src.queue.drain(..over);
            if let Some(s) = &mut src.start_ns {
                *s += (over / 2) as u64 * 1_000_000_000 / u64::from(out_rate);
            }
        }
    }

    /// Every block that ends `latency` before `now_ns`. The first call starts the clock there.
    pub fn mix_until(&mut self, now_ns: u64) -> Vec<Block> {
        let mut out = Vec::new();
        self.mix_until_into(now_ns, &mut out);
        out
    }

    /// [`Mixer::mix_until`], the blocks added to `out` (kept by the caller between calls, and
    /// the blocks given back with [`Mixer::recycle`]: a steady mix then allocates nothing).
    pub fn mix_until_into(&mut self, now_ns: u64, out: &mut Vec<Block>) {
        self.maybe_wait_less(now_ns);
        let until = now_ns.saturating_sub(self.latency_ns);
        let step = block_ns(self.rate);
        let started = self.next_ns.is_some();
        let mut next = *self.next_ns.get_or_insert(until.saturating_sub(step));
        // The clock jumped (the computer slept, a debugger held it): mixing every block of the
        // gap would allocate ~2.6 MB per second of it in one call (an hour: gigabytes, an
        // out-of-memory crash on wake; the M11 scan). What was queued is long past: start again.
        if until.saturating_sub(next) > MAX_CATCH_UP_NS {
            tracing::warn!(gap_ms = (until - next) / 1_000_000, "the clock jumped: the mix starts again from now");
            for src in self.sources.values_mut() {
                src.queue.clear();
                src.clear_hold();
                src.start_ns = None;
                src.drift.reset_segment();
            }
            next = until.saturating_sub(step);
        }
        let mut made = 0usize;
        while next + step <= until {
            let block = self.mix_block(next);
            out.push(block);
            made += 1;
            // Exact frame counts, so blocks never drift from the sample clock.
            next += step;
        }
        // The wait `next` was reached with: only a mix that moved it (or started it) changes
        // that, else a raised wait would count the next late piece as later still (review).
        if !started || made > 0 {
            self.mixed_with_ns = self.latency_ns;
        }
        self.next_ns = Some(next);
    }

    /// After a calm stretch, the wait comes down to what the sound in it needed: nothing is lost
    /// (what's waiting is mixed sooner), and the stream's sound stops lagging after a moment of
    /// trouble (libobs never comes back down).
    fn maybe_wait_less(&mut self, now_ns: u64) {
        let since = *self.window_ns.get_or_insert(now_ns);
        if now_ns.saturating_sub(since) < SHRINK_AFTER_NS {
            return;
        }
        let min_wait = (self.floor_ns + self.held_most_ns()).min(self.cap_ns());
        let target = (self.worst_late_ns + 2 * LATENCY_ROOM_NS + block_ns(self.rate)).max(min_wait).min(self.cap_ns());
        if self.latency_ns > target + LATENCY_ROOM_NS / 2 {
            tracing::info!(
                from_ms = self.latency_ns / 1_000_000,
                to_ms = target / 1_000_000,
                "sound has come on time: the mixer waits less"
            );
            self.latency_ns = target;
        }
        self.worst_late_ns = 0;
        self.window_ns = Some(now_ns);
    }

    fn mix_block(&mut self, start: u64) -> Block {
        let rate = self.rate;
        let Block { mut tracks, mut monitor, .. } = self.take_block(start);
        // The buffers one source's block is made in: kept between blocks.
        let mut buf = std::mem::take(&mut self.scratch.buf);
        let (mut fl, mut fr) = (std::mem::take(&mut self.scratch.fl), std::mem::take(&mut self.scratch.fr));
        let mut bbuf = std::mem::take(&mut self.scratch.bbuf);
        // Talking, from the voices' last block (a block late: about 20 ms, nothing to hear).
        let duck = self.ducker.as_mut().map(|d| {
            let talking = d.cfg.voices.iter().any(|v| {
                self.sources.get(v).is_some_and(|s| s.levels.peak[0].max(s.levels.peak[1]) > d.cfg.threshold_db)
            });
            d.step(talking, BLOCK as f32 / rate as f32)
        });
        let true_peak = self.true_peak;
        let Mixer { sources, ducker, mix_order, key_bufs, monitor_device, .. } = self;
        let mon_dev = monitor_device.as_deref();
        // OBS `should_silence_monitored_source`: when an active, unmuted non-self-excluding output
        // capture shares the monitoring device, suppress direct track output of other monitored
        // sources on the tracks it covers (preventing echo / double audio).
        let mut dup_tracks_mask = 0u8;
        if let Some(mon) = mon_dev {
            for s in sources.values() {
                if s.active
                    && !s.settings.muted
                    && !s.gated
                    && s.settings.volume > 0.0001
                    && s.settings.monitoring != Monitoring::Only
                    && s.output_capture.as_ref().is_some_and(|oc| !oc.excludes_self && oc.device == mon)
                {
                    dup_tracks_mask |= s.settings.tracks;
                }
            }
        }
        for &id in mix_order.iter() {
            let Some(src) = sources.get_mut(&id) else { continue };
            buf.clear();
            buf.resize(BLOCK * 2, 0.0);
            // Where the dry bypass plays in this block: its first frame and how many.
            let mut byp = (0usize, 0usize);
            if let Some(s0) = src.start_ns {
                // Drop what's before this block; place what's in it.
                if s0 < start {
                    let skip = frames_in(start - s0, rate).min(src.queue.len() / 2);
                    src.queue.drain(..skip * 2);
                    // The bypass starts where the queue does: it is late by the same.
                    let late = frames_in(start - s0, rate).min(src.bypass.len() / 2);
                    src.bypass.drain(..late * 2);
                    src.bypass_done += late;
                    src.start_ns = Some(s0 + skip.max(late) as u64 * 1_000_000_000 / u64::from(rate));
                }
                let s0 = src.start_ns.unwrap_or(start);
                let offset = if s0 > start { frames_in(s0 - start, rate) } else { 0 };
                if offset < BLOCK {
                    let n = (BLOCK - offset).min(src.queue.len() / 2);
                    for (i, v) in src.queue.drain(..n * 2).enumerate() {
                        buf[offset * 2 + i] = v;
                    }
                    let nb = (BLOCK - offset).min(src.bypass.len() / 2);
                    if nb > 0 {
                        bbuf.clear();
                        bbuf.resize(BLOCK * 2, 0.0);
                        for (i, v) in src.bypass.drain(..nb * 2).enumerate() {
                            bbuf[offset * 2 + i] = v;
                        }
                        byp = (offset, nb);
                    }
                    if n.max(nb) > 0 {
                        src.start_ns = Some(start + ((offset + n.max(nb)) as u64) * 1_000_000_000 / u64::from(rate));
                    }
                }
            }
            let mut input = [0f32; 2];
            for f in buf.as_chunks::<2>().0 {
                input[0] = input[0].max(f[0].abs());
                input[1] = input[1].max(f[1].abs());
            }
            let s = src.settings;
            // Push-to-talk shuts it unless the key is held (or was, within the release delay);
            // push-to-mute shuts it while its key is held (or was). The delays count down here.
            let gated = (s.push_to_talk.is_some() && !(src.ptt_down || src.ptt_hold > 0))
                || (s.push_to_mute.is_some() && (src.ptm_down || src.ptm_hold > 0));
            if !src.ptt_down {
                src.ptt_hold = src.ptt_hold.saturating_sub(BLOCK as u32);
            }
            if !src.ptm_down {
                src.ptm_hold = src.ptm_hold.saturating_sub(BLOCK as u32);
            }
            src.gated = gated;
            let (gl, gr) = if (s.balance - 0.5).abs() > 0.01 {
                let b = s.balance.clamp(0.0, 1.0);
                (((1.0 - b) * std::f32::consts::FRAC_PI_2).sin(), (b * std::f32::consts::FRAC_PI_2).sin())
            } else {
                (1.0, 1.0)
            };
            let vol = if s.muted || gated { 0.0 } else { s.volume.max(0.0) };
            for f in buf.as_chunks_mut::<2>().0 {
                if s.mono {
                    let m = (f[0] + f[1]) * 0.5;
                    f[0] = m;
                    f[1] = m;
                }
                f[0] *= gl;
                f[1] *= gr;
            }
            if let Some((from, to)) = &duck
                && ducker.as_ref().is_some_and(|d| d.cfg.music.contains(&id))
                && (from - 1.0).abs().max((to - 1.0).abs()) > 1e-4
            {
                // Smoothly across the block, no click.
                for (i, f) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let g = from + (to - from) * i as f32 / BLOCK as f32;
                    f[0] *= g;
                    f[1] *= g;
                }
            }
            // Not-a-number or infinite samples (a broken device, file or plugin) are made
            // silent before and after the filters: one NaN summed into a track makes the whole
            // mix NaN, and Rust's clamp keeps NaN as NaN (OBS clears NaN the same way,
            // `clamp_audio_output`). Before the filters too, so one can't poison a filter's state.
            finite(&mut buf);
            if byp.1 > 0 {
                // The dry bypass goes through the same steps, so it joins the filter's sound at
                // the level it is heard at.
                for f in bbuf.as_chunks_mut::<2>().0 {
                    if s.mono {
                        let m = (f[0] + f[1]) * 0.5;
                        f[0] = m;
                        f[1] = m;
                    }
                    f[0] *= gl;
                    f[1] *= gr;
                }
                if let Some((from, to)) = &duck
                    && ducker.as_ref().is_some_and(|d| d.cfg.music.contains(&id))
                    && (from - 1.0).abs().max((to - 1.0).abs()) > 1e-4
                {
                    for (i, f) in bbuf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                        let g = from + (to - from) * i as f32 / BLOCK as f32;
                        f[0] *= g;
                        f[1] *= g;
                    }
                }
                finite(&mut bbuf);
            }
            if !src.filters.is_empty() {
                fl.clear();
                fr.clear();
                for f in buf.as_chunks::<2>().0 {
                    fl.push(f[0]);
                    fr.push(f[1]);
                }
                for (idx, f) in src.filters.iter_mut().enumerate() {
                    let route = src.key_routes.get(idx).copied().unwrap_or(KeyRoute::SelfKey);
                    match route {
                        KeyRoute::SelfKey => f.process([&mut fl, &mut fr]),
                        KeyRoute::Slot(slot) => {
                            let (kl, kr) = &*key_bufs[slot];
                            f.process_sidechain([&mut fl, &mut fr], [kl.as_slice(), kr.as_slice()]);
                        }
                        KeyRoute::Silence => {
                            f.process_sidechain([&mut fl, &mut fr], [&SILENCE_KEY, &SILENCE_KEY]);
                        }
                    }
                    // Between filters too: one that makes NaN mustn't poison the next one's state.
                    finite(&mut fl);
                    finite(&mut fr);
                }
                for (i, f) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    f[0] = fl[i];
                    f[1] = fr[i];
                }
                finite(&mut buf);
            }
            if byp.1 > 0 {
                // The filter's first output is silence: the dry sound plays in its place, then
                // eases into the filter's own (sample-accurate, inside the block). The two are
                // the same sound at the same moment, so the weights add to one (equal-power
                // weights would swell a sound that is in both by up to 3 dB).
                let ease = ((BYPASS_EASE_S * f64::from(rate)).ceil() as usize).max(1);
                for i in 0..byp.1 {
                    let k = src.bypass_done + i;
                    let w = if k < src.bypass_pure {
                        0.0
                    } else {
                        let j = (k - src.bypass_pure) as f32;
                        (0.5 * (1.0 - (std::f32::consts::PI * (j + 0.5) / ease as f32).cos())).clamp(0.0, 1.0)
                    };
                    let at = (byp.0 + i) * 2;
                    for c in 0..2 {
                        buf[at + c] = bbuf[at + c] * (1.0 - w) + buf[at + c] * w;
                    }
                }
                src.bypass_done += byp.1;
                if src.bypass.is_empty() {
                    src.bypass_pure = 0;
                    src.bypass_done = 0;
                }
            }
            // When a filter was added, removed, or reordered mid-stream (e.g. toggling the eye
            // icon in the UI), ease out the step discontinuity over 4 ms so it never pops.
            if src.filter_declick {
                src.filter_declick = false;
                let [pl, pr] = src.last_post_filter;
                if (pl != 0.0 || pr != 0.0) && buf.len() >= 2 && buf[0].is_finite() && buf[1].is_finite() {
                    let dl = pl - buf[0];
                    let dr = pr - buf[1];
                    const FILTER_DECLICK: usize = 192;
                    let fade = FILTER_DECLICK.min(BLOCK);
                    for (i, f) in buf.as_chunks_mut::<2>().0.iter_mut().take(fade).enumerate() {
                        let w = 0.5 * (1.0 + (std::f32::consts::PI * i as f32 / fade as f32).cos());
                        f[0] += dl * w;
                        f[1] += dr * w;
                    }
                }
            }
            if let Some(last) = buf.as_chunks::<2>().0.last() {
                src.last_post_filter =
                    [if last[0].is_finite() { last[0] } else { 0.0 }, if last[1].is_finite() { last[1] } else { 0.0 }];
            }
            // Sidechain tap: OBS `obs_source_output_audio` -> `source_output_audio_data` ->
            // `source_signal_audio_data` fires post-filter, pre-fader, and `sidechain_capture`
            // writes silence when muted. Sample-aligned within this block; zero allocations.
            if let Some(slot) = src.key_slot {
                let (kl, kr) = &mut *key_bufs[slot];
                if s.muted || gated {
                    kl.fill(0.0);
                    kr.fill(0.0);
                } else {
                    for (i, frame) in buf.as_chunks::<2>().0.iter().enumerate() {
                        kl[i] = frame[0];
                        kr[i] = frame[1];
                    }
                }
            }
            let (mut peak, mut sq) = ([0f32; 2], [0f32; 2]);
            // A volume that isn't a number (a bad saved setting) is silence, not NaN.
            let vol = if vol.is_finite() { vol } else { 0.0 };
            // A mute, an unmute or the fader moved changes the gain over 8 ms (like the filters'
            // parameters), not in one step at the block's edge: that was a click as loud as the
            // sound. No allocation: a start, a slope and a count.
            let from = src.gain_applied.unwrap_or(vol);
            src.gain_applied = Some(vol);
            let ramp = if (from - vol).abs() > f32::EPSILON {
                ((rate as f32 * GAIN_RAMP_S) as usize).clamp(1, BLOCK)
            } else {
                0
            };
            let slope = if ramp > 0 { (vol - from) / ramp as f32 } else { 0.0 };
            for (i, f) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let g = if i < ramp { from + slope * (i + 1) as f32 } else { vol };
                f[0] = clip_wide(f[0] * g);
                f[1] = clip_wide(f[1] * g);
                for c in 0..2 {
                    peak[c] = peak[c].max(f[c].abs());
                    sq[c] += f[c] * f[c];
                }
            }
            // Between samples too, for meters only (the ducker hears sample peaks: turning true
            // peak on mustn't change when music ducks; CodeRabbit).
            let mut shown = peak;
            if true_peak {
                let phases = tp_phases();
                for f in buf.as_chunks::<2>().0 {
                    for c in 0..2 {
                        let h = &mut src.tp[c];
                        h.rotate_left(1);
                        h[TP_TAPS - 1] = f[c];
                        for ph in phases {
                            let v: f32 = ph.iter().zip(h.iter()).map(|(a, b)| a * b).sum();
                            shown[c] = shown[c].max(v.abs());
                        }
                    }
                }
            }
            src.levels = Levels {
                peak: [db(peak[0]), db(peak[1])],
                rms: [db((sq[0] / BLOCK as f32).sqrt()), db((sq[1] / BLOCK as f32).sqrt())],
                input_peak: input.map(db),
                gated,
            };
            src.recent.push_back(Measured { peak: shown, sq, input });
            if src.recent.len() > METER_BLOCKS {
                src.recent.pop_front();
            }
            // OBS `OBS_SOURCE_DO_NOT_SELF_MONITOR`: an output capture sharing the monitoring device
            // that doesn't exclude our own playback must not feed into monitoring (feedback howl).
            let suppress_self =
                src.output_capture.as_ref().is_some_and(|oc| !oc.excludes_self && mon_dev == Some(oc.device.as_str()));
            if s.monitoring != Monitoring::Off && !suppress_self {
                for (o, v) in monitor.iter_mut().zip(&buf) {
                    *o += v;
                }
            }
            let effective_tracks =
                if s.monitoring != Monitoring::Off && !suppress_self { s.tracks & !dup_tracks_mask } else { s.tracks };
            for (t, track) in tracks.iter_mut().enumerate() {
                if s.monitoring != Monitoring::Only && effective_tracks & (1 << t) != 0 {
                    for (o, v) in track.iter_mut().zip(&buf) {
                        *o += v;
                    }
                }
            }
        }
        if let Some(loud) = self.loudness.as_mut() {
            for (track, l) in tracks.iter_mut().zip(loud.iter_mut()) {
                // A sum that overflowed (huge gains) mustn't reach the loudness filters' state.
                finite(track);
                l.process(track);
            }
        }
        for track in &mut tracks {
            for v in track.iter_mut() {
                *v = clip(*v);
            }
        }
        for v in &mut monitor {
            *v = clip(*v);
        }
        self.scratch.buf = buf;
        self.scratch.bbuf = bbuf;
        self.scratch.fl = fl;
        self.scratch.fr = fr;
        Block { ts_ns: start, tracks, monitor }
    }
}

#[cfg(test)]
mod tests {

    /// Every edit of a collection sets each source's filters again: one whose settings didn't
    /// change keeps running (a VST's helper isn't started again), a changed one is made anew.
    #[test]
    fn unchanged_filters_keep_running() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static MADE: AtomicU32 = AtomicU32::new(0);
        #[derive(Debug)]
        struct Counted;
        impl crate::afx::SoundPlugin for Counted {
            fn process(&mut self, _ch: [&mut [f32]; 2]) -> bool {
                true
            }
        }
        fn make(_slot: u32, _rate: u32) -> Option<Box<dyn crate::afx::SoundPlugin>> {
            MADE.fetch_add(1, Ordering::SeqCst);
            Some(Box::new(Counted))
        }
        crate::afx::set_plugin_maker(make);
        let mut m = Mixer::new(48_000, 0);
        m.set(1, SourceSettings::default());
        let vst = AudioEffect::Plugin { slot: 900 };
        let gain = AudioEffect::Gain { db: -3.0 };
        m.set_effects(1, &[gain, vst]);
        assert_eq!(MADE.load(Ordering::SeqCst), 1);
        m.set_effects(1, &[vst, gain]);
        assert_eq!(MADE.load(Ordering::SeqCst), 1, "the same plugin, moved: still the one running");
        m.set_effects(1, &[AudioEffect::Plugin { slot: 901 }]);
        assert_eq!(MADE.load(Ordering::SeqCst), 2, "another plugin: made");
    }
    /// True peak finds the peak between samples: a full-scale sine at a quarter of the rate,
    /// sampled half-way between its peaks (every sample ±0.707), reads about 0 dBTP, its sample
    /// peak -3 dB.
    #[test]
    fn true_peak_sees_between_samples() {
        let sine: Vec<f32> = (0..4800)
            .flat_map(|i| {
                let v = (std::f32::consts::FRAC_PI_2 * i as f32 + std::f32::consts::FRAC_PI_4).sin();
                [v, v]
            })
            .collect();
        let read = |tp: bool| {
            let mut m = Mixer::new(48_000, 0);
            m.set(1, SourceSettings::default());
            m.set_true_peak(tp);
            let _ = m.mix_until(1_000_000_000);
            m.push(1, &sine, 2, 48_000, 1_000_000_000);
            let _ = m.mix_until(1_100_000_000);
            m.levels(1).map(|l| l.peak[0]).unwrap_or(f32::NEG_INFINITY)
        };
        let sample = read(false);
        assert!((sample + 3.0).abs() < 0.2, "sample peak about -3 dB: {sample}");
        let tp = read(true);
        assert!(tp.abs() < 0.5, "true peak about 0 dB: {tp}");
    }

    /// Low latency sound: the wait starts at 20 ms and a late device can't push it past 60 ms
    /// (its late start is cut instead); off again, the usual wait.
    /// A USB capture card's sound came over a second late (19 Nov, live): the mixer waited a
    /// second for it, and the mic came out late and broken with it. Now that source alone is
    /// timed by when its sound comes; the wait stays short and the mic plays on, unbroken.
    #[test]
    fn a_far_too_late_source_never_holds_the_mix_back() {
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set(1, SourceSettings::default());
        m.set(2, SourceSettings::default());
        let t0 = 10_000_000_000u64;
        let _ = m.mix_until(t0);
        let piece = 960; // 20 ms
        let mut heard = Vec::new();
        for k in 0..100u64 {
            let at = t0 + k * 20_000_000;
            // The mic, on time; the card, 1.2 s late.
            m.push(1, &[0.25; 1920], 2, 48_000, at);
            m.push(2, &[0.25; 1920], 2, 48_000, at.saturating_sub(1_200_000_000));
            for b in m.mix_until(at + 20_000_000) {
                heard.extend(b.tracks[0].iter().step_by(2).copied());
            }
            let _ = piece;
        }
        assert!(m.latency_ns() < 400_000_000, "the mix waits {} ms", m.latency_ns() / 1_000_000);
        // After the start, the mic is always there (0.25, plus the card's once it's retimed).
        let steady = &heard[heard.len() / 2..];
        assert!(
            steady.iter().all(|v| *v >= 0.24),
            "the mic broke up: {} silent samples",
            steady.iter().filter(|v| **v < 0.24).count()
        );
    }

    /// A sync offset that moves a source half a second earlier is meant, not lateness: it isn't
    /// undone by retiming (CodeRabbit). And a source whose times come right again after being
    /// retimed loses the correction (else it'd stay late by it for good).
    #[test]
    fn a_sync_offset_isnt_lateness_and_a_retimed_source_comes_back() {
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set(1, SourceSettings { sync_offset_ns: -500_000_000, ..SourceSettings::default() });
        let t0 = 10_000_000_000u64;
        let _ = m.mix_until(t0);
        m.push(1, &[0.1; 1920], 2, 48_000, t0);
        assert_eq!(m.sources[&1].adjust_ns, 0, "the offset kept");
        m.set(2, SourceSettings::default());
        m.push(2, &[0.1; 1920], 2, 48_000, t0 - 1_200_000_000);
        assert!(m.sources[&2].adjust_ns > 1_000_000_000, "retimed");
        m.push(2, &[0.1; 1920], 2, 48_000, t0 + 20_000_000);
        assert_eq!(m.sources[&2].adjust_ns, 0, "on time again: no correction");
    }

    /// A device that hands over a burst earlier than what's queued (by more than the smoothing)
    /// replaces only what's queued from its time on; the start of the queue isn't thrown away
    /// (that was the tearing, 19 Nov).
    #[test]
    fn a_burst_earlier_than_whats_queued_keeps_the_queue() {
        let mut m = Mixer::new(48_000, 300_000_000);
        m.set(1, SourceSettings::default());
        let t0 = 10_000_000_000u64;
        let _ = m.mix_until(t0);
        // 200 ms queued from t0, then a piece stamped 100 ms before its end.
        m.push(1, &[0.5; 2 * 9600], 2, 48_000, t0);
        m.push(1, &[0.5; 2 * 4800], 2, 48_000, t0 + 100_000_000);
        let mut heard = Vec::new();
        for b in m.mix_until(t0 + 300_000_000 + 200_000_000) {
            heard.extend(b.tracks[0].iter().step_by(2).copied());
        }
        let sounding = heard.iter().filter(|v| **v > 0.4).count();
        assert!(sounding >= 9000, "the first 100 ms and the burst both heard: {sounding} samples");
    }

    #[test]
    fn low_latency_keeps_the_wait_short() {
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set(1, SourceSettings::default());
        m.set_low_latency(true);
        assert_eq!(m.latency_ns(), 20_000_000);
        let t0 = 10_000_000_000u64;
        let _ = m.mix_until(t0);
        // A piece half a second late.
        m.push(1, &[0.1; 960], 2, 48_000, t0 - 500_000_000);
        assert!(m.latency_ns() <= 60_000_000, "capped: {}", m.latency_ns());
        m.set_low_latency(false);
        assert_eq!(m.latency_ns(), 40_000_000, "the usual start again");
    }

    /// A filter whose settings change keeps running (its state, a plugin's helper), and a
    /// filter that holds sound back makes the mixer wait that much longer at once.
    #[test]
    fn a_changed_filter_keeps_running_and_its_delay_is_waited_for() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static MADE: AtomicU32 = AtomicU32::new(0);
        #[derive(Debug)]
        struct Slow;
        impl crate::afx::SoundPlugin for Slow {
            fn process(&mut self, _ch: [&mut [f32]; 2]) -> bool {
                true
            }
            fn latency_frames(&self) -> usize {
                4800
            }
            fn retune(&mut self, _e: &AudioEffect) -> bool {
                true
            }
        }
        fn make(_strength: u32) -> Option<Box<dyn crate::afx::SoundPlugin>> {
            MADE.fetch_add(1, Ordering::SeqCst);
            Some(Box::new(Slow))
        }
        crate::afx::set_denoiser(make);
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set(1, SourceSettings::default());
        let made = MADE.load(Ordering::SeqCst);
        m.set_effects(1, &[AudioEffect::Denoise { strength: 100 }, AudioEffect::Gain { db: 0.0 }]);
        if MADE.load(Ordering::SeqCst) == made {
            // Another test set the app's denoiser first (it's set once): nothing to check here.
            return;
        }
        assert_eq!(m.latency_ns(), 140_000_000, "40 ms + the filter's 100 ms, at once");
        // Another source with the same filter: the wait covers either, not both added up.
        m.set(2, SourceSettings::default());
        m.set_effects(2, &[AudioEffect::Denoise { strength: 100 }]);
        assert_eq!(m.latency_ns(), 140_000_000, "the most one source needs");
        let made = MADE.load(Ordering::SeqCst);
        for s in [90, 80, 70] {
            m.set_effects(1, &[AudioEffect::Denoise { strength: s }, AudioEffect::Gain { db: -3.0 }]);
        }
        assert_eq!(MADE.load(Ordering::SeqCst), made, "no new model for a new strength");
        assert_eq!(m.latency_ns(), 140_000_000, "the same delay: no more waiting");
    }

    /// A filter that holds sound back (as the strong noise removal does, 40 ms) is made up
    /// for: a click filed at a moment comes out at that moment, not the filter's delay later.
    #[test]
    fn a_filters_delay_is_made_up_for() {
        #[derive(Debug)]
        struct Late([std::collections::VecDeque<f32>; 2]);
        impl crate::afx::SoundPlugin for Late {
            fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
                for (c, q) in ch.into_iter().zip(self.0.iter_mut()) {
                    for v in c.iter_mut() {
                        q.push_back(*v);
                        *v = q.pop_front().unwrap_or(0.0);
                    }
                }
                true
            }
            fn latency_frames(&self) -> usize {
                480
            }
        }
        let rate = 48_000u32;
        let mut m = Mixer::new(rate, 200_000_000);
        m.set(1, SourceSettings::default());
        // 480 frames (10 ms) of delay on each side.
        let late = Late([std::iter::repeat_n(0.0, 480).collect(), std::iter::repeat_n(0.0, 480).collect()]);
        if let Some(s) = m.sources.get_mut(&1) {
            s.filters = vec![AudioFilter::with_plugin(Box::new(late), rate)];
        }
        let t0 = 5_000_000_000u64;
        let click_at = t0 + 500_000_000;
        let mut pcm = vec![0f32; 48_000 * 2];
        pcm[24_000 * 2] = 1.0;
        pcm[24_000 * 2 + 1] = 1.0;
        let _ = m.mix_until(t0 + 200_000_000);
        m.push(1, &pcm, 2, rate, t0);
        let mut heard = None;
        for b in m.mix_until(t0 + 1_400_000_000) {
            if let Some(i) = b.tracks[0].as_chunks::<2>().0.iter().position(|f| f[0] > 0.5) {
                heard = Some(b.ts_ns + i as u64 * 1_000_000_000 / u64::from(rate));
            }
        }
        let heard = heard.unwrap_or_else(|| panic!("the click came out"));
        assert!(heard.abs_diff(click_at) < 100_000, "the click {} µs off", (heard as i64 - click_at as i64) / 1000);
    }

    /// A 440 Hz tone captured as ScreenCaptureKit hands it over (20 ms pieces, each 20 to 45 ms
    /// after it played, taken once a video frame at 30 fps) comes out whole: no break in it.
    #[test]
    fn late_pieces_of_sound_come_out_whole() {
        let rate = 48_000u32;
        let mut m = Mixer::new(rate, 40_000_000);
        m.set(1, SourceSettings::default());
        let t0 = 10_000_000_000u64;
        let piece = 960usize;
        let mut made = 0usize; // frames of the tone handed to the mixer
        let mut out: Vec<f32> = Vec::new();
        let frame_ns = 1_000_000_000 / 30;
        let mut now = t0;
        while now < t0 + 6_000_000_000 {
            now += frame_ns;
            // Every piece that has arrived by now (its end, plus 20 to 45 ms).
            loop {
                let start_ns = t0 + made as u64 * 1_000_000_000 / u64::from(rate);
                let end_ns = start_ns + piece as u64 * 1_000_000_000 / u64::from(rate);
                let late = 20_000_000 + (made / piece % 6) as u64 * 5_000_000;
                if end_ns + late > now {
                    break;
                }
                let pcm: Vec<f32> = (made..made + piece)
                    .flat_map(|i| {
                        let v = (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5;
                        [v, v]
                    })
                    .collect();
                m.push(1, &pcm, 2, rate, start_ns);
                made += piece;
            }
            for b in m.mix_until(now) {
                out.extend(b.tracks[0].as_chunks::<2>().0.iter().map(|f| f[0]));
            }
        }
        // From where the tone starts, skip the first second (the mixer settles), then no
        // sample may jump more than the tone itself can.
        let first = out.iter().position(|v| v.abs() > 0.01).unwrap_or(0) + rate as usize;
        let tone = &out[first..out.len() - 4096];
        let lim = 0.5 * std::f32::consts::TAU * 440.0 / rate as f32 * 1.5;
        let breaks = tone.windows(2).filter(|w| (w[1] - w[0]).abs() > lim).count();
        assert!(tone.len() > rate as usize * 3, "{} frames out", tone.len());
        assert_eq!(breaks, 0, "breaks in the tone; latency now {} ms", m.latency_ns() / 1_000_000);
    }

    /// A filter that holds sound back by `n` frames (the strong noise removal: 3968 = 82.7 ms),
    /// priming with silence as it does.
    #[derive(Debug)]
    struct LateBy(usize, [std::collections::VecDeque<f32>; 2], f32);
    impl LateBy {
        fn new(n: usize) -> Self {
            Self::with_gain(n, 1.0)
        }
        /// Its output is its (delayed) input times `gain`: like the denoiser, whose sound is not
        /// the level of what went in.
        fn with_gain(n: usize, gain: f32) -> Self {
            Self(n, [std::iter::repeat_n(0.0, n).collect(), std::iter::repeat_n(0.0, n).collect()], gain)
        }
    }
    impl crate::afx::SoundPlugin for LateBy {
        fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
            for (c, q) in ch.into_iter().zip(self.1.iter_mut()) {
                for v in c.iter_mut() {
                    q.push_back(*v);
                    *v = q.pop_front().unwrap_or(0.0) * self.2;
                }
            }
            true
        }
        fn latency_frames(&self) -> usize {
            self.0
        }
        fn hand_over(&mut self) -> Option<[Vec<f32>; 2]> {
            Some([self.1[0].drain(..).collect(), self.1[1].drain(..).collect()])
        }
    }

    /// A 440 Hz tone mixed as it comes, a 3968-frame filter attached at 2 s and taken off at
    /// 4 s (as toggling DeepFilterNet does): what came out.
    fn toggled_tone(attach: bool, detach: bool) -> Vec<f32> {
        toggled_tone_with(attach, detach, 1.0)
    }

    /// `toggled_tone` with a filter whose sound is `gain` times what went in.
    fn toggled_tone_with(attach: bool, detach: bool, gain: f32) -> Vec<f32> {
        let rate = 48_000u32;
        let mut m = Mixer::new(rate, 40_000_000);
        m.set(1, SourceSettings::default());
        let t0 = 10_000_000_000u64;
        let piece = 960usize;
        let (mut made, mut out, mut now) = (0usize, Vec::new(), t0);
        let (mut on, mut off) = (!attach, !detach);
        if !attach {
            if let Some(src) = m.sources.get_mut(&1) {
                src.filters = vec![AudioFilter::with_plugin(Box::new(LateBy::with_gain(3968, gain)), rate)];
            }
            m.raise_latency_for_filters(0, 3968);
        }
        while now < t0 + 7_000_000_000 {
            now += 1_000_000_000 / 30;
            if !on && now >= t0 + 2_000_000_000 {
                on = true;
                if let Some(s) = m.sources.get_mut(&1) {
                    s.filters = vec![AudioFilter::with_plugin(Box::new(LateBy::with_gain(3968, gain)), rate)];
                    s.filter_declick = true;
                }
                m.raise_latency_for_filters(0, 3968);
                m.reserve_bypass(1);
                m.begin_bypass(1, 3968);
            }
            if !off && now >= t0 + 4_000_000_000 {
                off = true;
                m.set_effects(1, &[]);
            }
            loop {
                let start_ns = t0 + made as u64 * 1_000_000_000 / u64::from(rate);
                if start_ns + 20_000_000 + 10_000_000 > now {
                    break;
                }
                let pcm: Vec<f32> = (made..made + piece)
                    .flat_map(|i| {
                        let v = (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5;
                        [v, v]
                    })
                    .collect();
                m.push(1, &pcm, 2, rate, start_ns);
                made += piece;
            }
            for b in m.mix_until(now) {
                out.extend(b.tracks[0].as_chunks::<2>().0.iter().map(|f| f[0]));
            }
        }
        out
    }

    /// Steps bigger than the tone's own, and holes (a run of silence), in what came out from
    /// `from` (frames after the tone starts) for `len` frames.
    fn glitches(out: &[f32], from: usize, len: usize) -> (Vec<usize>, Vec<usize>) {
        let first = out.iter().position(|v| v.abs() > 0.01).unwrap_or(0);
        let tone = &out[first + from..first + from + len];
        let lim = 0.5 * std::f32::consts::TAU * 440.0 / 48_000.0 * 1.5;
        let steps = tone.windows(2).enumerate().filter(|(_, w)| (w[1] - w[0]).abs() > lim).map(|(i, _)| i).collect();
        let (mut holes, mut run) = (Vec::new(), 0);
        for (i, v) in tone.iter().enumerate() {
            run = if v.abs() < 1e-4 { run + 1 } else { 0 };
            if run == 12 {
                holes.push(i);
            }
        }
        (steps, holes)
    }

    /// Taking a filter that holds sound back (DeepFilterNet, 3968 frames) out used to leave
    /// 82.7 ms of silence and a step: the source is filed that much later at once. What the
    /// filter still held plays first now, so the tone goes on whole.
    #[test]
    fn taking_a_latency_filter_off_leaves_the_tone_whole() {
        let out = toggled_tone(false, true);
        // The filter is on from the start, off at 4 s: look from 1 s to 6 s.
        let (steps, holes) = glitches(&out, 48_000, 5 * 48_000);
        assert!(steps.is_empty() && holes.is_empty(), "steps at {steps:?}, holes at {holes:?}");
    }

    /// The same with a filter whose sound is quieter than its input (the denoiser's is, by
    /// about 8 dB): the dry sound that takes over is at another level, so a join that just
    /// switches steps (S57: a step of 0.6 at every switch-off in the recording). It must ease.
    #[test]
    fn taking_a_quieter_latency_filter_off_does_not_step() {
        for gain in [0.4, 0.15] {
            let out = toggled_tone_with(false, true, gain);
            // The first second has the filter's prime and the mixer settling; judge 1-6 s but
            // not the stretch where the level changes over its ease (4 ms around the join).
            let (steps, holes) = glitches(&out, 48_000, 5 * 48_000);
            assert!(steps.is_empty() && holes.is_empty(), "gain {gain}: steps at {steps:?}, holes at {holes:?}");
        }
    }

    /// Putting such a filter on used to leave a hole and a step: the source is filed 82.7 ms
    /// earlier at once (the queued sound was cut) and the filter's first 82.7 ms of output is
    /// silence. The first 3968 frames of the queue now play dry in place of that silence while
    /// the filter's input starts after them, eased into its sound.
    #[test]
    fn putting_a_latency_filter_on_leaves_the_tone_whole() {
        let out = toggled_tone(true, false);
        let (steps, holes) = glitches(&out, 48_000, 5 * 48_000);
        assert!(steps.is_empty() && holes.is_empty(), "steps at {steps:?}, holes at {holes:?}");
    }

    /// After a filter that holds sound back goes on mid-stream, a click still comes out at its
    /// own moment (the sync offset ends where it was before), and the sound before and after is
    /// whole.
    #[test]
    fn a_click_after_a_latency_filter_goes_on_is_on_time() {
        let rate = 48_000u32;
        let mut m = Mixer::new(rate, 40_000_000);
        m.set(1, SourceSettings::default());
        let t0 = 10_000_000_000u64;
        let click_at = t0 + 4_000_000_000;
        let piece = 960usize;
        let (mut made, mut now, mut heard) = (0usize, t0, None);
        let mut on = false;
        while now < t0 + 6_000_000_000 {
            now += 1_000_000_000 / 30;
            if !on && now >= t0 + 2_000_000_000 {
                on = true;
                if let Some(s) = m.sources.get_mut(&1) {
                    s.filters = vec![AudioFilter::with_plugin(Box::new(LateBy::new(3968)), rate)];
                }
                m.raise_latency_for_filters(0, 3968);
                m.reserve_bypass(1);
                m.begin_bypass(1, 3968);
            }
            loop {
                let start_ns = t0 + made as u64 * 1_000_000_000 / u64::from(rate);
                if start_ns + 30_000_000 > now {
                    break;
                }
                let mut pcm = vec![0f32; piece * 2];
                let click = (click_at.saturating_sub(start_ns)) * u64::from(rate) / 1_000_000_000;
                if click_at >= start_ns && (click as usize) < piece {
                    pcm[click as usize * 2] = 1.0;
                    pcm[click as usize * 2 + 1] = 1.0;
                }
                m.push(1, &pcm, 2, rate, start_ns);
                made += piece;
            }
            for b in m.mix_until(now) {
                if let Some(i) = b.tracks[0].as_chunks::<2>().0.iter().position(|f| f[0] > 0.5) {
                    heard = Some(b.ts_ns + i as u64 * 1_000_000_000 / u64::from(rate));
                }
            }
        }
        let heard = heard.unwrap_or_else(|| panic!("the click came out"));
        assert!(heard.abs_diff(click_at) < 100_000, "the click {} µs off", (heard as i64 - click_at as i64) / 1000);
    }

    /// A 440 Hz tone in 20 ms pieces, each arriving when `arrives(start, end)` (ns) says, mixed
    /// once a 30 fps frame for `secs` → the first channel of what came out, and the wait after.
    fn tone_through(secs: u64, arrives: impl Fn(u64, u64) -> u64) -> (Vec<f32>, Mixer) {
        let rate = 48_000u32;
        let mut m = Mixer::new(rate, 40_000_000);
        m.set(1, SourceSettings::default());
        let t0 = 10_000_000_000u64;
        let piece = 960usize;
        let (mut made, mut out, mut now) = (0usize, Vec::new(), t0);
        while now < t0 + secs * 1_000_000_000 {
            now += 1_000_000_000 / 30;
            loop {
                let start_ns = t0 + made as u64 * 1_000_000_000 / u64::from(rate);
                let end_ns = start_ns + piece as u64 * 1_000_000_000 / u64::from(rate);
                if arrives(start_ns - t0, end_ns) > now {
                    break;
                }
                let pcm: Vec<f32> = (made..made + piece)
                    .flat_map(|i| {
                        let v = (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5;
                        [v, v]
                    })
                    .collect();
                m.push(1, &pcm, 2, rate, start_ns);
                made += piece;
            }
            for b in m.mix_until(now) {
                out.extend(b.tracks[0].as_chunks::<2>().0.iter().map(|f| f[0]));
            }
        }
        (out, m)
    }

    /// Sound held up 300 ms (the strong noise removal falling behind, 15 Nov) and then handed
    /// over all at once: the mixer waits about that much longer, once. It grew to a second
    /// before, each late piece adding its lateness on top of the last one's.
    #[test]
    fn a_burst_of_late_sound_raises_the_wait_once() {
        let stall = 3_000_000_000..3_300_000_000u64;
        let (_, m) = tone_through(5, |start, end| {
            if stall.contains(&start) { 10_000_000_000 + stall.end + 20_000_000 } else { end + 20_000_000 }
        });
        let ms = m.latency_ns() / 1_000_000;
        assert!((300..=420).contains(&ms), "waits {ms} ms");
    }

    /// A late piece raises the wait; a mix that then makes nothing (the clock hasn't reached the
    /// new wait yet) mustn't make the next late piece look later still: the wait stays put.
    #[test]
    fn a_mix_that_makes_nothing_doesnt_raise_the_wait_again() {
        let rate = 48_000u32;
        let mut m = Mixer::new(rate, 40_000_000);
        m.set(1, SourceSettings::default());
        let t0 = 10_000_000_000u64;
        let _ = m.mix_until(t0);
        let piece: Vec<f32> = vec![0.1; 960 * 2];
        // (Under RETIME_NS: later than that, the source is timed by when it comes instead.)
        m.push(1, &piece, 2, rate, t0 - 240_000_000);
        let raised = m.latency_ns();
        assert!(raised > 200_000_000, "the late piece raised the wait: {raised}");
        assert!(m.mix_until(t0 + 33_000_000).is_empty(), "nothing to mix yet");
        m.push(1, &piece, 2, rate, t0 - 220_000_000);
        assert_eq!(m.latency_ns(), raised, "a piece no later than the first one changes nothing");
    }

    /// After a calm stretch the wait comes back down to what the sound needs (never under where
    /// it started), and the tone goes on whole through the change.
    #[test]
    fn after_a_calm_stretch_the_wait_comes_back_down() {
        let stall = 3_000_000_000..3_300_000_000u64;
        let (out, m) = tone_through(50, |start, end| {
            if stall.contains(&start) { 10_000_000_000 + stall.end + 20_000_000 } else { end + 20_000_000 }
        });
        let ms = m.latency_ns() / 1_000_000;
        assert!((40..=120).contains(&ms), "waits {ms} ms");
        // From 5 s on (after the stall's lost start), no break: shrinking loses nothing.
        let from = 48_000 * 5;
        let tone = &out[from..out.len() - 4096];
        let lim = 0.5 * std::f32::consts::TAU * 440.0 / 48_000.0 * 1.5;
        let breaks = tone.windows(2).filter(|w| (w[1] - w[0]).abs() > lim).count();
        assert_eq!(breaks, 0);
    }

    use super::*;

    const RATE: u32 = 48_000;

    fn tone(hz: f32, frames: usize, rate: u32, amp: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = (i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * amp;
                [v, v]
            })
            .collect()
    }

    fn ns(frames: usize, rate: u32) -> u64 {
        frames as u64 * 1_000_000_000 / u64::from(rate)
    }

    /// Everything mixed between two times, track 1, interleaved.
    fn run(m: &mut Mixer, from: u64, to: u64) -> Vec<f32> {
        m.mix_until(from);
        m.mix_until(to).into_iter().flat_map(|b| b.tracks[0].clone()).collect()
    }

    #[test]
    fn sound_lands_at_its_time() {
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings::default());
        let start = 1_000_000_000;
        m.mix_until(start);
        // A click 100 ms in.
        let mut pcm = vec![0f32; 9600 * 2];
        pcm[4800 * 2] = 0.5;
        m.push(1, &pcm, 2, RATE, start);
        // The first call made the block ending at `start`; these begin there.
        let out: Vec<f32> = m.mix_until(start + 200_000_000).into_iter().flat_map(|b| b.tracks[0].clone()).collect();
        let at = out.iter().position(|v| *v > 0.4).unwrap_or(usize::MAX) / 2;
        assert_eq!(at, 4800);
    }

    #[test]
    fn jitter_is_smoothed_and_jumps_are_kept() {
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings::default());
        let t0 = 5_000_000_000;
        m.mix_until(t0);
        m.push(1, &vec![0.25; 960 * 2], 2, RATE, t0);
        // 5 ms late: still continues the first piece, no gap.
        m.push(1, &vec![0.25; 960 * 2], 2, RATE, t0 + ns(960, RATE) + 5_000_000);
        let out = run(&mut m, t0, t0 + 100_000_000);
        let first = out.iter().position(|v| *v > 0.2).unwrap_or(0);
        let lit = out[first..].iter().take_while(|v| **v > 0.2).count() / 2;
        assert_eq!(lit, 1920, "two pieces back to back");
    }

    #[test]
    fn a_gap_ahead_is_silence_and_nothing_queued_is_lost() {
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings::default());
        let t0 = 2_000_000_000;
        m.mix_until(t0);
        // Two 20 ms beeps 200 ms apart, both sent before either is mixed (as a source that
        // schedules ahead, or one that went quiet in between).
        m.push(1, &vec![0.3; 960 * 2], 2, RATE, t0);
        m.push(1, &vec![0.3; 960 * 2], 2, RATE, t0 + 200_000_000);
        let out = run(&mut m, t0, t0 + 400_000_000);
        let lit: Vec<usize> = out.chunks(2).enumerate().filter(|(_, f)| f[0] > 0.2).map(|(i, _)| i).collect();
        assert_eq!(lit.len(), 1920, "both beeps are heard");
        assert_eq!(lit[960] - lit[0], 9600, "the second 200 ms after the first");
    }

    #[test]
    fn a_tap_hears_one_source_until_it_is_muted() {
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings::default());
        m.set(2, SourceSettings::default());
        let tap = m.tap(1);
        m.push(1, &[0.2, 0.4, 0.2, 0.4], 2, RATE, 0);
        m.push(2, &[0.9, 0.9], 2, RATE, 0);
        assert_eq!(tap.take(), [0.3, 0.3], "source 1 only, as mono");
        m.set(1, SourceSettings { muted: true, ..Default::default() });
        m.push(1, &[0.2, 0.4], 2, RATE, 10_000);
        assert!(tap.take().is_empty(), "nothing while muted");
        drop(tap);
        m.push(1, &[0.2, 0.4], 2, RATE, 20_000);
        assert!(m.taps.iter().all(|(_, t)| t.strong_count() == 0), "a dropped tap gets nothing");
    }

    /// The meter's window: the slow one still holds a loud block for about 300 ms, the fast one
    /// (the dashboard's) only for a few blocks, so a bar that falls is not smeared by the engine.
    #[test]
    fn the_fast_meter_window_is_a_few_blocks_and_the_slow_one_is_unchanged() {
        const { assert!(METER_FAST_BLOCKS * BLOCK * 1000 / 48_000 <= 50) } // about 40 ms
        const { assert!(METER_BLOCKS * BLOCK * 1000 / 48_000 >= 290) } // about 300 ms
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings::default());
        let t0 = 1_000_000_000;
        m.mix_until(t0);
        // One loud block, then silence for the fast window and a block more.
        let mut pcm = vec![0.5f32; BLOCK * 2];
        pcm.extend(vec![0.0f32; BLOCK * 2 * (METER_FAST_BLOCKS + 1)]);
        m.push(1, &pcm, 2, RATE, t0);
        m.mix_until(t0 + ns(BLOCK * (METER_FAST_BLOCKS + 2), RATE));
        let slow = m.levels(1).unwrap_or_default();
        let fast = m.levels_over(1, METER_FAST_BLOCKS).unwrap_or_default();
        assert!(slow.peak[0] > -10.0, "the slow window still holds the loud block: {}", slow.peak[0]);
        assert!(fast.peak[0] == f32::NEG_INFINITY || fast.peak[0] < -60.0, "the fast one let it go: {}", fast.peak[0]);
    }

    #[test]
    fn volume_mute_balance_mono_and_tracks() {
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings { volume: 0.5, tracks: 0b10, ..Default::default() });
        m.set(2, SourceSettings { balance: 1.0, ..Default::default() });
        m.set(3, SourceSettings { muted: true, ..Default::default() });
        let t0 = 1_000_000_000;
        m.mix_until(t0);
        for id in 1..=3 {
            m.push(id, &vec![0.4; 4096 * 2], 2, RATE, t0);
        }
        let blocks = m.mix_until(t0 + ns(3072, RATE));
        let b = &blocks[1];
        // Track 1: source 2 only, all right.
        assert!(b.tracks[0][0].abs() < 1e-6, "left gone");
        assert!((b.tracks[0][1] - 0.4).abs() < 1e-4);
        // Track 2: source 1 at half.
        assert!((b.tracks[1][0] - 0.2).abs() < 1e-6);
        let lv = m.levels(1).unwrap_or_default();
        assert!((lv.peak[0] - db(0.2)).abs() < 0.01);
        let muted = m.levels(3).unwrap_or_default();
        assert_eq!(muted.peak[0], f32::NEG_INFINITY);

        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings { mono: true, ..Default::default() });
        m.mix_until(t0);
        let lr: Vec<f32> = (0..4096).flat_map(|_| [1.0, 0.0]).collect();
        m.push(1, &lr, 2, RATE, t0);
        let b = &m.mix_until(t0 + ns(3072, RATE))[1];
        assert!((b.tracks[0][0] - 0.5).abs() < 1e-6 && (b.tracks[0][1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn sync_offset_moves_sound_later() {
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings { sync_offset_ns: 50_000_000, ..Default::default() });
        let t0 = 2_000_000_000;
        m.mix_until(t0);
        let mut pcm = vec![0f32; 4800 * 2];
        pcm[0] = 0.5;
        m.push(1, &pcm, 2, RATE, t0);
        let out = run(&mut m, t0, t0 + 200_000_000);
        let at = out.iter().position(|v| *v > 0.4).unwrap_or(usize::MAX) / 2;
        // 50 ms is 2400 frames.
        assert_eq!(at, 2400);
    }

    #[test]
    fn other_rates_are_resampled_keeping_pitch_and_time() {
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings::default());
        let t0 = 3_000_000_000;
        m.mix_until(t0);
        // One second of 1 kHz at 44.1 kHz, in uneven pieces.
        let pcm = tone(1000.0, 44_100, 44_100, 0.5);
        let mut at = 0;
        for piece in pcm.chunks(882) {
            m.push(1, piece, 2, 44_100, t0 + ns(at, 44_100));
            at += piece.len() / 2;
        }
        let out = run(&mut m, t0, t0 + 1_100_000_000);
        // Count rising zero crossings in the middle half second: about 500.
        let left: Vec<f32> = out.chunks(2).map(|f| f[0]).collect();
        let mid = &left[12_000..36_000];
        let rises = mid.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        assert!((499..=501).contains(&rises), "{rises} crossings in 0.5 s");
        let peak = mid.iter().fold(0f32, |a, v| a.max(v.abs()));
        assert!((0.48..0.52).contains(&peak), "{peak}");
    }

    #[test]
    fn filters_run_before_volume() {
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings { volume: 0.5, ..Default::default() });
        m.set_effects(1, &[AudioEffect::Gain { db: 6.0206 }, AudioEffect::InvertPolarity]);
        let t0 = 1_000_000_000;
        m.mix_until(t0);
        m.push(1, &vec![0.4; 4096 * 2], 2, RATE, t0);
        let b = &m.mix_until(t0 + ns(3072, RATE))[1];
        // 0.4 doubled, turned over, halved.
        assert!((b.tracks[0][0] + 0.4).abs() < 1e-3, "{}", b.tracks[0][0]);
    }

    #[test]
    fn monitoring_hears_without_or_with_sending() {
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings { monitoring: Monitoring::Only, ..Default::default() });
        m.set(2, SourceSettings { monitoring: Monitoring::AndOutput, ..Default::default() });
        m.set(3, SourceSettings::default());
        let t0 = 1_000_000_000;
        m.mix_until(t0);
        m.push(1, &vec![0.1; 4096 * 2], 2, RATE, t0);
        m.push(2, &vec![0.2; 4096 * 2], 2, RATE, t0);
        m.push(3, &vec![0.4; 4096 * 2], 2, RATE, t0);
        let b = &m.mix_until(t0 + ns(3072, RATE))[1];
        assert!((b.monitor[0] - 0.3).abs() < 1e-6, "1 and 2 heard: {}", b.monitor[0]);
        assert!((b.tracks[0][0] - 0.6).abs() < 1e-6, "2 and 3 sent: {}", b.tracks[0][0]);
    }

    /// Music (2) ducks under a voice (1) a block after it's heard, and comes back after.
    #[test]
    fn music_ducks_under_the_voice() {
        let mut m = Mixer::new(RATE, 0);
        // The voice on track 2 only, the music on track 1 only: track 1 is the music alone.
        m.set(1, SourceSettings { tracks: 0b10, ..Default::default() });
        m.set(2, SourceSettings { tracks: 0b01, ..Default::default() });
        m.set_ducking(Some(crate::level::Ducking { voices: vec![1], music: vec![2], ..Default::default() }));
        let t0 = 1_000_000_000;
        m.mix_until(t0);
        let secs = 3 * RATE as usize;
        m.push(2, &vec![0.1; secs * 2], 2, RATE, t0);
        // A second of talking, then quiet.
        m.push(1, &tone(300.0, RATE as usize, RATE, 0.3), 2, RATE, t0);
        let blocks = m.mix_until(t0 + ns(secs, RATE));
        let music = |b: &Block| b.tracks[0][0];
        // Half a second in, the music is down about 12 dB.
        let mid = &blocks[blocks.len() / 6];
        assert!((music(mid) - 0.025).abs() < 0.005, "ducked: {}", music(mid));
        // Two seconds after the voice stopped (it fades back), it's back.
        assert!((music(blocks.last().unwrap()) - 0.1).abs() < 0.005, "back: {}", music(blocks.last().unwrap()));
    }

    #[test]
    fn mixing_clamps_instead_of_wrapping() {
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings::default());
        m.set(2, SourceSettings::default());
        let t0 = 1_000_000_000;
        m.mix_until(t0);
        m.push(1, &vec![0.8; 4096 * 2], 2, RATE, t0);
        m.push(2, &vec![0.8; 4096 * 2], 2, RATE, t0);
        let b = &m.mix_until(t0 + ns(3072, RATE))[1];
        assert_eq!(b.tracks[0][10], 1.0);
    }

    #[test]
    fn blocks_keep_the_sample_clock_for_an_hour() {
        let mut m = Mixer::new(RATE, 20_000_000);
        let mut made = 0usize;
        let mut first = None;
        // An hour, asked for at uneven moments.
        let mut now = 10_000_000_000u64;
        while now < 10_000_000_000 + 3_600_000_000_000 {
            for b in m.mix_until(now) {
                first.get_or_insert(b.ts_ns);
                made += 1;
            }
            now += 7_000_000 + (made as u64 % 5) * 3_000_000;
        }
        let elapsed = now - 20_000_000 - first.unwrap_or(0);
        let expected = (u128::from(elapsed) * u128::from(RATE) / 1_000_000_000) as usize / BLOCK;
        assert!(made.abs_diff(expected) <= 1, "{made} blocks for {expected}");
    }

    /// A source that's added to the mix after its settings were set keeps them.
    #[test]
    fn a_source_that_opens_late_keeps_its_settings() {
        let mut m = Mixer::new(48_000, 0);
        let muted = SourceSettings { muted: true, volume: 0.25, ..SourceSettings::default() };
        m.set(7, muted);
        m.ensure(7);
        assert_eq!(m.sources[&7].settings, muted, "the settings it had stay");
        m.ensure(8);
        assert_eq!(m.sources[&8].settings, SourceSettings::default(), "a new one gets the defaults");
    }

    /// A source sending NaN or infinity is silent in the mix; the others are still heard.
    #[test]
    fn a_source_sending_nan_never_breaks_the_mix() {
        let mut m = Mixer::new(48_000, 0);
        m.set(1, SourceSettings::default());
        m.set(2, SourceSettings::default());
        let t0 = 1_000_000_000;
        // The mix starts at t0, so the next block is the one with the bad samples.
        let _ = m.mix_until(t0);
        let mut bad = vec![0.5f32; BLOCK * 2];
        bad[10] = f32::NAN;
        bad[11] = f32::INFINITY;
        m.push(1, &bad, 2, 48_000, t0);
        m.push(2, &[0.25f32; BLOCK * 2], 2, 48_000, t0);
        let blocks = m.mix_until(t0 + block_ns(48_000));
        let all: Vec<f32> = blocks.iter().flat_map(|b| b.tracks[0].iter().copied()).collect();
        assert!(!all.is_empty());
        assert!(all.iter().all(|v| v.is_finite()), "no NaN reaches the track");
        assert!(all.iter().any(|v| *v > 0.2), "the good source is still heard");
    }

    /// Feeds 10 ms pieces of a steady tone on time from `from` to `to` (ms), skipping
    /// `stall` (ms range), mixing as it goes; returns the blocks mixed.
    fn feed(m: &mut Mixer, from: u64, to: u64, stall: Option<(u64, u64)>) -> Vec<Block> {
        let ms = 1_000_000u64;
        let piece = [0.5f32; 480 * 2];
        let mut out = Vec::new();
        let mut t = from;
        while t < to {
            if !stall.is_some_and(|(a, b)| t >= a && t < b) {
                m.push(1, &piece, 2, 48_000, t * ms);
            }
            t += 10;
            out.extend(m.mix_until(t * ms));
        }
        out
    }

    /// A source that stalls longer than the mixer's wait comes back whole: after a moment,
    /// every block has it all the way through (before 20 Nov each piece was placed back where
    /// the stall began, already mixed, and lost its start: choppy for good).
    #[test]
    fn a_source_that_stalls_a_moment_comes_back_whole() {
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set_low_latency(true);
        m.set(1, SourceSettings::default());
        let _ = feed(&mut m, 1_000, 3_000, None);
        let _ = feed(&mut m, 3_000, 3_200, Some((3_000, 3_055)));
        let after = feed(&mut m, 3_200, 5_000, None);
        let holes: usize = after
            .iter()
            .skip(10)
            .map(|b| b.tracks[0].as_chunks::<2>().0.iter().filter(|f| f[0].abs() < 0.01).count())
            .sum();
        assert_eq!(holes, 0, "no silent frames once it's back");
    }

    /// A sync offset under the smoothing still moves the sound (before 20 Nov it was smoothed
    /// away and did nothing).
    #[test]
    fn a_small_sync_offset_moves_the_sound() {
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set(1, SourceSettings::default());
        let _ = feed(&mut m, 1_000, 2_000, None);
        let src = &m.sources[&1];
        let end = |s: &Source| s.start_ns.unwrap_or(0) + (s.queue.len() / 2) as u64 * 1_000_000_000 / 48_000;
        let before = end(src);
        m.set(1, SourceSettings { sync_offset_ns: 50_000_000, ..SourceSettings::default() });
        m.push(1, &[0.5f32; 480 * 2], 2, 48_000, 2_000_000_000);
        let after = end(&m.sources[&1]);
        // The piece (10 ms) now ends at its time plus the offset: the 50 ms is filled before it,
        // nothing queued was thrown away.
        assert!(
            after.abs_diff(2_060_000_000) < 1_000_000_000 / 48_000,
            "placed at its time plus the offset, to the frame: ends {after} (the queue ended at {before})"
        );
    }

    /// A NaN burst through a stateful filter, with loudness on: the next clean block is heard
    /// and finite (the filter and loudness states weren't poisoned); a filter that overflows
    /// to infinity only silences its own source.
    #[test]
    fn nan_and_infinity_never_poison_filters_or_loudness() {
        let mut m = Mixer::new(48_000, 0);
        m.set_loudness(Some(-14.0));
        m.set(1, SourceSettings::default());
        m.set_effects(
            1,
            &[AudioEffect::Compressor {
                ratio: 4.0,
                threshold_db: -18.0,
                attack_ms: 6.0,
                release_ms: 60.0,
                output_db: 0.0,
                sidechain: None,
            }],
        );
        m.set(2, SourceSettings::default());
        m.set_effects(2, &[AudioEffect::Gain { db: 1000.0 }]);
        let t0 = 1_000_000_000;
        let step = block_ns(48_000);
        let _ = m.mix_until(t0);
        let mut bad = vec![0.25f32; BLOCK * 2];
        bad[0] = f32::NAN;
        bad[1] = f32::INFINITY;
        m.push(1, &bad, 2, 48_000, t0);
        m.push(2, &[0.25f32; BLOCK * 4], 2, 48_000, t0);
        let _ = m.mix_until(t0 + step);
        m.push(1, &[0.25f32; BLOCK * 2], 2, 48_000, t0 + step);
        let after = m.mix_until(t0 + 2 * step);
        let all: Vec<f32> = after.iter().flat_map(|b| b.tracks[0].iter().chain(&b.monitor).copied()).collect();
        assert!(!all.is_empty() && all.iter().all(|v| v.is_finite()), "finite after the burst");
        assert!(all.iter().any(|v| v.abs() > 0.01), "the clean source is still heard");
    }

    /// Filters set while a source is still opening (not in the mix yet) aren't lost.
    #[test]
    fn filters_set_before_the_source_opens_are_kept() {
        let mut m = Mixer::new(48_000, 0);
        m.set_effects(9, &[AudioEffect::Gain { db: -6.0 }]);
        assert_eq!(m.sources.get(&9).map(|s| s.filters.len()), Some(1));
    }

    #[test]
    fn loopback_sharing_monitor_device_never_self_monitors_or_doubles_output() {
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings { monitoring: Monitoring::AndOutput, tracks: 0b11, ..Default::default() });
        m.set(2, SourceSettings { monitoring: Monitoring::Only, tracks: 0b01, ..Default::default() });
        m.set(3, SourceSettings { monitoring: Monitoring::AndOutput, tracks: 0b01, ..Default::default() });
        m.set_monitor_device(Some("speakers".into()));
        m.set_output_capture(3, Some(OutputCapture { device: "speakers".into(), excludes_self: false }));
        let t0 = 1_000_000_000;
        m.mix_until(t0);
        m.push(1, &vec![0.2; 4096 * 2], 2, RATE, t0);
        m.push(2, &vec![0.1; 4096 * 2], 2, RATE, t0);
        m.push(3, &vec![0.4; 4096 * 2], 2, RATE, t0);
        let b = &m.mix_until(t0 + ns(3072, RATE))[1];
        // Loopback 3 must NOT appear in b.monitor (DO_NOT_SELF_MONITOR): 1 + 2 = 0.3, not 0.7.
        assert!((b.monitor[0] - 0.3).abs() < 1e-6, "loopback excluded from monitor: {}", b.monitor[0]);
        // Track 1 is covered by loopback 3, so monitored source 1 is silenced on track 1 (0.4, not 0.6).
        assert!((b.tracks[0][0] - 0.4).abs() < 1e-6, "track 1 deduplicated: {}", b.tracks[0][0]);
        // Track 2 is NOT in loopback 3's tracks, so source 1 still reaches track 2.
        assert!((b.tracks[1][0] - 0.2).abs() < 1e-6, "track 2 kept for source 1: {}", b.tracks[1][0]);
    }

    #[test]
    fn self_excluding_or_inactive_loopback_does_not_silence_monitored_sources() {
        let t0 = 1_000_000_000;
        // (1) Self-excluding loopback (macOS SCK / Windows process loopback): source 1 stays in track 1.
        let mut m = Mixer::new(RATE, 0);
        m.set(1, SourceSettings { monitoring: Monitoring::AndOutput, tracks: 0b01, ..Default::default() });
        m.set(2, SourceSettings { tracks: 0b01, ..Default::default() });
        m.set_monitor_device(Some("speakers".into()));
        m.set_output_capture(2, Some(OutputCapture { device: "speakers".into(), excludes_self: true }));
        m.mix_until(t0);
        m.push(1, &vec![0.2; 4096 * 2], 2, RATE, t0);
        m.push(2, &vec![0.4; 4096 * 2], 2, RATE, t0);
        let b = &m.mix_until(t0 + ns(3072, RATE))[1];
        assert!((b.tracks[0][0] - 0.6).abs() < 1e-6, "self-excluding keeps both: {}", b.tracks[0][0]);

        // (2) Muted, off-air (inactive), or different-device loopback never silences source 1.
        for (muted, active, dev) in [(true, true, "speakers"), (false, false, "speakers"), (false, true, "headphones")]
        {
            let mut m = Mixer::new(RATE, 0);
            m.set(1, SourceSettings { monitoring: Monitoring::AndOutput, tracks: 0b01, ..Default::default() });
            m.set(2, SourceSettings { muted, tracks: 0b01, ..Default::default() });
            m.set_monitor_device(Some("speakers".into()));
            m.set_output_capture(2, Some(OutputCapture { device: dev.into(), excludes_self: false }));
            m.set_active(2, active);
            m.mix_until(t0);
            m.push(1, &vec![0.2; 4096 * 2], 2, RATE, t0);
            m.push(2, &vec![0.4; 4096 * 2], 2, RATE, t0);
            let b = &m.mix_until(t0 + ns(3072, RATE))[1];
            let want = if muted { 0.2 } else { 0.6 };
            assert!(
                (b.tracks[0][0] - want).abs() < 1e-6,
                "muted={muted} active={active} dev={dev}: {}",
                b.tracks[0][0]
            );
        }
    }

    #[test]
    fn has_monitored_tracks_active_unmuted_non_suppressed_sources() {
        let mut m = Mixer::new(RATE, 0);
        assert!(!m.has_monitored());
        m.set(1, SourceSettings { monitoring: Monitoring::Only, ..Default::default() });
        assert!(m.has_monitored());
        m.set(1, SourceSettings { monitoring: Monitoring::Only, muted: true, ..Default::default() });
        assert!(!m.has_monitored());
        m.set(1, SourceSettings { monitoring: Monitoring::Only, ..Default::default() });
        m.set_monitor_device(Some("speakers".into()));
        m.set_output_capture(1, Some(OutputCapture { device: "speakers".into(), excludes_self: false }));
        assert!(!m.has_monitored(), "self-monitor-suppressed loopback doesn't wake monitor");
        m.set_output_capture(1, None);
        m.set_active(1, false);
        assert!(!m.has_monitored(), "inactive source doesn't wake monitor");
    }

    /// After the computer slept an hour, one call mixes about one block, not an hour of them.
    #[test]
    fn a_clock_jump_after_sleep_never_mixes_the_whole_gap() {
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set(1, SourceSettings::default());
        let t0 = 10_000_000_000;
        let _ = m.mix_until(t0);
        m.push(1, &[0.5f32; 480 * 2], 2, 48_000, t0);
        let blocks = m.mix_until(t0 + 3_600_000_000_000);
        assert!(blocks.len() <= 2, "{} blocks mixed in one call", blocks.len());
        // And it goes on normally from there.
        let later = m.mix_until(t0 + 3_600_000_000_000 + 100_000_000);
        assert!((4..=6).contains(&later.len()), "{}", later.len());
    }

    struct DriftSummary {
        follows: bool,
        has_resampler: bool,
        slope_ppm: f64,
        phase_err_ns: f64,
        worst_step: f32,
        longest_quiet: u32,
        end_phase_err_ms: f64,
    }

    fn run_drift_scenario(arrival: bool, ppm: f64, secs: u64, stall_at_sec: Option<u64>) -> DriftSummary {
        const HZ: f64 = 440.0;
        const AMP: f32 = 0.5;
        let mut m = Mixer::new(RATE, 40_000_000);
        m.set(1, SourceSettings { tracks: 0b01, ..SourceSettings::default() });
        m.set(2, SourceSettings { tracks: 0b10, ..SourceSettings::default() });
        let t0 = 10_000_000_000u64;
        let mut now = t0;
        let mut card_deliv = 0u64;
        let mut mic_deliv = 0u64;
        let mut next_burst = t0;
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut next_rng = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let per_sec = f64::from(RATE) * (1.0 + ppm / 1e6);
        let mut last: Option<f32> = None;
        let mut worst_step = 0.0f32;
        let mut quiet = 0u32;
        let mut longest_quiet = 0u32;
        let mut end_phase_err_ms = 0.0f64;
        let mut did_stall = false;
        for _tick in 0..(secs * 60) {
            now += 16_666_667;
            while t0 + (mic_deliv + 1) * 10_000_000 <= now {
                m.push(1, &[0.1f32; 480], 1, RATE, t0 + mic_deliv * 10_000_000);
                mic_deliv += 1;
            }
            if now >= next_burst {
                let total = ((now - t0) as f64 * per_sec / 1e9) as u64 / 480;
                let n = total - card_deliv;
                for k in 0..n {
                    let p = card_deliv + k;
                    let pcm: Vec<f32> = (0..480)
                        .map(|i| {
                            let t_dev = (p * 480 + i) as f64 / per_sec;
                            AMP * (2.0 * std::f64::consts::PI * HZ * t_dev).sin() as f32
                        })
                        .collect();
                    let ts = if arrival {
                        now.saturating_sub((n - k) * 10_000_000)
                    } else {
                        t0 + (p as f64 * 480.0 * 1e9 / per_sec) as u64
                    };
                    m.push(2, &pcm, 1, RATE, ts);
                }
                card_deliv = total;
                let trigger_stall = !did_stall && stall_at_sec.is_some_and(|s| now >= t0 + s * 1_000_000_000);
                let burst_ms = if trigger_stall {
                    did_stall = true;
                    450
                } else {
                    40 + next_rng() % 81
                };
                next_burst = now + burst_ms * 1_000_000;
            }
            for b in m.mix_until(now) {
                if b.ts_ns > t0 + 3_000_000_000 {
                    for (i, f) in b.tracks[1].as_chunks::<2>().0.iter().enumerate() {
                        if let Some(p) = last {
                            let d = (f[0] - p).abs();
                            if quiet < 48 && d > worst_step {
                                worst_step = d;
                            }
                        }
                        last = Some(f[0]);
                        quiet = if f[0].abs() < 0.01 { quiet + 1 } else { 0 };
                        longest_quiet = longest_quiet.max(quiet);
                        if b.ts_ns > t0 + secs.saturating_sub(2) * 1_000_000_000 && i == 0 {
                            let t_out = (b.ts_ns - t0) as f64 / 1e9;
                            let angle = 2.0 * std::f64::consts::PI * HZ * t_out;
                            let expected_v = AMP * angle.sin() as f32;
                            if angle.cos() > 0.5 && expected_v.abs() < 0.05 && f[0].abs() < 0.4 {
                                let slope = AMP * (2.0 * std::f64::consts::PI * HZ * angle.cos()) as f32;
                                end_phase_err_ms = f64::from((f[0] - expected_v) / slope) * 1000.0;
                            }
                        }
                    }
                }
            }
        }
        let s = &m.sources[&2];
        DriftSummary {
            follows: s.drift.follows,
            has_resampler: s.resampler.is_some(),
            slope_ppm: s.drift.slope * 1e6,
            phase_err_ns: s.drift.phase_err_ns,
            worst_step,
            longest_quiet,
            end_phase_err_ms,
        }
    }

    #[test]
    fn drift_is_not_engaged_by_arrival_jitter_alone() {
        let steepest = 0.5 * (2.0 * std::f64::consts::PI * 440.0 / f64::from(RATE)) as f32;
        let r = run_drift_scenario(true, 0.0, 90, None);
        assert!(!r.follows, "0 ppm jitter engaged drift tracking: slope={:.1}ppm", r.slope_ppm);
        assert!(!r.has_resampler, "0 ppm jitter should keep the resampler off");
        assert!(r.worst_step <= steepest * 1.25, "step={}", r.worst_step);
        assert!(r.longest_quiet < 120, "quiet={}", r.longest_quiet);
        assert!(r.end_phase_err_ms.abs() < 1.0, "end phase={}ms", r.end_phase_err_ms);
    }

    #[test]
    fn drift_locks_without_click_and_tracks_phase() {
        let steepest = 0.5 * (2.0 * std::f64::consts::PI * 440.0 / f64::from(RATE)) as f32;
        for (arrival, ppm) in [(true, 300.0), (false, 300.0), (false, -300.0)] {
            let r = run_drift_scenario(arrival, ppm, 45, None);
            assert!(r.follows, "drift not followed for arrival={arrival} ppm={ppm}");
            assert!(r.has_resampler, "resampler should be active once following");
            // Arrival stamps carry a 0..10 ms burst-end remainder (sigma = 10/sqrt(12) = 2.89 ms)
            // across ~80 ms bursts; with tau = 20 s EWMA and vxx ~ 161 s^2 at 45 s, slope SE is
            // ~10.2 ppm (~3.9 sigma = 40 ppm; 1.5 sigma observed), vs ~0.1 ppm for 1 ns Device stamps.
            let slope_tol = if arrival { 40.0 } else { 5.0 };
            assert!((r.slope_ppm + ppm).abs() < slope_tol, "expected ~-{ppm}ppm, got {:.1}", r.slope_ppm);
            assert!(r.worst_step <= steepest * 1.25, "click on arrival={arrival} ppm={ppm}: step={}", r.worst_step);
            assert!(r.longest_quiet < 120, "dropout on arrival={arrival} ppm={ppm}: quiet={}", r.longest_quiet);
            assert!(
                r.end_phase_err_ms.abs() < 2.0,
                "phase error {}ms on arrival={arrival} ppm={ppm}",
                r.end_phase_err_ms
            );
        }
    }

    #[test]
    fn drift_preserves_slope_across_stall() {
        let r = run_drift_scenario(false, 300.0, 50, Some(30));
        assert!(r.follows, "following should persist across a 450 ms stall");
        assert!((r.slope_ppm + 300.0).abs() < 15.0, "slope reset across stall: {:.1}ppm", r.slope_ppm);
        assert!(r.phase_err_ns.abs() < 5_000_000.0, "phase error after recovery: {}ms", r.phase_err_ns / 1e6);
    }

    #[test]
    fn sinc_warm_handoff_preserves_sample_alignment() {
        const HZ: f64 = 440.0;
        const AMP: f32 = 0.5;
        let steepest = AMP * (2.0 * std::f64::consts::PI * HZ / f64::from(RATE)) as f32;
        let sample = |k: usize| AMP * (2.0 * std::f64::consts::PI * HZ * k as f64 / f64::from(RATE)).sin() as f32;
        let mut queue = VecDeque::new();
        let mut drift = DriftTrack::default();
        for chunk in 0..3 {
            let l: Vec<f32> = (0..RESAMPLE_CHUNK).map(|i| sample(chunk * RESAMPLE_CHUNK + i)).collect();
            drift.push_tail(&l, &l);
            for v in l {
                queue.push_back(v);
                queue.push_back(v);
            }
        }
        let Some(mut rs) = SourceResampler::new_warm(RATE, RATE, &drift.tail, &mut queue) else {
            panic!("failed to allocate warm resampler");
        };
        for chunk in 3..6 {
            let l: Vec<f32> = (0..RESAMPLE_CHUNK).map(|i| sample(chunk * RESAMPLE_CHUNK + i)).collect();
            rs.pending[0].extend_from_slice(&l);
            rs.pending[1].extend_from_slice(&l);
            rs.process_ready(&mut queue, true, 0.9997);
        }
        let mut last: Option<f32> = None;
        let mut worst = 0.0f32;
        for f in queue.make_contiguous().as_chunks::<2>().0 {
            if let Some(p) = last {
                worst = worst.max((f[0] - p).abs());
            }
            last = Some(f[0]);
        }
        assert!(worst <= steepest * 1.15, "warm handoff jumped: step={worst:.4} (sine={steepest:.4})");
    }

    /// After a calm 20 s stretch (`SHRINK_AFTER_NS`), `maybe_wait_less` must never shrink the
    /// mixer's wait below `floor_ns + held_most_ns()` while a delayed filter (e.g. DeepFilterNet,
    /// 3968 frames = 82.7 ms) is active; once removed, the wait shrinks back down (M2-05).
    #[test]
    fn maybe_wait_less_never_drops_below_active_filter_latency() {
        #[derive(Debug)]
        struct Delayed(usize);
        impl crate::afx::SoundPlugin for Delayed {
            fn process(&mut self, _: [&mut [f32]; 2]) -> bool {
                true
            }
            fn latency_frames(&self) -> usize {
                self.0
            }
        }
        let floor = 40_000_000u64;
        let mut m = Mixer::new(RATE, floor);
        m.set(1, SourceSettings::default());
        let Some(src) = m.sources.get_mut(&1) else { panic!("source 1 missing") };
        src.filters.push(AudioFilter::with_plugin(Box::new(Delayed(3968)), RATE));
        let held = m.held_most_ns();
        assert!(held > 80_000_000, "expected ~82.7 ms filter delay, got {held}");
        // Case 1: already at floor + held; 25 s of silence must not drop wait below floor + held.
        m.latency_ns = floor + held;
        let mut t = 1_000_000_000u64;
        m.mix_until(t);
        for _ in 0..25 {
            t += 1_000_000_000;
            m.mix_until(t);
        }
        assert!(
            m.latency_ns() >= floor + held,
            "latency shrank below floor + filter delay: {} < {}",
            m.latency_ns(),
            floor + held
        );
        // Case 2: recover from a transient 400 ms spike down to floor + held, no lower.
        m.latency_ns = 400_000_000;
        for _ in 0..25 {
            t += 1_000_000_000;
            m.mix_until(t);
        }
        assert_eq!(m.latency_ns(), floor + held, "shrunk from spike back to floor + filter delay");
        // Case 3: once the delayed filter is removed, wait shrinks back down.
        m.set_effects(1, &[]);
        for _ in 0..25 {
            t += 1_000_000_000;
            m.mix_until(t);
        }
        assert!(m.latency_ns() < floor + held, "latency shrinks once filter is removed: {}", m.latency_ns());
    }

    #[test]
    fn rev22_m1_two_sources_same_non_mix_rate_starting_together_both_get_resamplers() {
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set_background_resamplers(true);
        m.set(1, SourceSettings { tracks: 0b01, ..SourceSettings::default() });
        m.set(2, SourceSettings { tracks: 0b10, ..SourceSettings::default() });
        let t0 = 10_000_000_000u64;
        let _ = m.mix_until(t0);
        let piece = vec![0.35f32; 441 * 2];
        // Both start at 44.1 kHz before the background thread delivers any resampler.
        m.push(1, &piece, 2, 44_100, t0);
        m.push(2, &piece, 2, 44_100, t0);
        let mut track1_energy = 0.0f32;
        let mut track2_energy = 0.0f32;
        let mut pending: Option<SpareResampler> = None;
        for k in 1..=25u64 {
            if let Some(r) = pending.take() {
                m.add_spare_resampler(r);
                m.release_held();
            }
            if let Some(rate) = m.pop_resampler_request() {
                pending = Mixer::make_spare_resampler(rate, 48_000);
            }
            let ts = t0 + k * 10_000_000;
            m.push(1, &piece, 2, 44_100, ts);
            m.push(2, &piece, 2, 44_100, ts);
            for b in m.mix_until(ts + 10_000_000) {
                track1_energy += b.tracks[0].iter().map(|v| v * v).sum::<f32>();
                track2_energy += b.tracks[1].iter().map(|v| v * v).sum::<f32>();
            }
        }
        assert!(track1_energy > 1.0, "source 1 should be heard: {track1_energy}");
        assert!(track2_energy > 1.0, "source 2 must not be permanently muted: {track2_energy}");

        // Short-alert case: both push 60 ms at start and stop pushing while resamplers arrive later.
        let mut m2 = Mixer::new(48_000, 40_000_000);
        m2.set_background_resamplers(true);
        m2.set(1, SourceSettings { tracks: 0b01, ..SourceSettings::default() });
        m2.set(2, SourceSettings { tracks: 0b10, ..SourceSettings::default() });
        let _ = m2.mix_until(t0);
        for k in 0..6u64 {
            let ts = t0 + k * 10_000_000;
            m2.push(1, &piece, 2, 44_100, ts);
            m2.push(2, &piece, 2, 44_100, ts);
        }
        let mut short2_energy = 0.0f32;
        let mut pending2: Option<SpareResampler> = None;
        for k in 6..=20u64 {
            if let Some(r) = pending2.take() {
                m2.add_spare_resampler(r);
                m2.release_held();
            }
            if let Some(rate) = m2.pop_resampler_request() {
                pending2 = Mixer::make_spare_resampler(rate, 48_000);
            }
            for b in m2.mix_until(t0 + k * 10_000_000) {
                short2_energy += b.tracks[1].iter().map(|v| v * v).sum::<f32>();
            }
        }
        assert!(short2_energy > 0.5, "short alert source 2 must still play out: {short2_energy}");
    }

    #[test]
    fn rev22_m3_full_rs_spares_evicts_and_burst_spares_do_not_over_request() {
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set_background_resamplers(true);
        // Fill rs_spares to RS_SPARES_ROOM (2 same-rate + 6 other rates).
        for rate in [8_000u32, 11_025, 16_000, 22_050, 32_000, 96_000] {
            m.add_spare_resampler(Mixer::make_spare_resampler(rate, 48_000).unwrap());
        }
        assert_eq!(m.rs_spares.len(), RS_SPARES_ROOM);
        m.set(1, SourceSettings::default());
        let t0 = 10_000_000_000u64;
        let _ = m.mix_until(t0);
        let piece = vec![0.4f32; 441 * 2];
        m.push(1, &piece, 2, 44_100, t0);
        let req = m.pop_resampler_request();
        assert_eq!(req, Some(44_100));
        m.add_spare_resampler(Mixer::make_spare_resampler(44_100, 48_000).unwrap());
        assert!(m.pop_retired_resampler().is_some(), "evicted spare should retire off-thread");
        m.release_held();
        assert!(m.pop_resampler_request().is_none(), "must not spin requesting 44.1k forever");
        let mut energy = 0.0f32;
        for k in 1..=10u64 {
            let ts = t0 + k * 10_000_000;
            m.push(1, &piece, 2, 44_100, ts);
            for b in m.mix_until(ts + 10_000_000) {
                energy += b.tracks[0].iter().map(|v| v * v).sum::<f32>();
            }
        }
        assert!(energy > 1.0, "44.1k source must be heard when rs_spares started full: {energy}");

        // Adding > SPARES sources in a single burst must not request more than SPARES.
        for id in 10..18u64 {
            m.set(id, SourceSettings::default());
        }
        assert!(m.take_spare_request() <= SPARES, "burst source adds must not over-request spares");
    }

    #[test]
    fn rev22_m4_hold_clears_on_flush_source_and_clock_jump() {
        let t0 = 10_000_000_000u64;
        let stale = vec![0.9f32; 441 * 2];
        let fresh = vec![0.2f32; 441 * 2];

        // 1. flush_source (seek/pause) clears hold.
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set_background_resamplers(true);
        m.set(1, SourceSettings::default());
        let _ = m.mix_until(t0);
        m.push(1, &stale, 2, 44_100, t0);
        m.push(1, &stale, 2, 44_100, t0 + 10_000_000);
        m.flush_source(1);
        let _ = m.pop_resampler_request();
        m.add_spare_resampler(Mixer::make_spare_resampler(44_100, 48_000).unwrap());
        m.release_held();
        let mut peak = 0.0f32;
        for k in 2..12u64 {
            let ts = t0 + k * 10_000_000;
            m.push(1, &fresh, 2, 44_100, ts);
            for b in m.mix_until(ts + 10_000_000) {
                peak = b.tracks[0].iter().fold(peak, |a, v| a.max(v.abs()));
            }
        }
        assert!(peak < 0.35, "pre-flush held audio leaked after flush_source: peak={peak}");

        // 2. MAX_CATCH_UP_NS clock jump (sleep/wake) clears hold.
        let mut m2 = Mixer::new(48_000, 40_000_000);
        m2.set_background_resamplers(true);
        m2.set(1, SourceSettings::default());
        let _ = m2.mix_until(t0);
        m2.push(1, &stale, 2, 44_100, t0);
        let wake = t0 + 3_600_000_000_000;
        let _ = m2.mix_until(wake);
        let _ = m2.pop_resampler_request();
        m2.add_spare_resampler(Mixer::make_spare_resampler(44_100, 48_000).unwrap());
        m2.release_held();
        let mut peak2 = 0.0f32;
        for k in 0..10u64 {
            let ts = wake + k * 10_000_000;
            m2.push(1, &fresh, 2, 44_100, ts);
            for b in m2.mix_until(ts + 10_000_000) {
                peak2 = b.tracks[0].iter().fold(peak2, |a, v| a.max(v.abs()));
            }
        }
        assert!(peak2 < 0.35, "pre-sleep held audio leaked after wake: peak={peak2}");
    }

    #[test]
    fn rev22_m4_hold_return_to_mix_rate_and_format_change_never_drop() {
        let t0 = 10_000_000_000u64;
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set_background_resamplers(true);
        m.set(1, SourceSettings::default());
        let _ = m.mix_until(t0);
        // Push 44.1k while resampler hasn't arrived -> enters Source::hold.
        m.push(1, &[0.9f32; 441 * 2], 2, 44_100, t0);
        // Immediately switch back to 48 kHz before any 44.1k resampler arrives.
        let mut energy = 0.0f32;
        for k in 1..15u64 {
            let ts = t0 + k * 10_000_000;
            m.push(1, &[0.3f32; 480 * 2], 2, 48_000, ts);
            for b in m.mix_until(ts + 10_000_000) {
                energy += b.tracks[0].iter().map(|v| v * v).sum::<f32>();
            }
        }
        assert!(energy > 1.0, "returning to 48 kHz while held must not drop audio: {energy}");

        // Format change 44.1k -> 96k while held must request 96k and play 96k.
        let mut m2 = Mixer::new(48_000, 40_000_000);
        m2.set_background_resamplers(true);
        m2.set(1, SourceSettings::default());
        let _ = m2.mix_until(t0);
        m2.push(1, &[0.9f32; 441 * 2], 2, 44_100, t0);
        let _ = m2.pop_resampler_request(); // drain 44.1k
        m2.push(1, &[0.25f32; 960 * 2], 2, 96_000, t0 + 10_000_000);
        assert_eq!(m2.pop_resampler_request(), Some(96_000), "must request 96 kHz resampler on format change");
        m2.add_spare_resampler(Mixer::make_spare_resampler(96_000, 48_000).unwrap());
        m2.release_held();
        let mut energy2 = 0.0f32;
        for k in 2..12u64 {
            let ts = t0 + k * 10_000_000;
            m2.push(1, &[0.25f32; 960 * 2], 2, 96_000, ts);
            for b in m2.mix_until(ts + 10_000_000) {
                energy2 += b.tracks[0].iter().map(|v| v * v).sum::<f32>();
            }
        }
        assert!(energy2 > 0.5, "96 kHz audio after format change must play: {energy2}");
    }

    #[test]
    fn rev22_m4_mono_hold_never_exceeds_resampler_tail_room() {
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set_background_resamplers(true);
        m.set(1, SourceSettings::default());
        let t0 = 10_000_000_000u64;
        let _ = m.mix_until(t0);
        let mono = vec![0.2f32; 882]; // 20 ms mono at 44.1 kHz
        for k in 0..45u64 {
            m.push(1, &mono, 1, 44_100, t0 + k * 20_000_000);
        }
        assert!(
            m.sources[&1].hold.len() <= TAIL_ROOM,
            "mono hold {} exceeded TAIL_ROOM {}",
            m.sources[&1].hold.len(),
            TAIL_ROOM
        );
        m.add_spare_resampler(Mixer::make_spare_resampler(44_100, 48_000).unwrap());
        m.release_held();
        let cap = m.sources[&1].resampler.as_ref().unwrap().pending[0].capacity();
        assert_eq!(cap, TAIL_ROOM, "mono release_held reallocated rs.pending[0] from {TAIL_ROOM} to {cap}");
    }

    #[test]
    fn rev22_m4_stale_hold_past_retime_and_channel_change_are_clean() {
        let t0 = 10_000_000_000u64;
        // 1. Hold older than RETIME_NS does not poison adjust_ns or wipe live audio.
        let mut m = Mixer::new(48_000, 40_000_000);
        m.set_background_resamplers(true);
        m.set(1, SourceSettings::default());
        let _ = m.mix_until(t0);
        // Short alert at t0, then mix advances 450 ms (> RETIME_NS = 300 ms) while afx-bg stalls.
        m.push(1, &[0.8f32; 441 * 2], 2, 44_100, t0);
        let _ = m.mix_until(t0 + 450_000_000);
        let _ = m.pop_resampler_request();
        m.add_spare_resampler(Mixer::make_spare_resampler(44_100, 48_000).unwrap());
        m.release_held();
        assert_eq!(m.sources[&1].adjust_ns, 0, "stale hold must not trip RETIME_NS and set adjust_ns");
        // Live audio at t0 + 450 ms continues cleanly without adjust_ns reset wiping the queue.
        let mut energy = 0.0f32;
        for k in 0..10u64 {
            let ts = t0 + 450_000_000 + k * 10_000_000;
            m.push(1, &[0.3f32; 441 * 2], 2, 44_100, ts);
            for b in m.mix_until(ts + 10_000_000) {
                energy += b.tracks[0].iter().map(|v| v * v).sum::<f32>();
            }
        }
        assert!(energy > 0.5, "live audio after stale hold drop must play cleanly: {energy}");

        // 2. Channel change (stereo -> mono at 44.1 kHz) while held switches cleanly to mono.
        let mut m2 = Mixer::new(48_000, 40_000_000);
        m2.set_background_resamplers(true);
        m2.set(1, SourceSettings::default());
        let _ = m2.mix_until(t0);
        m2.push(1, &[0.9f32; 441 * 2], 2, 44_100, t0);
        m2.push(1, &[0.25f32; 441], 1, 44_100, t0 + 10_000_000);
        assert_eq!(m2.sources[&1].hold_ch, 1);
        let _ = m2.pop_resampler_request();
        m2.add_spare_resampler(Mixer::make_spare_resampler(44_100, 48_000).unwrap());
        m2.release_held();
        let mut peak = 0.0f32;
        for k in 2..12u64 {
            let ts = t0 + k * 10_000_000;
            m2.push(1, &[0.25f32; 441], 1, 44_100, ts);
            for b in m2.mix_until(ts + 10_000_000) {
                peak = b.tracks[0].iter().fold(peak, |a, v| a.max(v.abs()));
            }
        }
        assert!(peak > 0.15 && peak < 0.4, "channel change while held must play new mono cleanly: peak={peak}");
    }
}

#[cfg(test)]
mod steady_state_tests {
    use super::*;

    /// Every buffer that grows with the sound, as sizes: when a steady mix stops changing these, it
    /// is not allocating (the allocator itself is counted in `tests/mix_alloc.rs`).
    fn footprint(m: &Mixer) -> Vec<usize> {
        let mut v = vec![
            m.blocks_allocated(),
            m.pool.len(),
            m.scratch.l.capacity(),
            m.scratch.r.capacity(),
            m.scratch.buf.capacity(),
            m.scratch.fl.capacity(),
            m.scratch.fr.capacity(),
        ];
        for s in m.sources.values() {
            v.push(s.queue.capacity());
            v.push(s.recent.capacity());
            if let Some(rs) = &s.resampler {
                v.extend([rs.pending[0].capacity(), rs.pending[1].capacity()]);
            }
            v.extend([s.drift.tail[0].capacity(), s.drift.tail[1].capacity()]);
        }
        v
    }

    /// A same-rate mic, a 44.1 kHz card (resampled) and a source with a filter, 20 ms at a time,
    /// meters reading true peak: after a short warm-up, 1,000 blocks change no buffer's size and
    /// take every block from the pool.
    #[test]
    fn mix_block_steady_state_reuses_buffers() {
        let mut m = Mixer::new(48_000, 40_000_000);
        for id in 1..=3 {
            m.set(id, SourceSettings::default());
        }
        m.set_effects(3, &[AudioEffect::Gain { db: -2.0 }]);
        m.set_true_peak(true);
        let mic = vec![0.2f32; 960 * 2];
        let card = vec![0.1f32; 882 * 2];
        let mut out: Vec<Block> = Vec::new();
        let t0 = 10_000_000_000u64;
        let _ = m.mix_until(t0);
        let mut step = |m: &mut Mixer, k: u64| {
            let at = t0 + k * 20_000_000;
            m.push(1, &mic, 2, 48_000, at);
            m.push(2, &card, 2, 44_100, at);
            m.push(3, &mic, 2, 48_000, at);
            m.mix_until_into(at + 20_000_000, &mut out);
            for b in out.drain(..) {
                assert!(b.tracks[0].iter().any(|v| *v != 0.0) || k < 5, "sound on track 1 at {k}");
                m.recycle(b);
            }
        };
        for k in 0..100 {
            step(&mut m, k);
        }
        let warm = footprint(&m);
        for k in 100..1100 {
            step(&mut m, k);
        }
        assert_eq!(footprint(&m), warm, "a buffer grew or a block was made in 1,000 steady blocks");
        assert!(m.blocks_allocated() <= 3, "blocks made new: {}", m.blocks_allocated());
    }

    /// Without giving blocks back, nothing changes for the caller: they are made as before.
    #[test]
    fn blocks_not_given_back_are_simply_made_new() {
        let mut m = Mixer::new(48_000, 0);
        // A new mixer holds blocks made ahead; none here, so each one is made new.
        assert_eq!(m.pool.len(), PREFILL_POOL);
        m.pool.clear();
        m.set(1, SourceSettings::default());
        let _ = m.mix_until(1_000_000_000);
        m.push(1, &[0.5; 2 * 4800], 2, 48_000, 1_000_000_000);
        let before = m.blocks_allocated();
        let blocks = m.mix_until(1_100_000_000);
        assert!(blocks.len() >= 4);
        assert_eq!(m.blocks_allocated() - before, blocks.len());
        assert!(blocks[1].tracks[0].iter().any(|v| *v > 0.4));
        // A block of another shape isn't kept.
        m.recycle(Block { ts_ns: 0, tracks: vec![vec![0.0; 4]], monitor: vec![] });
        assert_eq!(m.pool.len(), 0);
        // A recycled one comes back silent, not with the old sound in it.
        let b = blocks.into_iter().nth(1).unwrap();
        m.recycle(b);
        let again = m.take_block(5);
        assert_eq!(again.ts_ns, 5);
        assert!(again.tracks.iter().all(|t| t.iter().all(|v| *v == 0.0)) && again.monitor.iter().all(|v| *v == 0.0));
    }

    /// A filter toggled after the source went quiet (a pause, a muted mic, a stopped clip) must not
    /// fade in a stale sample from before the pause: the resumed sound is exactly what it would
    /// have been had the filter been there all along.
    #[test]
    fn toggling_a_filter_after_a_pause_adds_no_thump() {
        for eff in [AudioEffect::Gain { db: 10.0 }, AudioEffect::InvertPolarity] {
            let run = |toggle_during_pause: bool| -> Vec<f32> {
                let mut m = Mixer::new(48_000, 0);
                m.set(1, SourceSettings::default());
                if !toggle_during_pause {
                    m.set_effects(1, &[eff]);
                }
                let step = block_ns(48_000);
                let t0 = 1_000_000_000u64;
                let _ = m.mix_until(t0);
                let mut sample_idx = 0usize;
                let mut tone = |n: usize| {
                    let mut pcm = Vec::with_capacity(n * 2);
                    for _ in 0..n {
                        let s = 0.25 * (std::f32::consts::TAU * 220.0 * sample_idx as f32 / 48_000.0).sin();
                        pcm.push(s);
                        pcm.push(s);
                        sample_idx += 1;
                    }
                    pcm
                };
                let mut out = Vec::new();
                // Two blocks of tone, then six blocks with nothing pushed (the pause).
                for b in 0..8u64 {
                    if b < 2 {
                        let pcm = tone(BLOCK);
                        m.push(1, &pcm, 2, 48_000, t0 + b * step);
                    }
                    for blk in m.mix_until(t0 + (b + 1) * step) {
                        out.extend(blk.tracks[0].as_chunks::<2>().0.iter().map(|f| f[0]));
                    }
                }
                // The toggle comes during the silence, then the tone returns.
                if toggle_during_pause {
                    m.set_effects(1, &[eff]);
                }
                let pcm = tone(BLOCK);
                m.push(1, &pcm, 2, 48_000, t0 + 8 * step);
                for blk in m.mix_until(t0 + 9 * step) {
                    out.extend(blk.tracks[0].as_chunks::<2>().0.iter().map(|f| f[0]));
                }
                out[8 * BLOCK..].to_vec()
            };
            let toggled = run(true);
            let always = run(false);
            let worst = toggled.iter().zip(&always).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(worst < 1e-4, "{eff:?}: a toggle after a pause changed the resumed sound by {worst:.4}");
        }
    }

    /// The audio thread's own way in (`set_effects_nonblocking`, the D1 path) retunes a running
    /// filter in place like `set_effects` does: a gain dragged from 0 to +12 dB mid-tone is ramped
    /// (no zipper click) and needs no de-click or rebuild.
    #[test]
    fn a_gain_dragged_through_the_audio_threads_path_is_ramped_in_place() {
        let mut m = Mixer::new(48_000, 0);
        m.set(1, SourceSettings::default());
        let mut id = 0u64;
        let mut next = || {
            id += 1;
            id
        };
        let step = block_ns(48_000);
        let t0 = 1_000_000_000u64;
        let _ = m.mix_until(t0);
        let (_, missing) = m.set_effects_nonblocking(1, &[AudioEffect::Gain { db: 0.0 }], &mut next, &|_, _| false);
        assert!(missing.is_empty());
        let mut sample_idx = 0usize;
        let mut out = Vec::new();
        for b in 0..6u64 {
            if b == 3 {
                let (evicted, missing) =
                    m.set_effects_nonblocking(1, &[AudioEffect::Gain { db: 12.0 }], &mut next, &|_, _| false);
                assert!(evicted.is_empty() && missing.is_empty(), "retuned in place, nothing rebuilt");
                assert!(!m.sources.get(&1).is_some_and(|s| s.filter_declick), "no step de-click for a retune");
            }
            let mut pcm = Vec::with_capacity(BLOCK * 2);
            for _ in 0..BLOCK {
                let s = 0.2 * (std::f32::consts::TAU * 220.0 * sample_idx as f32 / 48_000.0).sin();
                pcm.push(s);
                pcm.push(s);
                sample_idx += 1;
            }
            m.push(1, &pcm, 2, 48_000, t0 + b * step);
            for blk in m.mix_until(t0 + (b + 1) * step) {
                out.extend(blk.tracks[0].as_chunks::<2>().0.iter().map(|f| f[0]));
            }
        }
        let at = 3 * BLOCK;
        let worst = out[at - 2..at + 600].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        // 220 Hz at 0.2 moves 0.006 a sample; at +12 dB (x4) at most 0.026. A jump of the gain would
        // step by up to 0.6.
        assert!(worst < 0.04, "a gain drag clicked: max step {worst:.4}");
        let end = out[out.len() - 480..].iter().fold(0.0f32, |a, x| a.max(x.abs()));
        assert!(end > 0.7, "and reached +12 dB: peak {end:.3}");
    }

    /// Runs a 0.5 tone at 220 Hz through source 1 for `blocks` blocks, calling `before(b, mixer)` ahead
    /// of each; the mixed left side and what the source's levels said (gated) at each block's end.
    fn gated_run(
        settings: SourceSettings,
        blocks: u64,
        mut before: impl FnMut(u64, &mut Mixer),
    ) -> (Vec<f32>, Vec<bool>) {
        let mut m = Mixer::new(48_000, 0);
        m.set(1, settings);
        let step = block_ns(48_000);
        let t0 = 1_000_000_000u64;
        let _ = m.mix_until(t0);
        let (mut out, mut gated, mut k) = (Vec::new(), Vec::new(), 0usize);
        for b in 0..blocks {
            before(b, &mut m);
            let mut pcm = Vec::with_capacity(BLOCK * 2);
            for _ in 0..BLOCK {
                let v = 0.5 * (std::f32::consts::TAU * 220.0 * k as f32 / 48_000.0).sin();
                pcm.push(v);
                pcm.push(v);
                k += 1;
            }
            m.push(1, &pcm, 2, 48_000, t0 + b * step);
            for blk in m.mix_until(t0 + (b + 1) * step) {
                out.extend(blk.tracks[0].as_chunks::<2>().0.iter().map(|f| f[0]));
            }
            gated.push(m.sources[&1].gated);
        }
        (out, gated)
    }

    fn block_rms(out: &[f32], b: usize) -> f32 {
        let s = &out[b * BLOCK + BLOCK / 2..(b + 1) * BLOCK];
        (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt()
    }

    /// Push-to-talk: silent until the key is held, heard while it is, and for the release delay
    /// (counted in samples: 100 ms = 4800) after it is let go; then shut again. A press inside the
    /// delay keeps it open and starts the delay again at the next release. Never a click.
    #[test]
    fn push_to_talk_opens_on_press_and_closes_after_the_release_delay() {
        let ptt = SourceSettings { push_to_talk: Some(100), ..SourceSettings::default() };
        let (out, gated) = gated_run(ptt, 16, |b, m| match b {
            2 => m.push_key(1, true, true),
            5 => m.push_key(1, true, false),
            _ => {}
        });
        // Shut before the key, open from it.
        assert!(block_rms(&out, 0) < 1e-4 && block_rms(&out, 1) < 1e-4, "silent until the key");
        assert!(gated[0] && gated[1] && !gated[2], "{gated:?}");
        for b in 3..=9 {
            assert!(block_rms(&out, b) > 0.3, "block {b} heard: held, then within the 100 ms");
        }
        // 100 ms is 4800 samples: open through block 9 (counted per 1024-sample block), shut at 10.
        assert!(block_rms(&out, 10) < 1e-3, "shut after the delay: {}", block_rms(&out, 10));
        assert!(gated[10] && gated[15]);
        // No step bigger than the tone's own slope (0.5 * 2π * 220 / 48000 = 0.0144) plus the ramp's share.
        let worst = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.04, "a click of {worst:.3}");
    }

    #[test]
    fn a_press_inside_the_release_delay_keeps_push_to_talk_open() {
        let ptt = SourceSettings { push_to_talk: Some(100), ..SourceSettings::default() };
        let (out, _) = gated_run(ptt, 20, |b, m| match b {
            1 => m.push_key(1, true, true),
            3 => m.push_key(1, true, false),
            5 => m.push_key(1, true, true),
            9 => m.push_key(1, true, false),
            _ => {}
        });
        // Down 5..9 (held), then 4800 samples (five blocks) after the second release at block 9.
        for b in 2..=13 {
            assert!(block_rms(&out, b) > 0.3, "block {b} open");
        }
        assert!(block_rms(&out, 14) < 1e-3, "shut 100 ms after the last release");
    }

    /// Push-to-mute: heard until the key is held; muted while it is and for the delay after.
    #[test]
    fn push_to_mute_shuts_on_press_and_opens_after_the_release_delay() {
        let ptm = SourceSettings { push_to_mute: Some(50), ..SourceSettings::default() };
        let (out, gated) = gated_run(ptm, 14, |b, m| match b {
            3 => m.push_key(1, false, true),
            6 => m.push_key(1, false, false),
            _ => {}
        });
        for b in 0..3 {
            assert!(block_rms(&out, b) > 0.3, "block {b} heard before the key");
        }
        // Held at blocks 3..6, then 50 ms (2400 samples) after: muted through block 8.
        for b in 4..=8 {
            assert!(block_rms(&out, b) < 1e-3, "block {b} muted: held, then within 50 ms ({})", block_rms(&out, b));
        }
        assert!(gated[4] && !gated[11]);
        assert!(block_rms(&out, 10) > 0.3, "heard again after the delay");
        let worst = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.04, "a click of {worst:.3}");
    }

    /// A key for a source that has neither switched on does nothing (the old mute-toggle shortcut
    /// handles those).
    #[test]
    fn a_push_key_does_nothing_for_a_source_without_it_on() {
        let (out, gated) = gated_run(SourceSettings::default(), 6, |b, m| match b {
            1 => m.push_key(1, true, false),
            3 => m.push_key(1, false, true),
            _ => {}
        });
        for b in 1..6 {
            assert!(block_rms(&out, b) > 0.3, "block {b} heard");
        }
        assert!(gated.iter().all(|g| !g));
    }

    /// Muting, unmuting and moving the fader mid-sound used to step the gain at a block boundary
    /// (a click as loud as the sound, the biggest spike in a measured stream). The change now
    /// ramps over 8 ms inside the mixer.
    #[test]
    fn muting_unmuting_and_the_fader_ramp_instead_of_clicking() {
        let mut m = Mixer::new(48_000, 0);
        m.set(1, SourceSettings::default());
        let step = block_ns(48_000);
        let t0 = 1_000_000_000u64;
        let _ = m.mix_until(t0);
        let mut sample_idx = 0usize;
        let mut out = Vec::new();
        // Tone 0.5 at 220 Hz; blocks 0-2 as is, 3-5 muted, 6-8 back, 9-11 the fader at 0.25.
        let settings = |muted: bool, volume: f32| SourceSettings { muted, volume, ..SourceSettings::default() };
        for b in 0..12u64 {
            match b {
                3 => m.set(1, settings(true, 1.0)),
                6 => m.set(1, settings(false, 1.0)),
                9 => m.set(1, settings(false, 0.25)),
                _ => {}
            }
            let mut pcm = Vec::with_capacity(BLOCK * 2);
            for _ in 0..BLOCK {
                let v = 0.5 * (std::f32::consts::TAU * 220.0 * sample_idx as f32 / 48_000.0).sin();
                pcm.push(v);
                pcm.push(v);
                sample_idx += 1;
            }
            m.push(1, &pcm, 2, 48_000, t0 + b * step);
            for blk in m.mix_until(t0 + (b + 1) * step) {
                out.extend(blk.tracks[0].as_chunks::<2>().0.iter().map(|f| f[0]));
            }
        }
        // A 220 Hz tone at 0.5 moves 0.015 a sample; a step of the gain at 0.5 moves it by ~0.5.
        for (name, block) in [("mute", 3usize), ("unmute", 6), ("fader", 9)] {
            let at = block * BLOCK;
            let worst = out[at - 2..at + 800].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            assert!(worst < 0.04, "{name} clicked: max step {worst:.4}");
        }
        // Gone within 10 ms, back within 10 ms at the right level.
        let muted = &out[3 * BLOCK + 480..4 * BLOCK];
        assert!(muted.iter().all(|v| v.abs() < 1e-6), "silent 10 ms after the mute");
        let back = out[7 * BLOCK..8 * BLOCK].iter().fold(0.0f32, |a, x| a.max(x.abs()));
        assert!((back - 0.5).abs() < 0.01, "unmuted level {back}");
        let faded = out[10 * BLOCK..11 * BLOCK].iter().fold(0.0f32, |a, x| a.max(x.abs()));
        assert!((faded - 0.125).abs() < 0.005, "fader level {faded}");
    }

    /// Toggling a filter on/off via `set_effects` (eye icon in UI) eases out the step jump so
    /// adding or removing `Gain +10 dB` or `InvertPolarity` mid-tone produces no click.
    #[test]
    fn toggling_filters_in_mixer_produces_no_click() {
        for eff in [AudioEffect::Gain { db: 10.0 }, AudioEffect::InvertPolarity] {
            let mut m = Mixer::new(48_000, 0);
            m.set(1, SourceSettings::default());
            let step = block_ns(48_000);
            let t0 = 1_000_000_000u64;
            let _ = m.mix_until(t0);

            // Feed continuous 220 Hz sine across 6 blocks (128 ms): blocks 0..2 dry, 2..4 wet, 4..6 dry.
            let mut sample_idx = 0usize;
            let mut make_block = || {
                let mut pcm = Vec::with_capacity(BLOCK * 2);
                for _ in 0..BLOCK {
                    let s = 0.25 * (std::f32::consts::TAU * 220.0 * sample_idx as f32 / 48_000.0).sin();
                    pcm.push(s);
                    pcm.push(s);
                    sample_idx += 1;
                }
                pcm
            };

            let mut out_l = Vec::new();
            for b in 0..6u64 {
                if b == 2 {
                    m.set_effects(1, &[eff]);
                } else if b == 4 {
                    m.set_effects(1, &[]);
                }
                let pcm = make_block();
                m.push(1, &pcm, 2, 48_000, t0 + b * step);
                for blk in m.mix_until(t0 + (b + 1) * step) {
                    out_l.extend(blk.tracks[0].as_chunks::<2>().0.iter().map(|f| f[0]));
                }
            }
            let b_on = 2 * BLOCK;
            let b_off = 4 * BLOCK;
            // Measure max sample-to-sample delta right across the on/off block boundaries.
            let step_on = out_l[b_on - 2..b_on + 200].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            let step_off = out_l[b_off - 2..b_off + 200].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            assert!(step_on < 0.08, "{eff:?} clicked on add in mixer: max delta = {step_on:.4}");
            assert!(step_off < 0.08, "{eff:?} clicked on remove in mixer: max delta = {step_off:.4}");
        }
    }

    /// Dropping taps and removing sources keeps `Mixer::taps` and `Mixer::sources` at 0 over
    /// 10,000 cycles (no dead `Weak` accumulation).
    #[test]
    fn mixer_taps_and_sources_stay_bounded_over_churn() {
        let mut m = Mixer::new(48_000, 40);
        let pcm = vec![0.1f32; BLOCK * 2];
        let step = block_ns(48_000);
        for i in 1..=10_000u64 {
            m.set(i, SourceSettings::default());
            let tap = m.tap(i);
            m.push(i, &pcm, 2, 48_000, i * step);
            drop(tap);
            m.remove(i);
        }
        assert_eq!(m.source_count(), 0);
        assert_eq!(m.tap_count(), 0);

        // Even without remove(), feeding audio prunes dropped taps on the fly.
        m.set(1, SourceSettings::default());
        for _ in 0..100 {
            let tap = m.tap(1);
            drop(tap);
        }
        m.push(1, &pcm, 2, 48_000, 10_001 * step);
        assert_eq!(m.tap_count(), 0);
    }

    /// Sidechain compression through `Mixer` matches OBS `compressor-filter.c:analyze_sidechain`
    /// sample-for-sample (with B ordered before A even when B has a higher ID), hits nominal
    /// attack and release times within 10%, and taps B's post-filter, pre-fader signal.
    #[test]
    fn sidechain_compressor_matches_obs_reference_with_attack_and_release() {
        let rate = 48_000u32;
        let step = block_ns(rate);
        let r = rate as usize;
        let music_amp = crate::afx::db_to_mul(-12.0);
        let voice_amp = crate::afx::db_to_mul(-6.0);

        // Music = ID 1, Voice = ID 2 (higher ID than dependent -> proves topological reordering).
        let mut m = Mixer::new(rate, 0);
        m.set(1, SourceSettings { tracks: 0b01, ..Default::default() });
        m.set(2, SourceSettings { tracks: 0b10, ..Default::default() });
        m.set_effects(
            1,
            &[AudioEffect::Compressor {
                ratio: 10.0,
                threshold_db: -24.0,
                attack_ms: 20.0,
                release_ms: 100.0,
                output_db: 0.0,
                sidechain: Some(2),
            }],
        );

        let total = 2 * r;
        let mut music_mono = vec![0.0f32; total];
        let mut voice_mono = vec![0.0f32; total];
        let mut music_pcm = vec![0.0f32; total * 2];
        let mut voice_pcm = vec![0.0f32; total * 2];
        for i in 0..total {
            let mv = music_amp * (std::f32::consts::TAU * 300.0 * i as f32 / rate as f32).sin();
            music_mono[i] = mv;
            music_pcm[i * 2] = mv;
            music_pcm[i * 2 + 1] = mv;
            // Voice bursts at full DC-like step from 0.25 s to 0.75 s so attack/release 63.2% points
            // are measured cleanly, then silent again until 2.0 s.
            let vv = if i >= r / 4 && i < 3 * r / 4 { voice_amp } else { 0.0 };
            voice_mono[i] = vv;
            voice_pcm[i * 2] = vv;
            voice_pcm[i * 2 + 1] = vv;
        }

        let t0 = 1_000_000_000u64;
        let _ = m.mix_until(t0);
        m.push(1, &music_pcm, 2, rate, t0);
        m.push(2, &voice_pcm, 2, rate, t0);
        let blocks = m.mix_until(t0 + (total / BLOCK) as u64 * step);
        let got_l: Vec<f32> = blocks.iter().flat_map(|b| b.tracks[0].as_chunks::<2>().0.iter().map(|f| f[0])).collect();
        assert!(got_l.len() >= r + r / 2, "expected >= 1.5 s of mixed output, got {}", got_l.len());

        // Independent OBS `compressor-filter.c:analyze_sidechain` reference.
        let ag = (-1.0f32 / (rate as f32 * 0.020)).exp();
        let rg = (-1.0f32 / (rate as f32 * 0.100)).exp();
        let slope = 1.0 - 1.0 / 10.0;
        let thresh_lin = crate::afx::db_to_mul(-24.0);
        let mut env = 0.0f32;
        let mut max_diff = 0.0f32;
        for (i, &got) in got_l.iter().enumerate() {
            let x = voice_mono[i].abs();
            env = if env < x { x + ag * (env - x) } else { x + rg * (env - x) };
            let g = if env > 0.0 {
                crate::afx::db_to_mul((slope * (-24.0 - crate::afx::mul_to_db(env))).min(0.0))
            } else {
                1.0
            };
            let expected = music_mono[i] * g;
            max_diff = max_diff.max((got - expected).abs());
        }
        assert!(max_diff < 1e-4, "mixer sidechain diverged from OBS reference: max_diff={max_diff}");

        // Measure 63.2% attack and release times directly from the mixer's gain reduction.
        // At 0.25 s voice steps 0 -> voice_amp; 63.2% of envelope is voice_amp * (1 - 1/e).
        let burst_start = r / 4;
        let burst_end = 3 * r / 4;
        let target_atk_env = voice_amp * (1.0 - (-1.0f32).exp());
        let atk_idx = (burst_start..burst_end)
            .find(|&i| {
                if music_mono[i].abs() < 0.05 {
                    return false;
                }
                let gain = (got_l[i] / music_mono[i]).abs().clamp(1e-6, 1.0);
                let rec_env = thresh_lin * gain.powf(-1.0 / slope);
                rec_env >= target_atk_env
            })
            .expect("sidechain attack crossed 63.2%");
        let atk_ms = (atk_idx - burst_start) as f32 * 1000.0 / rate as f32;
        assert!((atk_ms - 20.0).abs() <= 2.0, "sidechain attack 63%: {atk_ms:.2} ms (want 20 ± 10%)");

        // At 0.75 s voice drops voice_amp -> 0; 63.2% decay leaves env = voice_amp / e (> thresh_lin).
        let target_rel_env = voice_amp * (-1.0f32).exp();
        let rel_idx = (burst_end..got_l.len())
            .find(|&i| {
                if music_mono[i].abs() < 0.05 {
                    return false;
                }
                let gain = (got_l[i] / music_mono[i]).abs().clamp(1e-6, 1.0);
                let rec_env = thresh_lin * gain.powf(-1.0 / slope);
                rec_env <= target_rel_env
            })
            .expect("sidechain release crossed 63.2%");
        let rel_ms = (rel_idx - burst_end) as f32 * 1000.0 / rate as f32;
        assert!((rel_ms - 100.0).abs() <= 10.0, "sidechain release 63%: {rel_ms:.2} ms (want 100 ± 10%)");

        // Post-filter, pre-fader tap check (OBS `obs_source_output_audio` -> `source_signal_audio_data`):
        // Voice has Gain +12 dB filter AND fader turned to 0.0 (unmuted). Music MUST still duck
        // because the tap is post-filter and pre-fader, while track 2 (Voice) is silent!
        let mut pf = Mixer::new(rate, 0);
        pf.set(1, SourceSettings { tracks: 0b01, ..Default::default() });
        pf.set(2, SourceSettings { volume: 0.0, muted: false, tracks: 0b10, ..Default::default() });
        pf.set_effects(2, &[AudioEffect::Gain { db: 12.0 }]);
        pf.set_effects(
            1,
            &[AudioEffect::Compressor {
                ratio: 10.0,
                threshold_db: -24.0,
                attack_ms: 1.0,
                release_ms: 60.0,
                output_db: 0.0,
                sidechain: Some(2),
            }],
        );
        let _ = pf.mix_until(t0);
        // Voice input is -28 dBFS (below -24 dB threshold on its own), boosted by +12 dB filter to -16 dBFS!
        let quiet_voice = vec![crate::afx::db_to_mul(-28.0); BLOCK * 10 * 2];
        let steady_music = vec![0.5f32; BLOCK * 10 * 2];
        pf.push(1, &steady_music, 2, rate, t0);
        pf.push(2, &quiet_voice, 2, rate, t0);
        let pf_blocks = pf.mix_until(t0 + 10 * step);
        let last = pf_blocks.last().expect("mixed blocks");
        assert!(last.tracks[1].iter().all(|v| v.abs() < 1e-6), "voice fader at 0.0 must silence voice on its track");
        // -16 dBFS key over -24 dB threshold at 10:1 -> -7.2 dB reduction -> 0.5 * 10^(-7.2/20) ≈ 0.218.
        let ducked = last.tracks[0][BLOCK * 2 - 2].abs();
        assert!(
            (ducked - 0.5 * crate::afx::db_to_mul(-7.2)).abs() < 0.02,
            "music must duck by post-filter pre-fader key even with fader=0: got {ducked:.4}"
        );
    }

    /// A key that stops delivering (a stalled device, a paused file: the source stays, its ring
    /// runs dry) is silence for the blocks it misses: the music comes back up as the compressor
    /// releases, it never stays ducked on the key's last block. A key that arrives half a block
    /// late ducks only from where its samples start.
    #[test]
    fn sidechain_key_that_stops_or_arrives_late_never_leaves_the_music_ducked() {
        let rate = 48_000u32;
        let step = block_ns(rate);
        let mut m = Mixer::new(rate, 0);
        m.set(1, SourceSettings { tracks: 0b01, ..Default::default() });
        m.set(2, SourceSettings { volume: 0.0, tracks: 0b10, ..Default::default() });
        m.set_effects(
            1,
            &[AudioEffect::Compressor {
                ratio: 10.0,
                threshold_db: -30.0,
                attack_ms: 1.0,
                release_ms: 60.0,
                output_db: 0.0,
                sidechain: Some(2),
            }],
        );
        let t0 = 1_000_000_000u64;
        let _ = m.mix_until(t0);
        let blocks = 200usize;
        m.push(1, &vec![0.5f32; BLOCK * blocks * 2], 2, rate, t0);
        // The key: 10 blocks of loud tone, then nothing at all (its ring runs dry).
        m.push(2, &vec![0.4f32; BLOCK * 10 * 2], 2, rate, t0);
        let out = m.mix_until(t0 + blocks as u64 * step);
        let level = |i: usize| out[i].tracks[0].iter().fold(0f32, |a, v| a.max(v.abs()));
        assert!(level(8) < 0.2, "ducked while the key plays: {}", level(8));
        assert!(level(blocks - 5) > 0.49, "music back after the key stops: {}", level(blocks - 5));

        // A key 512 frames late: the first half block is unducked, the rest ducked.
        let mut m = Mixer::new(rate, 0);
        m.set(1, SourceSettings { tracks: 0b01, ..Default::default() });
        m.set(2, SourceSettings { volume: 0.0, tracks: 0b10, ..Default::default() });
        m.set_effects(
            1,
            &[AudioEffect::Compressor {
                ratio: 20.0,
                threshold_db: -40.0,
                attack_ms: 1.0,
                release_ms: 60.0,
                output_db: 0.0,
                sidechain: Some(2),
            }],
        );
        let _ = m.mix_until(t0);
        m.push(1, &vec![0.5f32; BLOCK * 20 * 2], 2, rate, t0);
        let late = 512u64;
        m.push(2, &vec![0.4f32; BLOCK * 20 * 2], 2, rate, t0 + late * 1_000_000_000 / u64::from(rate));
        let out = m.mix_until(t0 + 20 * step);
        let first = &out[0].tracks[0];
        let head = first[..(late as usize - 8) * 2].iter().fold(0f32, |a, v| a.max(v.abs()));
        let tail = first[(late as usize + 400) * 2..].iter().fold(0f32, |a, v| a.max(v.abs()));
        assert!(head > 0.49, "before the key's first sample the music is untouched: {head}");
        assert!(tail < 0.25, "from the key on it is ducked: {tail}");
    }

    /// Missing, muted, or removed sidechain key sources feed silence (no ducking, no crash).
    #[test]
    fn sidechain_missing_muted_and_removed_key_feeds_silence() {
        let rate = 48_000u32;
        let step = block_ns(rate);
        let mut m = Mixer::new(rate, 0);
        m.set(1, SourceSettings { tracks: 0b01, ..Default::default() });
        m.set_effects(
            1,
            &[AudioEffect::Compressor {
                ratio: 20.0,
                threshold_db: -30.0,
                attack_ms: 1.0,
                release_ms: 20.0,
                output_db: 0.0,
                sidechain: Some(2),
            }],
        );
        let t0 = 1_000_000_000u64;
        let _ = m.mix_until(t0);
        let music = vec![0.5f32; BLOCK * 2];
        let voice = vec![0.5f32; BLOCK * 2];
        let mut t = t0;
        let mut mix_n = |m: &mut Mixer, n: usize, push_voice: bool| -> f32 {
            let mut last = 0.0f32;
            for _ in 0..n {
                m.push(1, &music, 2, rate, t);
                if push_voice {
                    m.push(2, &voice, 2, rate, t);
                }
                t += step;
                for b in m.mix_until(t) {
                    last = b.tracks[0][BLOCK * 2 - 2].abs();
                }
            }
            last
        };

        // 1. Key source 2 does not exist yet -> silence key -> music untouched at 0.5.
        let missing_lvl = mix_n(&mut m, 6, false);
        assert!((missing_lvl - 0.5).abs() < 1e-4, "missing key must not duck: {missing_lvl}");

        // 2. Add key source 2 unmuted -> music ducks sharply.
        m.set(2, SourceSettings { tracks: 0b10, ..Default::default() });
        let active_lvl = mix_n(&mut m, 6, true);
        assert!(active_lvl < 0.06, "active key must duck music: {active_lvl}");

        // 3. Mute key source 2 -> silence key -> music recovers to 0.5 (OBS `sidechain_capture` muted=true).
        m.set(2, SourceSettings { muted: true, tracks: 0b10, ..Default::default() });
        let muted_lvl = mix_n(&mut m, 15, true);
        assert!((muted_lvl - 0.5).abs() < 0.01, "muted key must release ducking: {muted_lvl}");

        // 4. Unmute key source 2 -> ducks again.
        m.set(2, SourceSettings { muted: false, tracks: 0b10, ..Default::default() });
        let unmuted_lvl = mix_n(&mut m, 6, true);
        assert!(unmuted_lvl < 0.06, "unmuted key must resume ducking: {unmuted_lvl}");

        // 5. Remove key source 2 -> silence key -> recovers to 0.5, no crash.
        m.remove(2);
        let removed_lvl = mix_n(&mut m, 15, false);
        assert!((removed_lvl - 0.5).abs() < 0.01, "removed key must release ducking: {removed_lvl}");

        // 6. Re-add key source 2 -> reconnects automatically and ducks again.
        m.set(2, SourceSettings { tracks: 0b10, ..Default::default() });
        let readded_lvl = mix_n(&mut m, 6, true);
        assert!(readded_lvl < 0.06, "re-added key must resume ducking: {readded_lvl}");
    }

    /// 10,000 add/remove cycles of the sidechain key source stay crash-free, keep pre-allocated
    /// key buffers bounded, and never leak or drift.
    #[test]
    fn sidechain_ten_thousand_add_remove_cycles_stay_stable() {
        let rate = 48_000u32;
        let step = block_ns(rate);
        let mut m = Mixer::new(rate, 0);
        m.set(1, SourceSettings { tracks: 0b01, ..Default::default() });
        m.set_effects(
            1,
            &[AudioEffect::Compressor {
                ratio: 10.0,
                threshold_db: -24.0,
                attack_ms: 1.0,
                release_ms: 10.0,
                output_db: 0.0,
                sidechain: Some(2),
            }],
        );
        let t0 = 1_000_000_000u64;
        let _ = m.mix_until(t0);
        let pcm = vec![0.5f32; BLOCK * 2];
        let mut out: Vec<Block> = Vec::with_capacity(4);
        let mut t = t0;
        for cycle in 0..10_000u64 {
            m.set(2, SourceSettings { tracks: 0b10, ..Default::default() });
            if cycle % 250 == 0 {
                m.push(1, &pcm, 2, rate, t);
                m.push(2, &pcm, 2, rate, t);
                t += step;
                out.clear();
                m.mix_until_into(t, &mut out);
                for b in out.drain(..) {
                    assert!(b.tracks[0].iter().all(|v| v.is_finite()));
                    m.recycle(b);
                }
            }
            m.remove(2);
            if cycle % 250 == 0 {
                m.push(1, &pcm, 2, rate, t);
                t += step;
                out.clear();
                m.mix_until_into(t, &mut out);
                for b in out.drain(..) {
                    assert!(b.tracks[0].iter().all(|v| v.is_finite()));
                    m.recycle(b);
                }
            }
        }
        assert_eq!(m.source_count(), 1);
        assert!(m.key_bufs.len() <= KEY_BUFS_AHEAD, "key_bufs grew unexpectedly over 10k cycles: {}", m.key_bufs.len());
        assert!(m.broken_sidechains().is_empty());
    }

    /// Sidechain cycles (self-loop, 2-cycle A↔B, 3-cycle, and cycle with downstream dependent C)
    /// are detected and broken deterministically so non-cyclic edges still duck cleanly.
    #[test]
    fn sidechain_cycle_is_detected_broken_and_preserves_downstream() {
        let rate = 48_000u32;
        let step = block_ns(rate);
        let mut m = Mixer::new(rate, 0);
        let comp = |sc: u64| AudioEffect::Compressor {
            ratio: 20.0,
            threshold_db: -30.0,
            attack_ms: 1.0,
            release_ms: 10.0,
            output_db: 0.0,
            sidechain: Some(sc),
        };
        // C(3) -> A(1) <-> B(2): B's cyclic edge (2, 1) breaks; B runs first (silence key),
        // A ducks under B, and downstream C still ducks under A!
        m.set(1, SourceSettings { tracks: 0b001, ..Default::default() });
        m.set(2, SourceSettings { tracks: 0b010, ..Default::default() });
        m.set(3, SourceSettings { tracks: 0b100, ..Default::default() });
        m.set_effects(1, &[comp(2)]);
        m.set_effects(2, &[comp(1)]);
        m.set_effects(3, &[comp(1)]);
        assert_eq!(m.broken_sidechains(), &[(2, 1)]);
        assert_eq!(m.mix_order, vec![2, 1, 3]);

        let t0 = 1_000_000_000u64;
        let _ = m.mix_until(t0);
        // Push 0.5 on A, 0.25 on B (above -30 dB so B ducks A), and 0.5 on C.
        let a_in = vec![0.5f32; BLOCK * 8 * 2];
        let b_in = vec![0.25f32; BLOCK * 8 * 2];
        let c_in = vec![0.5f32; BLOCK * 8 * 2];
        m.push(1, &a_in, 2, rate, t0);
        m.push(2, &b_in, 2, rate, t0);
        m.push(3, &c_in, 2, rate, t0);
        let blocks = m.mix_until(t0 + 8 * step);
        let last = blocks.last().expect("blocks");
        let out_a = last.tracks[0][BLOCK * 2 - 2].abs();
        let out_b = last.tracks[1][BLOCK * 2 - 2].abs();
        let out_c = last.tracks[2][BLOCK * 2 - 2].abs();
        // B's cyclic edge broke to silence -> B is untouched at 0.25.
        assert!((out_b - 0.25).abs() < 1e-4, "cycle-broken source B must stay untouched: {out_b}");
        // A ducks under B (0.25 ≈ -12 dBFS, 18 dB over -30 dB @ 20:1 -> -17.1 dB -> ~0.070).
        assert!(out_a < 0.09, "A must still duck under B: {out_a}");
        // C ducks under A's post-filter output (~0.070 ≈ -23 dBFS, 7 dB over -30 dB @ 20:1 -> -6.65 dB -> ~0.23).
        assert!(out_c < 0.30, "downstream C must still duck under A: {out_c}");

        // Self-loop on source 4 is also detected and broken to silence.
        m.set(4, SourceSettings::default());
        m.set_effects(4, &[comp(4)]);
        assert_eq!(m.broken_sidechains(), &[(2, 1), (4, 4)]);

        // Removing the cyclic filter on B clears (2, 1).
        m.set_effects(2, &[]);
        assert_eq!(m.broken_sidechains(), &[(4, 4)]);
    }

    /// Sidechain routing preserves exact A/V block timestamps and latency accounting even when
    /// the key source arrives at a different sample rate (44.1 kHz -> 48 kHz) and carries a
    /// latency-inducing filter.
    #[test]
    fn sidechain_preserves_av_sync_block_timestamps() {
        let rate = 48_000u32;
        let step = block_ns(rate);
        let mut m = Mixer::new(rate, 40_000_000);
        m.set(1, SourceSettings { tracks: 0b01, ..Default::default() });
        m.set(2, SourceSettings { tracks: 0b10, ..Default::default() });
        m.set_effects(
            1,
            &[AudioEffect::Compressor {
                ratio: 10.0,
                threshold_db: -24.0,
                attack_ms: 5.0,
                release_ms: 50.0,
                output_db: 0.0,
                sidechain: Some(2),
            }],
        );
        let t0 = 1_000_000_000u64;
        let _ = m.mix_until(t0);
        let music_chunk = vec![0.4f32; 960 * 2]; // 20 ms at 48 kHz
        let voice_chunk = vec![0.4f32; 882 * 2]; // 20 ms at 44.1 kHz
        let mut produced = Vec::new();
        for k in 0..50u64 {
            let at = t0 + k * 20_000_000;
            m.push(1, &music_chunk, 2, 48_000, at);
            m.push(2, &voice_chunk, 2, 44_100, at);
            produced.extend(m.mix_until(at + 20_000_000));
        }
        assert!(produced.len() >= 40, "expected >= 40 blocks, got {}", produced.len());
        // Exact monotonic block cadence with zero drift or sidechain-induced timestamp shift.
        for pair in produced.windows(2) {
            assert_eq!(pair[1].ts_ns - pair[0].ts_ns, step, "block cadence drifted under sidechain");
        }
        // Latency stayed at baseline 40 ms (sidechain adds zero extra buffering).
        assert_eq!(m.latency_ns(), 40_000_000);
        // Music actually ducked under the 44.1 kHz resampled voice!
        let tail_music = produced.last().unwrap().tracks[0][BLOCK * 2 - 2].abs();
        assert!(tail_music < 0.15, "music should duck under 44.1 kHz key: {tail_music}");
    }
}
