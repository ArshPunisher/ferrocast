//! The stream's health while live: one score out of 100 and, when it's not full, the main
//! reason, from the last ten seconds of the engine's and the network's counters.

use std::collections::VecDeque;

use serde::Serialize;

/// Looks kept (two seconds apart: the last ten seconds).
const KEEP: usize = 6;
/// Frames drawn with none sent for this long (ms) is a stalled stream.
const STALL_AFTER_MS: u64 = 5_000;

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
    /// All the time the engine's encoders took (its `encoding_ms`), and when this look was taken
    /// (ms): between two looks, encoding over 60% of the time means late frames are the
    /// encoder's doing, not the drawing's.
    pub encoding_ms: u64,
    pub at_ms: u64,
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
    /// What to do about it, behind an (i) (None with the reason).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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
    /// Of the frames drawn late in the window, the share drawn while the encoders held the
    /// frames back (each look's flag stands for the late frames since the look before).
    fn encoder_late_share(&self) -> f64 {
        let (mut by_encoder, mut all) = (0u64, 0u64);
        for (a, b) in self.looks.iter().zip(self.looks.iter().skip(1)) {
            let late = b.lagged.saturating_sub(a.lagged);
            all += late;
            let busy = share(b.encoding_ms.saturating_sub(a.encoding_ms), b.at_ms.saturating_sub(a.at_ms));
            if busy > 0.6 {
                by_encoder += late;
            }
        }
        share(by_encoder, all)
    }

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
        // (points lost, what it is in a few words, what to do about it)
        let mut costs: Vec<(f64, String, &str)> = vec![
            (
                (share(d(s.lagged, first.lagged), d(s.frames, first.frames)) * 400.0).min(40.0),
                if self.encoder_late_share() >= 0.5 {
                    "the encoder can't keep up".into()
                } else {
                    "the computer is busy".into()
                },
                if self.encoder_late_share() >= 0.5 {
                    "Lower the resolution or the frame rate."
                } else {
                    "Use fewer sources or a lower frame rate."
                },
            ),
            (
                (share(d(s.skipped, first.skipped), d(s.encoded, first.encoded)) * 400.0).min(40.0),
                "the encoder is behind".into(),
                "Use the hardware encoder, or a lower quality.",
            ),
            (
                (share(d(s.dropped, first.dropped), d(s.sent, first.sent)) * 400.0).min(40.0),
                "the upload is struggling".into(),
                "Frames are being dropped on the way out. Lower the bitrate, or use a wired connection.",
            ),
            (
                (share(d(s.net_dropped, first.net_dropped), d(s.sent, first.sent)) * 400.0).min(40.0),
                "a platform is losing packets".into(),
                "The upload is struggling. Lower the bitrate, or use a wired connection.",
            ),
        ];
        if s.reconnecting {
            costs.push((50.0, "the stream is reconnecting".into(), "Check your internet connection."));
        }
        // Frames are still being drawn but the stream's own sender has delivered none for several
        // seconds: the stream (and a recording sharing its encoder) has silently stopped (a stall seen
        // after 26 minutes of a soak, with Health still 100).
        let span = s.at_ms.saturating_sub(first.at_ms);
        if span >= STALL_AFTER_MS && first.sent > 0 && s.sent == first.sent && d(s.frames, first.frames) >= 60 {
            costs.push((
                60.0,
                "the stream has stopped sending".into(),
                "Frames are drawn but none are being sent. Stop and start the stream again.",
            ));
        }
        if s.paused {
            costs.push((
                20.0,
                "a platform is paused".into(),
                "Upload protection paused it to keep the others smooth. It comes back by itself.",
            ));
        }
        if s.quality_pct < 100 {
            costs.push((
                f64::from(100 - s.quality_pct) / 3.0,
                format!("quality is at {}%", s.quality_pct),
                "Auto quality lowered it to keep the stream smooth. It goes back up by itself.",
            ));
        }
        if s.cpu > 85.0 {
            costs.push(((s.cpu - 85.0).min(15.0), "the computer is nearly at its limit".into(), "Close other apps."));
        }
        let lost: f64 = costs.iter().map(|(c, ..)| c).sum();
        let score = (100.0 - lost).round().clamp(0.0, 100.0) as u8;
        let (reason, detail) = costs
            .into_iter()
            .filter(|(c, ..)| *c >= 1.0)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .filter(|_| score < 100)
            .map_or((None, None), |(_, why, fix)| (Some(why), Some(fix.to_string())));
        let verdict = match score {
            90.. => "great",
            75.. => "good",
            50.. => "shaky",
            _ => "poor",
        };
        Health { score, verdict, reason, detail }
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
        assert!(h.reason.clone().unwrap().contains("encoder"));
        assert!(h.detail.unwrap().contains("hardware encoder"), "what to do is the detail");
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
    fn a_stream_that_stops_sending_while_frames_are_drawn_is_not_great() {
        let mut m = HealthMeter::default();
        let at = |n: u64, sent: u64| Sample { sent, at_ms: n * 1_000, ..at(n) };
        let mut last = None;
        // Sending for 3 s, then drawing on with nothing sent.
        for n in 0..4 {
            last = Some(m.push(at(n, n * 60)));
        }
        assert_eq!(last.clone().unwrap().score, 100);
        for n in 4..12 {
            last = Some(m.push(at(n, 180)));
        }
        let h = last.clone().unwrap();
        assert!(h.score <= 40, "score {}", h.score);
        assert!(h.reason.unwrap().contains("stopped sending"));
        // It resumes: the window clears.
        for n in 12..20 {
            last = Some(m.push(at(n, 180 + (n - 11) * 60)));
        }
        assert_eq!(last.unwrap().score, 100);
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

    #[test]
    fn late_frames_name_the_encoder_when_it_holds_them_back() {
        // Two seconds apart; `busy` ms of them spent encoding.
        let look = |n: u64, lagged: u64, busy: u64| Sample { lagged, encoding_ms: busy, at_ms: n * 2000, ..at(n) };
        let late = |busy: u64| {
            let mut m = HealthMeter::default();
            m.push(look(0, 0, 0));
            m.push(look(1, 30, busy)).reason.unwrap_or_default()
        };
        assert!(late(1800).contains("the encoder can't keep up"), "{}", late(1800));
        assert!(late(400).contains("the computer is busy"), "{}", late(400));
        // Late frames from a busy computer aren't the encoder's because it's busy afterwards.
        let mut m = HealthMeter::default();
        m.push(look(0, 0, 0));
        m.push(look(1, 30, 200));
        assert!(m.push(look(2, 30, 2000)).reason.unwrap_or_default().contains("the computer is busy"));
    }
}
