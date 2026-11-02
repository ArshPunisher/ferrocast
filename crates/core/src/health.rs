//! The stream's health while live: one score out of 100 and, when it's not full, the main
//! reason, from the last ten seconds of the engine's and the network's counters.

use std::collections::VecDeque;

use serde::Serialize;

/// Looks kept (two seconds apart: the last ten seconds).
const KEEP: usize = 6;

/// The counters at one look (running totals since the stream started, except the "now" ones).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    /// Frames drawn, and drawn late.
    pub frames: u64,
    pub lagged: u64,
    /// Frames given to the encoders, and skipped by them.
    pub encoded: u64,
    pub skipped: u64,
    /// Frames sent, and dropped on the way out (the Studio's own sender).
    pub sent: u64,
    pub dropped: u64,
    /// Packets the platforms' senders dropped (all told).
    pub net_dropped: u64,
    /// Now: the stream is reconnecting, a platform is paused, auto quality's level, CPU %.
    pub reconnecting: bool,
    pub paused: bool,
    pub quality_pct: u8,
    pub cpu: f64,
}

/// How the stream is doing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Health {
    pub score: u8,
    /// `great` (90+), `good` (75+), `shaky` (50+) or `poor`.
    pub verdict: &'static str,
    /// What costs the most, in words (None at 100).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The score's memory: the last few looks.
#[derive(Debug, Default)]
pub struct HealthMeter {
    looks: VecDeque<Sample>,
}

/// Share of `part` in `all` between two looks, 0 to 1.
fn share(part: u64, all: u64) -> f64 {
    if all == 0 { 0.0 } else { (part as f64 / all as f64).min(1.0) }
}

impl HealthMeter {
    /// A new look → the health over the last ten seconds.
    pub fn push(&mut self, s: Sample) -> Health {
        // Counters that went back mean a new stream: start over.
        if self.looks.back().is_some_and(|b| s.frames < b.frames || s.sent < b.sent) {
            self.looks.clear();
        }
        self.looks.push_back(s);
        while self.looks.len() > KEEP {
            self.looks.pop_front();
        }
        let first = self.looks.front().copied().unwrap_or(s);
        let d = |a: u64, b: u64| a.saturating_sub(b);
        // Each trouble costs points (the worst named): 1% of frames late, skipped or dropped
        // costs 4, up to 40 each.
        let mut costs: Vec<(f64, String)> = vec![
            (
                (share(d(s.lagged, first.lagged), d(s.frames, first.frames)) * 400.0).min(40.0),
                "frames are drawn late: the computer is busy (fewer sources or a lower frame rate helps)".into(),
            ),
            (
                (share(d(s.skipped, first.skipped), d(s.encoded, first.encoded)) * 400.0).min(40.0),
                "the encoder is falling behind (the hardware encoder or a lower quality helps)".into(),
            ),
            (
                (share(d(s.dropped, first.dropped), d(s.sent, first.sent)) * 400.0).min(40.0),
                "frames are dropped on the way out: the upload is struggling".into(),
            ),
            (
                (share(d(s.net_dropped, first.net_dropped), d(s.sent, first.sent)) * 400.0).min(40.0),
                "a platform is losing packets: the upload is struggling".into(),
            ),
        ];
        if s.reconnecting {
            costs.push((50.0, "the stream is reconnecting".into()));
        }
        if s.paused {
            costs.push((20.0, "a platform is paused to keep the others smooth".into()));
        }
        if s.quality_pct < 100 {
            costs.push((
                f64::from(100 - s.quality_pct) / 3.0,
                format!("quality is lowered to {}% to stay smooth", s.quality_pct),
            ));
        }
        if s.cpu > 85.0 {
            costs.push(((s.cpu - 85.0).min(15.0), "the computer is nearly at its limit".into()));
        }
        let lost: f64 = costs.iter().map(|(c, _)| c).sum();
        let score = (100.0 - lost).round().clamp(0.0, 100.0) as u8;
        let reason = costs
            .into_iter()
            .filter(|(c, _)| *c >= 1.0)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, why)| why)
            .filter(|_| score < 100);
        let verdict = match score {
            90.. => "great",
            75.. => "good",
            50.. => "shaky",
            _ => "poor",
        };
        Health { score, verdict, reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(n: u64) -> Sample {
        Sample { frames: n * 60, encoded: n * 60, sent: n * 60, quality_pct: 100, cpu: 30.0, ..Sample::default() }
    }

    #[test]
    fn a_clean_stream_is_great() {
        let mut m = HealthMeter::default();
        for n in 0..10 {
            let h = m.push(at(n));
            assert_eq!((h.score, h.verdict, h.reason.clone()), (100, "great", None));
        }
    }

    #[test]
    fn trouble_costs_points_and_names_the_worst() {
        let mut m = HealthMeter::default();
        m.push(at(0));
        // 1 in 60 late, 3 in 60 skipped by the encoder: about 7 + 20.
        let h = m.push(Sample { lagged: 1, skipped: 3, ..at(1) });
        assert_eq!((h.score, h.verdict), (73, "shaky"));
        assert!(h.reason.unwrap().contains("encoder"));
        // Reconnecting outweighs it.
        let h = m.push(Sample { lagged: 1, skipped: 3, reconnecting: true, ..at(2) });
        assert!(h.reason.unwrap().contains("reconnecting"));
        // Ten seconds on, the old trouble is out of the window.
        for n in 3..10 {
            m.push(Sample { lagged: 1, skipped: 3, ..at(n) });
        }
        assert_eq!(m.push(Sample { lagged: 1, skipped: 3, ..at(10) }).score, 100);
    }

    #[test]
    fn lowered_quality_and_a_busy_computer_count() {
        let mut m = HealthMeter::default();
        let h = m.push(Sample { quality_pct: 55, cpu: 95.0, ..at(0) });
        // 15 for quality, 10 for CPU.
        assert_eq!((h.score, h.verdict), (75, "good"));
        assert!(h.reason.unwrap().contains("55%"));
    }

    #[test]
    fn a_new_stream_starts_over() {
        let mut m = HealthMeter::default();
        m.push(at(5));
        m.push(Sample { lagged: 30, ..at(6) });
        // Counters back at the start: the earlier looks are dropped.
        assert_eq!(m.push(at(1)).score, 100);
    }
}
