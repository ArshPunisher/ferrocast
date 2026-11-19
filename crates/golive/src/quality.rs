//! Quality per destination: one destination that falls behind alone (the others keep up, so
//! it's its own route, not the streamer's upload) takes the lighter stream for a while, then
//! tries the full one again; coming back down soon after doubles the wait. When they all fall
//! behind it's the upload, and upload protection (`protect`) decides. OBS can only lower every
//! destination at once. A pure state machine, fed one sample per destination every second.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::protect::Sample;

/// Seconds a destination falls behind alone before it takes the lighter stream.
pub const LIGHTEN_AFTER: u32 = 6;
/// How long it stays on the lighter stream at first, and at most.
pub const LIGHT_FIRST: Duration = Duration::from_secs(120);
pub const LIGHT_LONGEST: Duration = Duration::from_secs(20 * 60);
/// Back on the full stream this long without trouble: the next move starts from the first wait.
pub const LIGHT_SETTLED: Duration = Duration::from_secs(300);
/// A destination that only just went live is still filling its buffers: not judged yet.
const WARM_UP_MS: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Move {
    /// To the lighter stream, for this long.
    Lighter(String, Duration),
    /// Back to the full stream.
    Full(String),
}

#[derive(Debug, Default)]
pub struct Quality {
    /// Seconds in a row each judged destination fell behind, and its skipped count then.
    trouble: HashMap<String, (u32, u64)>,
    /// On the lighter stream: since when, and for how long this time.
    light: HashMap<String, (Instant, Duration)>,
    /// Back on the full stream: when, and the last wait (doubled if it comes down again soon).
    back: HashMap<String, (Instant, Duration)>,
}

impl Quality {
    /// On the lighter stream now.
    pub fn is_light(&self, id: &str) -> bool {
        self.light.contains_key(id)
    }

    /// A new stream: everyone gets the full one again.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// One second: `full` are the destinations on the full stream that can move (their
    /// samples); `slow_pct` is upload protection's "behind" (percent of the stream's rate).
    pub fn tick(&mut self, now: Instant, full: &[Sample<'_>], slow_pct: u8) -> Vec<Move> {
        let mut moves = Vec::new();
        let due: Vec<String> = self
            .light
            .iter()
            .filter(|(_, (at, wait))| now.duration_since(*at) >= *wait)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            if let Some((_, wait)) = self.light.remove(&id) {
                self.back.insert(id.clone(), (now, wait));
                moves.push(Move::Full(id));
            }
        }
        self.back.retain(|_, (at, _)| now.duration_since(*at) < LIGHT_SETTLED);
        let judged: Vec<&Sample<'_>> =
            full.iter().filter(|s| !self.light.contains_key(s.id) && s.live_ms >= WARM_UP_MS).collect();
        let behind = |s: &Sample<'_>, was: u64| {
            s.want_kbps > 0
                && (u64::from(s.kbps) * 100 < u64::from(s.want_kbps) * u64::from(slow_pct) || s.dropped > was)
        };
        let slow: Vec<&str> = judged
            .iter()
            .filter(|s| behind(s, self.trouble.get(s.id).map_or(s.dropped, |t| t.1)))
            .map(|s| s.id)
            .collect();
        // All behind is the upload, unless there's only one to judge.
        let alone = slow.len() < judged.len() || judged.len() == 1;
        let mut trouble = HashMap::new();
        for s in &judged {
            let n = if alone && slow.contains(&s.id) { self.trouble.get(s.id).map_or(1, |t| t.0 + 1) } else { 0 };
            if n >= LIGHTEN_AFTER {
                // Down again soon after coming back: twice as long this time.
                let wait = self.back.remove(s.id).map_or(LIGHT_FIRST, |(_, w)| (w * 2).min(LIGHT_LONGEST));
                self.light.insert(s.id.to_string(), (now, wait));
                moves.push(Move::Lighter(s.id.to_string(), wait));
            } else {
                trouble.insert(s.id.to_string(), (n, s.dropped));
            }
        }
        self.trouble = trouble;
        moves
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, kbps: u32) -> Sample<'_> {
        Sample { id, kbps, want_kbps: 6000, dropped: 0, live_ms: 60_000 }
    }

    #[test]
    fn one_slow_route_takes_the_lighter_stream_and_tries_again_later() {
        let mut q = Quality::default();
        let t0 = Instant::now();
        let sec = |n: u64| t0 + Duration::from_secs(n);
        // Twitch keeps up, Kick doesn't: after six seconds Kick moves.
        for n in 0..5 {
            assert!(q.tick(sec(n), &[s("twitch", 6000), s("kick", 2500)], 70).is_empty());
        }
        assert_eq!(
            q.tick(sec(5), &[s("twitch", 6000), s("kick", 2500)], 70),
            [Move::Lighter("kick".into(), LIGHT_FIRST)]
        );
        assert!(q.is_light("kick"));
        // Its wait over, it tries the full stream again.
        let back = sec(5) + LIGHT_FIRST;
        assert_eq!(q.tick(back, &[s("twitch", 6000)], 70), [Move::Full("kick".into())]);
        // Behind again soon after: down for twice as long.
        let mut moves = Vec::new();
        for n in 1..=6 {
            moves = q.tick(back + Duration::from_secs(n), &[s("twitch", 6000), s("kick", 2000)], 70);
        }
        assert_eq!(moves, [Move::Lighter("kick".into(), LIGHT_FIRST * 2)]);
    }

    #[test]
    fn all_behind_is_the_upload_not_one_route() {
        let mut q = Quality::default();
        let t0 = Instant::now();
        for n in 0..20 {
            let moves = q.tick(t0 + Duration::from_secs(n), &[s("twitch", 2000), s("kick", 2500)], 70);
            assert!(moves.is_empty(), "upload protection's job");
        }
        // The only one: lighter beats pausing it.
        let mut moves = Vec::new();
        for n in 20..26 {
            moves = q.tick(t0 + Duration::from_secs(n), &[s("kick", 2500)], 70);
        }
        assert_eq!(moves, [Move::Lighter("kick".into(), LIGHT_FIRST)]);
    }

    #[test]
    fn skipped_video_counts_and_warm_up_doesnt() {
        let mut q = Quality::default();
        let t0 = Instant::now();
        let mut kick = s("kick", 6000);
        // The first second sets the count; skipping is seen from the next.
        for n in 0..7 {
            kick.dropped += 3;
            let mut fresh = s("twitch", 100);
            fresh.live_ms = 1_000;
            let moves = q.tick(t0 + Duration::from_secs(n), &[fresh, kick], 70);
            if n == 6 {
                assert_eq!(moves, [Move::Lighter("kick".into(), LIGHT_FIRST)], "skipping video is falling behind");
            } else {
                assert!(moves.is_empty());
            }
        }
    }
}
