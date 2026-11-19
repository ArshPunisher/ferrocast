//! Sound bars (OBS's audio visualizer plugins, built in): bars that move with what's heard,
//! the sound on air or one source's. Each frame the engine hands in what came since, the
//! strength of each band is measured (Goertzel, log-spaced from 50 Hz to 12 kHz over the last
//! 2048 samples), bars rise at once and fall gently, and the picture is drawn on the CPU (small:
//! a few hundred thousand pixels).

use std::collections::VecDeque;

use crate::Picture;

/// What a visualizer shows and how.
#[derive(Debug, Clone, PartialEq)]
pub struct VisualizerSpec {
    /// The source it listens to by name, or None for the sound on air.
    pub of: Option<String>,
    /// How many bars (8 to 64).
    pub bars: u32,
    pub width: u32,
    pub height: u32,
    /// Straight RGBA.
    pub rgba: [u8; 4],
    /// Bars grow both ways from the middle (else up from the bottom).
    pub mirror: bool,
}

/// How many samples each measure looks at.
const WINDOW: usize = 2048;
/// How fast a bar falls back each frame (rising is at once).
const FALL: f32 = 0.86;

/// A visualizer as it runs: what it heard last and where its bars stand.
#[derive(Debug)]
pub(crate) struct VisualizerLive {
    pub spec: VisualizerSpec,
    ring: VecDeque<f32>,
    levels: Vec<f32>,
    /// The source it taps now (its id) and the tap.
    pub tap: Option<(u64, crate::mix::Tap)>,
}

impl VisualizerLive {
    pub fn new(spec: VisualizerSpec) -> Self {
        let n = spec.bars.clamp(8, 64) as usize;
        Self { spec, ring: VecDeque::with_capacity(WINDOW), levels: vec![0.0; n], tap: None }
    }

    /// Takes in `fresh` mono samples at `rate` and draws the bars as they stand now.
    pub fn next(&mut self, fresh: &[f32], rate: u32) -> Picture {
        for s in fresh.iter().rev().take(WINDOW).rev() {
            if self.ring.len() == WINDOW {
                self.ring.pop_front();
            }
            self.ring.push_back(*s);
        }
        let now = bands(self.ring.make_contiguous(), rate, self.levels.len());
        for (l, n) in self.levels.iter_mut().zip(now) {
            *l = n.max(*l * FALL);
        }
        draw(&self.levels, &self.spec)
    }
}

/// How strong each of `n` log-spaced bands is (0 to 1, from -60 dB to 0 dB).
pub fn bands(samples: &[f32], rate: u32, n: usize) -> Vec<f32> {
    if samples.len() < 64 || rate == 0 || n == 0 {
        return vec![0.0; n];
    }
    let (lo, hi) = (50f32, 12_000f32.min(rate as f32 * 0.45));
    let len = samples.len() as f32;
    (0..n)
        .map(|i| {
            let f = lo * (hi / lo).powf((i as f32 + 0.5) / n as f32);
            // Goertzel for one frequency, with a Hann window.
            let w = std::f32::consts::TAU * f / rate as f32;
            let coeff = 2.0 * w.cos();
            let (mut s1, mut s2) = (0f32, 0f32);
            for (k, x) in samples.iter().enumerate() {
                let hann = 0.5 - 0.5 * (std::f32::consts::TAU * k as f32 / (len - 1.0)).cos();
                let s0 = x * hann + coeff * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0);
            // A full-scale sine reads about 0 dB (the Hann window halves its amplitude).
            let amp = 4.0 * power.sqrt() / len;
            let db = 20.0 * amp.max(1e-9).log10();
            ((db + 60.0) / 60.0).clamp(0.0, 1.0)
        })
        .collect()
}

/// The bars as a picture (clear behind them).
pub fn draw(levels: &[f32], spec: &VisualizerSpec) -> Picture {
    let (w, h) = (spec.width.clamp(16, 3840), spec.height.clamp(8, 2160));
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let n = levels.len().max(1) as u32;
    let slot = w as f32 / n as f32;
    let gap = (slot * 0.2).max(1.0);
    for (i, level) in levels.iter().enumerate() {
        let x0 = (i as f32 * slot + gap / 2.0) as u32;
        let x1 = (((i + 1) as f32 * slot - gap / 2.0) as u32).max(x0 + 1).min(w);
        // Always a sliver, so silence still shows where the bars are.
        let tall = ((level.clamp(0.0, 1.0) * h as f32) as u32).max(2).min(h);
        let (y0, y1) = if spec.mirror { ((h - tall) / 2, (h + tall) / 2) } else { (h - tall, h) };
        for y in y0..y1 {
            for x in x0..x1 {
                let p = ((y * w + x) * 4) as usize;
                rgba[p..p + 4].copy_from_slice(&spec.rgba);
            }
        }
    }
    Picture { width: w, height: h, rgba, premultiplied: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, rate: u32, n: usize, amp: f32) -> Vec<f32> {
        (0..n).map(|i| (i as f32 * std::f32::consts::TAU * hz / rate as f32).sin() * amp).collect()
    }

    fn spec(bars: u32, mirror: bool) -> VisualizerSpec {
        VisualizerSpec { of: None, bars, width: 320, height: 100, rgba: [255, 255, 255, 255], mirror }
    }

    #[test]
    fn a_tone_lights_its_own_band() {
        let low = bands(&sine(100.0, 48_000, 2048, 0.5), 48_000, 16);
        let high = bands(&sine(5000.0, 48_000, 2048, 0.5), 48_000, 16);
        let peak = |b: &[f32]| b.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).unwrap_or(0);
        assert!(peak(&low) < 5, "100 Hz is a low band: {low:?}");
        assert!(peak(&high) > 10, "5 kHz is a high band: {high:?}");
        assert!(low[peak(&low)] > 0.75, "a loud tone fills its band: {low:?}");
        // Silence and too little sound are flat.
        assert!(bands(&vec![0.0; 2048], 48_000, 16).iter().all(|b| *b == 0.0));
        assert_eq!(bands(&[0.5; 10], 48_000, 16), vec![0.0; 16]);
        assert_eq!(bands(&sine(100.0, 48_000, 2048, 0.5), 0, 16), vec![0.0; 16]);
    }

    #[test]
    fn bars_rise_at_once_and_fall_gently() {
        let mut v = VisualizerLive::new(spec(16, false));
        v.next(&sine(200.0, 48_000, 2048, 0.8), 48_000);
        let up = v.levels.iter().copied().fold(0.0, f32::max);
        assert!(up > 0.7, "up at once: {up}");
        v.next(&vec![0.0; 2048], 48_000);
        let after = v.levels.iter().copied().fold(0.0, f32::max);
        assert!(after < up && after > up * 0.8, "falls gently: {up} then {after}");
    }

    #[test]
    fn the_picture_has_its_bars_where_they_belong() {
        let levels = vec![1.0, 0.0, 0.5, 0.0];
        let p = draw(&levels, &spec(4, false));
        assert_eq!((p.width, p.height), (320, 100));
        let at = |x: u32, y: u32| p.rgba[((y * p.width + x) * 4 + 3) as usize];
        assert_eq!(at(40, 1), 255, "a full bar reaches the top");
        assert_eq!(at(120, 50), 0, "a silent bar is only a sliver");
        assert_eq!(at(120, 99), 255, "…at the bottom");
        assert_eq!(at(200, 40), 0, "half a bar stops halfway");
        assert_eq!(at(200, 60), 255);
        // Mirrored: from the middle both ways.
        let m = draw(&[0.5], &spec(1, true));
        let at = |x: u32, y: u32| m.rgba[((y * m.width + x) * 4 + 3) as usize];
        assert_eq!((at(160, 50), at(160, 5), at(160, 95)), (255, 0, 0));
        // Odd sizes are held to something drawable.
        let tiny = draw(&[1.0; 64], &VisualizerSpec { width: 1, height: 1, ..spec(64, false) });
        assert_eq!((tiny.width, tiny.height), (16, 8));
    }
}
