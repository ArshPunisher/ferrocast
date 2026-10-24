//! Upload protection (docs/plan/LIVE-STUDIO.md A1.8): every destination is a full copy of the
//! stream on the streamer's own upload. When the line can't carry them all, the bottom one in
//! the streamer's order pauses so the ones above stay smooth, and comes back once the line has
//! been calm for a while. A pure state machine, fed one sample per destination every second.

use std::time::{Duration, Instant};

/// A live destination getting less than this share of the stream's rate is struggling.
const SLOW_SHARE: f64 = 0.7;
/// Seconds of trouble in a row before something pauses (a keyframe burst isn't trouble).
const STRUGGLE_TICKS: u32 = 4;
/// Calm seconds in a row before a paused destination may come back.
const CALM_TICKS: u32 = 10;
/// A destination that only just went live is still filling its buffers: not judged yet.
const WARM_UP_MS: u64 = 3_000;
/// How long the first pause lasts at least; each pause in the same stream doubles it.
const FIRST_PAUSE: Duration = Duration::from_secs(60);
const MAX_PAUSE: Duration = Duration::from_secs(600);

/// One live destination's last second.
#[derive(Debug, Clone, Copy)]
pub struct Sample<'a> {
    pub id: &'a str,
    pub kbps: u32,
    /// Packets skipped so far this session (it only grows).
    pub dropped: u64,
    /// How long this session has been live.
    pub live_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Pause(String),
    Resume(String),
}

#[derive(Debug)]
pub struct Protector {
    struggle: u32,
    calm: u32,
    next_pause: Duration,
    /// Paused destinations and the earliest time each may come back.
    paused: Vec<(String, Instant)>,
    last_dropped: Vec<(String, u64)>,
}

impl Default for Protector {
    fn default() -> Self {
        Self { struggle: 0, calm: 0, next_pause: FIRST_PAUSE, paused: Vec::new(), last_dropped: Vec::new() }
    }
}

impl Protector {
    /// One second: `stream_kbps` is what OBS sends, `live` the destinations sending now, and
    /// `order` every ticked destination, top first. At most one action per tick.
    pub fn tick(&mut self, now: Instant, stream_kbps: u32, live: &[Sample<'_>], order: &[&str]) -> Option<Action> {
        if stream_kbps == 0 {
            return None;
        }
        let judged: Vec<&Sample<'_>> = live.iter().filter(|s| s.live_ms >= WARM_UP_MS).collect();
        let mut trouble = false;
        for s in &judged {
            let before = self.last_dropped.iter().find(|(id, _)| id == s.id).map(|(_, d)| *d);
            if before.is_some_and(|b| s.dropped > b) || f64::from(s.kbps) < f64::from(stream_kbps) * SLOW_SHARE {
                trouble = true;
            }
        }
        self.last_dropped = live.iter().map(|s| (s.id.to_string(), s.dropped)).collect();
        if trouble {
            self.struggle += 1;
            self.calm = 0;
        } else {
            self.calm += 1;
            self.struggle = 0;
        }

        if self.struggle >= STRUGGLE_TICKS && live.len() >= 2 {
            // The bottom-most destination that's sending: it pauses so the top ones keep going.
            let victim = order.iter().rev().find(|id| live.iter().any(|s| s.id == **id))?;
            self.struggle = 0;
            self.paused.push((victim.to_string(), now + self.next_pause));
            self.next_pause = (self.next_pause * 2).min(MAX_PAUSE);
            return Some(Action::Pause(victim.to_string()));
        }
        if self.calm >= CALM_TICKS {
            // The highest one in the order comes back first.
            let ready =
                order.iter().find(|id| self.paused.iter().any(|(p, until)| p == **id && *until <= now))?.to_string();
            self.paused.retain(|(p, _)| *p != ready);
            self.calm = 0;
            return Some(Action::Resume(ready));
        }
        None
    }

    pub fn is_paused(&self, id: &str) -> bool {
        self.paused.iter().any(|(p, _)| p == id)
    }

    /// When a paused destination may come back.
    pub fn paused_until(&self, id: &str) -> Option<Instant> {
        self.paused.iter().find(|(p, _)| p == id).map(|(_, u)| *u)
    }

    /// The streamer pressed "Resume now" (or unticked it): it's no longer paused.
    pub fn release(&mut self, id: &str) {
        self.paused.retain(|(p, _)| p != id);
    }

    /// A new stream starts with nothing paused and the shortest pause.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, kbps: u32) -> Sample<'_> {
        Sample { id, kbps, dropped: 0, live_ms: 10_000 }
    }

    const ORDER: &[&str] = &["twitch", "youtube", "kick"];

    #[test]
    fn a_struggling_line_pauses_the_bottom_one_after_four_seconds() {
        let mut p = Protector::default();
        let t = Instant::now();
        let slow = [s("twitch", 6000), s("youtube", 3000), s("kick", 3000)];
        for i in 0..3 {
            assert_eq!(p.tick(t + Duration::from_secs(i), 6000, &slow, ORDER), None, "a burst isn't trouble");
        }
        assert_eq!(p.tick(t + Duration::from_secs(3), 6000, &slow, ORDER), Some(Action::Pause("kick".into())));
        assert!(p.is_paused("kick"));
        // Still struggling with two left: the next one up pauses after another four seconds.
        let two = [s("twitch", 6000), s("youtube", 3000)];
        let mut next = None;
        for i in 4..8 {
            next = p.tick(t + Duration::from_secs(i), 6000, &two, ORDER);
        }
        assert_eq!(next, Some(Action::Pause("youtube".into())));
        // One left: never paused, whatever happens.
        for i in 8..20 {
            assert_eq!(p.tick(t + Duration::from_secs(i), 6000, &[s("twitch", 1000)], ORDER), None);
        }
    }

    #[test]
    fn it_comes_back_after_calm_and_the_pause_doubles() {
        let mut p = Protector::default();
        let t = Instant::now();
        let slow = [s("twitch", 6000), s("kick", 1000)];
        for i in 0..4 {
            p.tick(t + Duration::from_secs(i), 6000, &slow, &["twitch", "kick"]);
        }
        assert!(p.is_paused("kick"));
        let calm = [s("twitch", 6100)];
        // Calm, but the first pause lasts 60 s.
        for i in 4..60 {
            assert_eq!(p.tick(t + Duration::from_secs(i), 6000, &calm, &["twitch", "kick"]), None, "second {i}");
        }
        let mut back = None;
        for i in 60..75 {
            if let Some(a) = p.tick(t + Duration::from_secs(i), 6000, &calm, &["twitch", "kick"]) {
                back = Some((i, a));
                break;
            }
        }
        assert_eq!(back.map(|(_, a)| a), Some(Action::Resume("kick".into())));
        // Trouble again: this pause is 120 s.
        let now = t + Duration::from_secs(80);
        for i in 0..4 {
            p.tick(now + Duration::from_secs(i), 6000, &slow, &["twitch", "kick"]);
        }
        let until = p.paused_until("kick").unwrap();
        assert!(until >= now + Duration::from_secs(120));
    }

    #[test]
    fn skipped_packets_are_trouble_and_new_sessions_warm_up_first() {
        let mut p = Protector::default();
        let t = Instant::now();
        let mut dropped = 0;
        let mut last = None;
        for i in 0..4 {
            dropped += 50;
            let live = [s("twitch", 6000), Sample { id: "youtube", kbps: 6000, dropped, live_ms: 10_000 }];
            last = p.tick(t + Duration::from_secs(i), 6000, &live, &["twitch", "youtube"]);
        }
        // The first sample has no "before": three growing ticks after it aren't four yet.
        assert_eq!(last, None);
        dropped += 50;
        let live = [s("twitch", 6000), Sample { id: "youtube", kbps: 6000, dropped, live_ms: 10_000 }];
        assert_eq!(
            p.tick(t + Duration::from_secs(4), 6000, &live, &["twitch", "youtube"]),
            Some(Action::Pause("youtube".into()))
        );
        // A destination that just went live at 0 kbps isn't judged.
        let mut q = Protector::default();
        let fresh = [s("twitch", 6000), Sample { id: "kick", kbps: 0, dropped: 0, live_ms: 1_000 }];
        for i in 0..10 {
            assert_eq!(q.tick(t + Duration::from_secs(i), 6000, &fresh, ORDER), None);
        }
    }

    #[test]
    fn released_and_reset_clear_the_pause() {
        let mut p = Protector::default();
        let t = Instant::now();
        for i in 0..4 {
            p.tick(t + Duration::from_secs(i), 6000, &[s("twitch", 6000), s("kick", 10)], &["twitch", "kick"]);
        }
        assert!(p.is_paused("kick"));
        p.release("kick");
        assert!(!p.is_paused("kick"));
        p.reset();
        assert_eq!(p.next_pause, FIRST_PAUSE);
        // Nothing streaming: nothing happens.
        assert_eq!(p.tick(t, 0, &[], &[]), None);
    }
}
