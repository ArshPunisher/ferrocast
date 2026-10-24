//! Upload protection (docs/plan/LIVE-STUDIO.md A1.8): every destination is a full copy of the
//! stream on the streamer's own upload. When the line can't carry them all, the bottom one in
//! the streamer's order pauses so the ones above stay smooth, and comes back once the line has
//! been calm for a while. A pure state machine, fed one sample per destination every second.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// A destination's rate is judged over this many seconds: a low-bitrate stream arrives in bursts
/// around its keyframes, so one second alone can look slow when nothing is wrong.
const RATE_SECONDS: usize = 3;
/// A destination that only just went live is still filling its buffers: not judged yet.
const WARM_UP_MS: u64 = 3_000;
/// The rule, as Advanced mode writes it (board item 3): pause when a destination gets under
/// `slow_pct` % of its stream, or skips video, for `trouble_s` seconds; pause `first_to_pause`
/// (else the bottom one) first; bring it back after `calm_s` calm seconds, no sooner than
/// `first_pause_s`, doubling each time up to `max_pause_s`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    pub slow_pct: u8,
    pub trouble_s: u32,
    pub calm_s: u32,
    pub first_pause_s: u32,
    pub max_pause_s: u32,
    /// A destination that always pauses first, when it's live.
    pub first_to_pause: Option<String>,
}

impl Default for Rules {
    fn default() -> Self {
        Self { slow_pct: 70, trouble_s: 4, calm_s: 10, first_pause_s: 60, max_pause_s: 600, first_to_pause: None }
    }
}

impl Rules {
    /// Messages name the field.
    pub fn validate(&self) -> Result<(), String> {
        if !(50..=95).contains(&self.slow_pct) {
            return Err("slow_pct: 50 to 95 percent.".into());
        }
        if !(2..=30).contains(&self.trouble_s) {
            return Err("trouble_s: 2 to 30 seconds.".into());
        }
        if !(5..=120).contains(&self.calm_s) {
            return Err("calm_s: 5 to 120 seconds.".into());
        }
        if !(10..=3600).contains(&self.first_pause_s) {
            return Err("first_pause_s: 10 seconds to an hour.".into());
        }
        if self.max_pause_s < self.first_pause_s || self.max_pause_s > 7200 {
            return Err("max_pause_s: at least the first pause, at most two hours.".into());
        }
        Ok(())
    }
}

/// One live destination's last second.
#[derive(Debug, Clone, Copy)]
pub struct Sample<'a> {
    pub id: &'a str,
    pub kbps: u32,
    /// The rate of the stream it forwards (the normal picture or the vertical one).
    pub want_kbps: u32,
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
    rules: Rules,
    /// Paused destinations and the earliest time each may come back.
    paused: Vec<(String, Instant)>,
    last_dropped: Vec<(String, u64)>,
    /// The last few seconds' rates per destination.
    rates: Vec<(String, std::collections::VecDeque<u32>)>,
}

impl Default for Protector {
    fn default() -> Self {
        Self {
            struggle: 0,
            calm: 0,
            next_pause: Duration::from_secs(60),
            rules: Rules::default(),
            paused: Vec::new(),
            last_dropped: Vec::new(),
            rates: Vec::new(),
        }
    }
}

impl Protector {
    /// One second: `live` the destinations sending now (each with its stream's rate), and
    /// `order` every ticked destination, top first. At most one action per tick.
    pub fn tick(&mut self, now: Instant, live: &[Sample<'_>], order: &[&str]) -> Option<Action> {
        if live.iter().all(|s| s.want_kbps == 0) {
            return None;
        }
        // Each live destination's recent rates (the others are forgotten).
        self.rates.retain(|(id, _)| live.iter().any(|s| s.id == id));
        for s in live {
            match self.rates.iter_mut().find(|(id, _)| id == s.id) {
                Some((_, q)) => {
                    q.push_back(s.kbps);
                    if q.len() > RATE_SECONDS {
                        q.pop_front();
                    }
                }
                None => self.rates.push((s.id.to_string(), std::collections::VecDeque::from([s.kbps]))),
            }
        }
        let judged: Vec<&Sample<'_>> = live.iter().filter(|s| s.live_ms >= WARM_UP_MS && s.want_kbps > 0).collect();
        let mut trouble = false;
        for s in &judged {
            let before = self.last_dropped.iter().find(|(id, _)| id == s.id).map(|(_, d)| *d);
            let avg = self
                .rates
                .iter()
                .find(|(id, _)| id == s.id)
                .map(|(_, q)| q.iter().map(|k| u64::from(*k)).sum::<u64>() / q.len().max(1) as u64)
                .unwrap_or(u64::from(s.kbps));
            if before.is_some_and(|b| s.dropped > b)
                || (avg as f64) < f64::from(s.want_kbps) * f64::from(self.rules.slow_pct) / 100.0
            {
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

        if self.struggle >= self.rules.trouble_s && live.len() >= 2 {
            // The bottom-most destination that's sending: it pauses so the top ones keep going.
            let sending = |id: &str| live.iter().any(|s| s.id == id);
            let chosen = self.rules.first_to_pause.as_deref().filter(|id| sending(id) && order.contains(id));
            let victim = chosen.or_else(|| order.iter().rev().find(|id| sending(id)).copied())?;
            self.struggle = 0;
            self.paused.push((victim.to_string(), now + self.next_pause));
            self.next_pause = (self.next_pause * 2).min(Duration::from_secs(u64::from(self.rules.max_pause_s)));
            return Some(Action::Pause(victim.to_string()));
        }
        if self.calm >= self.rules.calm_s {
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
        let rules = self.rules.clone();
        *self = Self::default();
        self.set_rules(rules);
    }

    /// The streamer's rule (Advanced mode); a pause already running keeps its end.
    pub fn set_rules(&mut self, rules: Rules) {
        if self.paused.is_empty() {
            self.next_pause = Duration::from_secs(u64::from(rules.first_pause_s));
        }
        self.rules = rules;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, kbps: u32) -> Sample<'_> {
        Sample { id, kbps, want_kbps: 6000, dropped: 0, live_ms: 10_000 }
    }

    const ORDER: &[&str] = &["twitch", "youtube", "kick"];

    #[test]
    fn a_struggling_line_pauses_the_bottom_one_after_four_seconds() {
        let mut p = Protector::default();
        let t = Instant::now();
        let slow = [s("twitch", 6000), s("youtube", 3000), s("kick", 3000)];
        for i in 0..3 {
            assert_eq!(p.tick(t + Duration::from_secs(i), &slow, ORDER), None, "a burst isn't trouble");
        }
        assert_eq!(p.tick(t + Duration::from_secs(3), &slow, ORDER), Some(Action::Pause("kick".into())));
        assert!(p.is_paused("kick"));
        // Still struggling with two left: the next one up pauses after another four seconds.
        let two = [s("twitch", 6000), s("youtube", 3000)];
        let mut next = None;
        for i in 4..8 {
            next = p.tick(t + Duration::from_secs(i), &two, ORDER);
        }
        assert_eq!(next, Some(Action::Pause("youtube".into())));
        // One left: never paused, whatever happens.
        for i in 8..20 {
            assert_eq!(p.tick(t + Duration::from_secs(i), &[s("twitch", 1000)], ORDER), None);
        }
    }

    #[test]
    fn it_comes_back_after_calm_and_the_pause_doubles() {
        let mut p = Protector::default();
        let t = Instant::now();
        let slow = [s("twitch", 6000), s("kick", 1000)];
        for i in 0..4 {
            p.tick(t + Duration::from_secs(i), &slow, &["twitch", "kick"]);
        }
        assert!(p.is_paused("kick"));
        let calm = [s("twitch", 6100)];
        // Calm, but the first pause lasts 60 s.
        for i in 4..60 {
            assert_eq!(p.tick(t + Duration::from_secs(i), &calm, &["twitch", "kick"]), None, "second {i}");
        }
        let mut back = None;
        for i in 60..75 {
            if let Some(a) = p.tick(t + Duration::from_secs(i), &calm, &["twitch", "kick"]) {
                back = Some((i, a));
                break;
            }
        }
        assert_eq!(back.map(|(_, a)| a), Some(Action::Resume("kick".into())));
        // Trouble again: this pause is 120 s.
        let now = t + Duration::from_secs(80);
        for i in 0..4 {
            p.tick(now + Duration::from_secs(i), &slow, &["twitch", "kick"]);
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
            let live =
                [s("twitch", 6000), Sample { id: "youtube", kbps: 6000, want_kbps: 6000, dropped, live_ms: 10_000 }];
            last = p.tick(t + Duration::from_secs(i), &live, &["twitch", "youtube"]);
        }
        // The first sample has no "before": three growing ticks after it aren't four yet.
        assert_eq!(last, None);
        dropped += 50;
        let live = [s("twitch", 6000), Sample { id: "youtube", kbps: 6000, want_kbps: 6000, dropped, live_ms: 10_000 }];
        assert_eq!(
            p.tick(t + Duration::from_secs(4), &live, &["twitch", "youtube"]),
            Some(Action::Pause("youtube".into()))
        );
        // A destination that just went live at 0 kbps isn't judged.
        let mut q = Protector::default();
        let fresh = [s("twitch", 6000), Sample { id: "kick", kbps: 0, want_kbps: 6000, dropped: 0, live_ms: 1_000 }];
        for i in 0..10 {
            assert_eq!(q.tick(t + Duration::from_secs(i), &fresh, ORDER), None);
        }
    }

    #[test]
    fn the_streamers_rule_is_used() {
        let mut p = Protector::default();
        p.set_rules(Rules {
            slow_pct: 50,
            trouble_s: 2,
            first_pause_s: 30,
            max_pause_s: 40,
            first_to_pause: Some("twitch".into()),
            ..Rules::default()
        });
        let t = Instant::now();
        let order = &["twitch", "youtube", "kick"];
        // 60% of the rate isn't slow under a 50% rule.
        let fine = [s("twitch", 3600), s("youtube", 3600), s("kick", 3600)];
        for i in 0..5 {
            assert_eq!(p.tick(t + Duration::from_secs(i), &fine, order), None);
        }
        // Slow: after 2 seconds the chosen one pauses, not the bottom one.
        let slow = [s("twitch", 1000), s("youtube", 1000), s("kick", 1000)];
        p.tick(t + Duration::from_secs(5), &slow, order);
        assert_eq!(p.tick(t + Duration::from_secs(6), &slow, order), Some(Action::Pause("twitch".into())));
        assert!(p.paused_until("twitch").unwrap() >= t + Duration::from_secs(36));
        // Then the bottom one; the second pause is capped at 40 s.
        let two = [s("youtube", 1000), s("kick", 1000)];
        p.tick(t + Duration::from_secs(7), &two, order);
        assert_eq!(p.tick(t + Duration::from_secs(8), &two, order), Some(Action::Pause("kick".into())));
        assert!(p.paused_until("kick").unwrap() <= t + Duration::from_secs(48));
        assert!(Rules { slow_pct: 99, ..Rules::default() }.validate().unwrap_err().starts_with("slow_pct:"));
        assert!(Rules { max_pause_s: 30, ..Rules::default() }.validate().unwrap_err().starts_with("max_pause_s:"));
        assert!(Rules::default().validate().is_ok());
    }

    #[test]
    fn a_bursty_low_rate_stream_is_not_trouble() {
        // A still scene: 370 and 1,670 kbps in turns against a 1,040 kbps stream.
        let mut p = Protector::default();
        let t = Instant::now();
        let order = &["twitch", "shorts"];
        for i in 0..20 {
            let k = if i % 2 == 0 { 370 } else { 1670 };
            let live =
                [s("twitch", 6000), Sample { id: "shorts", kbps: k, want_kbps: 1040, dropped: 0, live_ms: 10_000 }];
            assert_eq!(p.tick(t + Duration::from_secs(i), &live, order), None, "second {i}");
        }
    }

    #[test]
    fn released_and_reset_clear_the_pause() {
        let mut p = Protector::default();
        let t = Instant::now();
        for i in 0..4 {
            p.tick(t + Duration::from_secs(i), &[s("twitch", 6000), s("kick", 10)], &["twitch", "kick"]);
        }
        assert!(p.is_paused("kick"));
        p.release("kick");
        assert!(!p.is_paused("kick"));
        p.reset();
        assert_eq!(p.next_pause, Duration::from_secs(60));
        // Nothing streaming: nothing happens.
        assert_eq!(p.tick(t, &[], &[]), None);
    }
}
