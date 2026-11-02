//! The audio mixer (ENGINE.md E3): sources push sound with timestamps (nanoseconds, the same
//! clock as video), at any rate; the mixer turns each to the output rate, places it by its time
//! (jitter under 70 ms smoothed over, as libobs does), applies volume, balance (sine law, as
//! libobs), mono and sync offset, and mixes blocks of 1024 frames into up to 6 tracks. Each
//! source's levels (peak, RMS) are measured after its volume, as OBS's meters show them.
//!
//! The caller drives the clock: `mix_until(now)` makes every block that ends `latency` before
//! `now`, so sound that arrives a little late still makes its block.

use std::collections::{BTreeMap, VecDeque};

use crate::afx::{AudioEffect, AudioFilter};
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};

/// Frames per block (libobs's `AUDIO_OUTPUT_FRAMES`).
pub const BLOCK: usize = 1024;
/// Tracks (libobs's `MAX_AUDIO_MIXES`).
pub const TRACKS: usize = 6;
/// Timestamps closer than this to where a source's sound would continue are taken as continuing
/// (libobs's `TS_SMOOTHING_THRESHOLD`).
const SMOOTHING_NS: u64 = 70_000_000;
/// The longest gap ahead filled with silence; beyond it, the queue starts again.
const MAX_GAP_NS: u64 = 2_000_000_000;
/// A source never holds more than this much sound waiting (a stalled mixer can't grow memory).
const MAX_QUEUE_SECS: usize = 5;
/// Input frames the resampler takes at a time.
const RESAMPLE_CHUNK: usize = 480;

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
        }
    }
}

/// A source's levels over the last block, per channel (left, right), in dBFS (-inf is silence).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Levels {
    pub peak: [f32; 2],
    pub rms: [f32; 2],
}

/// One mixed block: its start time and each track's interleaved stereo frames.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub ts_ns: u64,
    pub tracks: Vec<Vec<f32>>,
    /// What's heard here: the monitored sources mixed (interleaved stereo).
    pub monitor: Vec<f32>,
}

struct Source {
    settings: SourceSettings,
    /// Sound waiting, interleaved stereo at the mixer's rate, starting at `start_ns`.
    queue: VecDeque<f32>,
    start_ns: Option<u64>,
    /// Turning its rate into the mixer's, when they differ: the resampler and what it hasn't taken.
    resampler: Option<(u32, SincFixedIn<f32>, [Vec<f32>; 2])>,
    /// Its filters, in order (after mono and balance, before volume, as OBS runs them).
    filters: Vec<AudioFilter>,
    levels: Levels,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source").field("settings", &self.settings).field("queued", &(self.queue.len() / 2)).finish()
    }
}

/// The mixer.
#[derive(Debug)]
pub struct Mixer {
    rate: u32,
    latency_ns: u64,
    sources: BTreeMap<u64, Source>,
    /// The next block's start; set by the first `mix_until`.
    next_ns: Option<u64>,
    /// Listeners to one source's sound (captions), by source; gone when their tap is dropped.
    taps: Vec<(u64, std::sync::Weak<std::sync::Mutex<std::collections::VecDeque<f32>>>)>,
    /// Music quieter while the voices talk, and each track steered to a loudness.
    ducker: Option<crate::level::Ducker>,
    loudness: Option<Vec<crate::level::Loudness>>,
    loudness_target: Option<f32>,
}

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

/// Gives source `id`'s sound to its taps (at the mixer's rate only: every capture here gives
/// it; other rates aren't tapped).
#[allow(clippy::type_complexity)]
fn feed_taps(
    taps: &[(u64, std::sync::Weak<std::sync::Mutex<std::collections::VecDeque<f32>>>)],
    id: u64,
    (l, r): (&[f32], &[f32]),
    rate: u32,
    mixer_rate: u32,
) {
    if rate != mixer_rate {
        return;
    }
    let max = TAP_SECONDS * mixer_rate as usize;
    for (_, t) in taps.iter().filter(|(i, _)| *i == id) {
        if let Some(buf) = t.upgrade()
            && let Ok(mut b) = buf.lock()
        {
            b.extend(l.iter().zip(r).map(|(a, b)| (a + b) * 0.5));
            let over = b.len().saturating_sub(max);
            b.drain(..over);
        }
    }
}

impl Mixer {
    /// `rate` Hz stereo out; blocks are made `latency_ns` behind the clock.
    pub fn new(rate: u32, latency_ns: u64) -> Self {
        Self {
            rate,
            latency_ns,
            sources: BTreeMap::new(),
            next_ns: None,
            taps: Vec::new(),
            ducker: None,
            loudness: None,
            loudness_target: None,
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Starts listening to source `id`'s sound (see [`Tap`]).
    pub fn tap(&mut self, id: u64) -> Tap {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        self.taps.retain(|(_, t)| t.strong_count() > 0);
        self.taps.push((id, std::sync::Arc::downgrade(&buf)));
        Tap(buf)
    }

    /// Adds a source (or changes its settings).
    pub fn set(&mut self, id: u64, settings: SourceSettings) {
        match self.sources.get_mut(&id) {
            Some(s) => s.settings = settings,
            None => {
                self.sources.insert(
                    id,
                    Source {
                        settings,
                        queue: VecDeque::new(),
                        start_ns: None,
                        resampler: None,
                        filters: Vec::new(),
                        levels: Levels::default(),
                    },
                );
            }
        }
    }

    /// A source's filters, in order (replacing any before; their memory starts fresh).
    pub fn set_effects(&mut self, id: u64, effects: &[AudioEffect]) {
        let rate = self.rate;
        if let Some(s) = self.sources.get_mut(&id) {
            s.filters = effects.iter().map(|e| AudioFilter::new(*e, rate)).collect();
        }
    }

    pub fn remove(&mut self, id: u64) {
        self.sources.remove(&id);
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

    pub fn levels(&self, id: u64) -> Option<Levels> {
        self.sources.get(&id).map(|s| s.levels)
    }

    /// A source's sound: `channels` interleaved (1 or 2; more are folded to their first two) at
    /// `rate`, its first frame at `ts_ns`. Unknown sources are ignored.
    pub fn push(&mut self, id: u64, pcm: &[f32], channels: usize, rate: u32, ts_ns: u64) {
        let out_rate = self.rate;
        let Some(src) = self.sources.get_mut(&id) else { return };
        if channels == 0 || rate == 0 || pcm.is_empty() {
            return;
        }
        let ts = (ts_ns as i64 + src.settings.sync_offset_ns).max(0) as u64;
        // Where this sound would continue what's queued: jitter is smoothed over; a gap ahead
        // (a source that went quiet, beeps scheduled apart) is filled with silence, as libobs
        // does, so nothing queued is lost; a jump back or a long jump (a seek) restarts the queue.
        let queued = src.queue.len() / 2
            + src.resampler.as_ref().map_or(0, |(_, _, p)| p[0].len() * out_rate as usize / rate as usize);
        let mut gap = 0usize;
        let at = match src.start_ns {
            Some(start) => {
                let expected = start + queued as u64 * 1_000_000_000 / u64::from(out_rate);
                if ts.abs_diff(expected) < SMOOTHING_NS {
                    None
                } else if ts > expected && ts - expected <= MAX_GAP_NS {
                    gap = ((ts - expected) * u64::from(rate) / 1_000_000_000) as usize;
                    None
                } else {
                    Some(ts)
                }
            }
            None => Some(ts),
        };
        // Stereo, planar, after the silence that fills a gap.
        let frames = pcm.len() / channels;
        let (mut l, mut r) = (vec![0.0; gap], vec![0.0; gap]);
        l.reserve(frames);
        r.reserve(frames);
        for f in pcm.chunks_exact(channels) {
            l.push(f[0]);
            r.push(if channels > 1 { f[1] } else { f[0] });
        }
        if !self.taps.is_empty() && !src.settings.muted {
            feed_taps(&self.taps, id, (&l[gap..], &r[gap..]), rate, out_rate);
        }
        if let Some(t) = at {
            src.queue.clear();
            src.start_ns = Some(t);
            if let Some((_, rs, pending)) = &mut src.resampler {
                rs.reset();
                pending[0].clear();
                pending[1].clear();
            }
        }
        if rate == out_rate {
            src.resampler = None;
            for (a, b) in l.iter().zip(&r) {
                src.queue.push_back(*a);
                src.queue.push_back(*b);
            }
        } else {
            if src.resampler.as_ref().is_none_or(|(r0, ..)| *r0 != rate) {
                let params = SincInterpolationParameters {
                    sinc_len: 128,
                    f_cutoff: 0.95,
                    oversampling_factor: 128,
                    interpolation: SincInterpolationType::Cubic,
                    window: WindowFunction::BlackmanHarris2,
                };
                match SincFixedIn::<f32>::new(f64::from(out_rate) / f64::from(rate), 1.1, params, RESAMPLE_CHUNK, 2) {
                    Ok(rs) => src.resampler = Some((rate, rs, [Vec::new(), Vec::new()])),
                    Err(_) => return,
                }
            }
            let Some((_, rs, pending)) = &mut src.resampler else { return };
            pending[0].extend_from_slice(&l);
            pending[1].extend_from_slice(&r);
            while pending[0].len() >= RESAMPLE_CHUNK {
                let chunk = [pending[0][..RESAMPLE_CHUNK].to_vec(), pending[1][..RESAMPLE_CHUNK].to_vec()];
                pending[0].drain(..RESAMPLE_CHUNK);
                pending[1].drain(..RESAMPLE_CHUNK);
                if let Ok(out) = rs.process(&chunk, None) {
                    for (a, b) in out[0].iter().zip(&out[1]) {
                        src.queue.push_back(*a);
                        src.queue.push_back(*b);
                    }
                }
            }
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
        let until = now_ns.saturating_sub(self.latency_ns);
        let step = block_ns(self.rate);
        let mut next = *self.next_ns.get_or_insert(until.saturating_sub(step));
        let mut out = Vec::new();
        while next + step <= until {
            out.push(self.mix_block(next));
            // Exact frame counts, so blocks never drift from the sample clock.
            next += step;
        }
        self.next_ns = Some(next);
        out
    }

    fn mix_block(&mut self, start: u64) -> Block {
        let rate = self.rate;
        let mut tracks = vec![vec![0f32; BLOCK * 2]; TRACKS];
        let mut monitor = vec![0f32; BLOCK * 2];
        // Talking, from the voices' last block (a block late: about 20 ms, nothing to hear).
        let duck = self.ducker.as_mut().map(|d| {
            let talking = d.cfg.voices.iter().any(|v| {
                self.sources.get(v).is_some_and(|s| s.levels.peak[0].max(s.levels.peak[1]) > d.cfg.threshold_db)
            });
            let (from, to) = d.step(talking, BLOCK as f32 / rate as f32);
            (d.cfg.music.clone(), from, to)
        });
        for (id, src) in self.sources.iter_mut() {
            let mut buf = vec![0f32; BLOCK * 2];
            if let Some(s0) = src.start_ns {
                // Drop what's before this block; place what's in it.
                if s0 < start {
                    let skip = frames_in(start - s0, rate).min(src.queue.len() / 2);
                    src.queue.drain(..skip * 2);
                    src.start_ns = Some(s0 + skip as u64 * 1_000_000_000 / u64::from(rate));
                }
                let s0 = src.start_ns.unwrap_or(start);
                let offset = if s0 > start { frames_in(s0 - start, rate) } else { 0 };
                if offset < BLOCK {
                    let n = (BLOCK - offset).min(src.queue.len() / 2);
                    for (i, v) in src.queue.drain(..n * 2).enumerate() {
                        buf[offset * 2 + i] = v;
                    }
                    if n > 0 {
                        src.start_ns = Some(start + ((offset + n) as u64) * 1_000_000_000 / u64::from(rate));
                    }
                }
            }
            let s = src.settings;
            let (gl, gr) = if (s.balance - 0.5).abs() > 0.01 {
                let b = s.balance.clamp(0.0, 1.0);
                (((1.0 - b) * std::f32::consts::FRAC_PI_2).sin(), (b * std::f32::consts::FRAC_PI_2).sin())
            } else {
                (1.0, 1.0)
            };
            let vol = if s.muted { 0.0 } else { s.volume.max(0.0) };
            for f in buf.as_chunks_mut::<2>().0 {
                if s.mono {
                    let m = (f[0] + f[1]) * 0.5;
                    f[0] = m;
                    f[1] = m;
                }
                f[0] *= gl;
                f[1] *= gr;
            }
            if let Some((music, from, to)) = &duck
                && music.contains(id)
                && (from - 1.0).abs().max((to - 1.0).abs()) > 1e-4
            {
                // Smoothly across the block, no click.
                for (i, f) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let g = from + (to - from) * i as f32 / BLOCK as f32;
                    f[0] *= g;
                    f[1] *= g;
                }
            }
            if !src.filters.is_empty() {
                let (mut l, mut r): (Vec<f32>, Vec<f32>) = buf.as_chunks::<2>().0.iter().map(|f| (f[0], f[1])).unzip();
                for f in &mut src.filters {
                    f.process([&mut l, &mut r]);
                }
                for (i, f) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    f[0] = l[i];
                    f[1] = r[i];
                }
            }
            let (mut peak, mut sq) = ([0f32; 2], [0f32; 2]);
            for f in buf.as_chunks_mut::<2>().0 {
                f[0] *= vol;
                f[1] *= vol;
                for c in 0..2 {
                    peak[c] = peak[c].max(f[c].abs());
                    sq[c] += f[c] * f[c];
                }
            }
            src.levels = Levels {
                peak: [db(peak[0]), db(peak[1])],
                rms: [db((sq[0] / BLOCK as f32).sqrt()), db((sq[1] / BLOCK as f32).sqrt())],
            };
            if s.monitoring != Monitoring::Off {
                for (o, v) in monitor.iter_mut().zip(&buf) {
                    *o += v;
                }
            }
            for (t, track) in tracks.iter_mut().enumerate() {
                if s.monitoring != Monitoring::Only && s.tracks & (1 << t) != 0 {
                    for (o, v) in track.iter_mut().zip(&buf) {
                        *o += v;
                    }
                }
            }
        }
        if let Some(loud) = self.loudness.as_mut() {
            for (track, l) in tracks.iter_mut().zip(loud.iter_mut()) {
                l.process(track);
            }
        }
        for track in &mut tracks {
            for v in track.iter_mut() {
                *v = v.clamp(-1.0, 1.0);
            }
        }
        for v in &mut monitor {
            *v = v.clamp(-1.0, 1.0);
        }
        Block { ts_ns: start, tracks, monitor }
    }
}

#[cfg(test)]
mod tests {
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
}
