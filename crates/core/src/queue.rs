//! The alert queue as a state machine (R-31).
//!
//! One alert plays at a time. The overlay says when it finished; if it never does (OBS
//! hid the source, the page crashed) a deadline moves the queue on. After an outage the
//! queue doesn't fire fifty stale alerts in a row: old ones are folded into one summary.
//! The engine persists every change, so a crash never loses a paid alert.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::alert::RenderedAlert;
use crate::event::Millis;

/// Tunable timings. Kept together so the settings screen and tests use the same numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueTuning {
    /// Extra time after an alert's own duration before we stop waiting for its "finished".
    pub ack_grace_ms: i64,
    /// Breathing room between two alerts.
    pub gap_ms: i64,
    /// Alerts that waited longer than this are folded into a summary instead of played.
    /// 0 = never fold.
    pub stale_after_ms: i64,
    /// Hard cap on waiting alerts; beyond it the oldest lowest-priority ones are folded.
    pub max_waiting: usize,
}

impl Default for QueueTuning {
    fn default() -> Self {
        QueueTuning { ack_grace_ms: 5_000, gap_ms: 600, stale_after_ms: 180_000, max_waiting: 200 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Waiting {
    pub alert: RenderedAlert,
    pub priority: i32,
    pub enqueued_at: Millis,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playing {
    pub alert: RenderedAlert,
    pub started_at: Millis,
    pub deadline: Millis,
    /// The overlay confirmed it began showing.
    pub acknowledged: bool,
}

/// What the engine must do after a call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// Send this alert to the overlays.
    Show { alert: Box<RenderedAlert> },
    /// Tell overlays to remove the current alert now (skip or timeout).
    Hide { alert_id: String },
    /// Stale alerts were folded; show "N alerts while you were away" in the feed.
    Folded { alert_ids: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckPhase {
    Started,
    Finished,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AlertQueue {
    pub tuning: QueueTuning,
    waiting: VecDeque<Waiting>,
    playing: Option<Playing>,
    paused: bool,
    /// Earliest time the next alert may start (gap after the previous one).
    next_at: Millis,
}

impl AlertQueue {
    pub fn new(tuning: QueueTuning) -> Self {
        AlertQueue { tuning, ..Default::default() }
    }

    /// Adds an alert. Higher priority goes first; equal priority keeps arrival order.
    pub fn enqueue(&mut self, alert: RenderedAlert, priority: i32, now: Millis) -> Vec<Action> {
        if self.contains(&alert.alert_id) {
            return Vec::new();
        }
        let pos = self.waiting.iter().position(|w| w.priority < priority).unwrap_or(self.waiting.len());
        self.waiting.insert(pos, Waiting { alert, priority, enqueued_at: now });
        let mut actions = self.enforce_cap();
        actions.extend(self.tick(now));
        actions
    }

    /// Advances time: times out the current alert, folds stale ones, starts the next.
    pub fn tick(&mut self, now: Millis) -> Vec<Action> {
        let mut actions = Vec::new();
        if let Some(p) = &self.playing
            && now >= p.deadline
        {
            actions.push(Action::Hide { alert_id: p.alert.alert_id.clone() });
            self.finish(now);
        }
        if self.playing.is_some() || self.paused || now < self.next_at {
            return actions;
        }
        let stale: Vec<String> = if self.tuning.stale_after_ms > 0 {
            let cutoff = now.0 - self.tuning.stale_after_ms;
            let (old, fresh): (Vec<_>, Vec<_>) = self.waiting.drain(..).partition(|w| w.enqueued_at.0 < cutoff);
            self.waiting = fresh.into();
            old.into_iter().map(|w| w.alert.alert_id).collect()
        } else {
            Vec::new()
        };
        if !stale.is_empty() {
            actions.push(Action::Folded { alert_ids: stale });
        }
        if let Some(next) = self.waiting.pop_front() {
            let deadline = Millis(now.0 + i64::from(next.alert.duration_ms) + self.tuning.ack_grace_ms);
            actions.push(Action::Show { alert: Box::new(next.alert.clone()) });
            self.playing = Some(Playing { alert: next.alert, started_at: now, deadline, acknowledged: false });
        }
        actions
    }

    /// The overlay reports progress. Acks for anything but the current alert are ignored,
    /// so a stale or forged ack can't skip someone's paid alert.
    pub fn ack(&mut self, alert_id: &str, phase: AckPhase, now: Millis) -> Vec<Action> {
        let Some(p) = &mut self.playing else {
            return Vec::new();
        };
        if p.alert.alert_id != alert_id {
            return Vec::new();
        }
        match phase {
            AckPhase::Started => {
                if !p.acknowledged {
                    p.acknowledged = true;
                    // Restart the clock from when it actually appeared.
                    p.deadline = Millis(now.0 + i64::from(p.alert.duration_ms) + self.tuning.ack_grace_ms);
                }
                Vec::new()
            }
            AckPhase::Finished | AckPhase::Failed => {
                self.finish(now);
                self.tick(now)
            }
        }
    }

    /// Ends the current alert now.
    pub fn skip(&mut self, now: Millis) -> Vec<Action> {
        let Some(p) = &self.playing else {
            return Vec::new();
        };
        let mut actions = vec![Action::Hide { alert_id: p.alert.alert_id.clone() }];
        self.finish(now);
        actions.extend(self.tick(now));
        actions
    }

    pub fn pause(&mut self) {
        self.paused = true;
    }

    pub fn resume(&mut self, now: Millis) -> Vec<Action> {
        self.paused = false;
        self.tick(now)
    }

    /// Drops everything waiting (the current alert finishes normally).
    pub fn clear_waiting(&mut self) -> Vec<String> {
        self.waiting.drain(..).map(|w| w.alert.alert_id).collect()
    }

    /// Removes one waiting alert (the streamer declined it in the approval list).
    pub fn remove(&mut self, alert_id: &str) -> bool {
        let before = self.waiting.len();
        self.waiting.retain(|w| w.alert.alert_id != alert_id);
        before != self.waiting.len()
    }

    /// Removes the speech from waiting alerts for `event_id` (a mod deleted the message,
    /// so it must not be read aloud). Returns how many alerts changed.
    pub fn strip_tts(&mut self, event_id: &str) -> usize {
        let mut n = 0;
        for w in self.waiting.iter_mut().filter(|w| w.alert.event_id == event_id) {
            if w.alert.tts_text.take().is_some() | w.alert.tts_audio.take().is_some() {
                n += 1;
            }
        }
        n
    }

    /// Attaches synthesised speech to a waiting alert. `false` if it already started or is
    /// gone (the overlay then speaks `tts_text` with the browser's voice).
    pub fn set_tts_audio(&mut self, alert_id: &str, media_id: &str) -> bool {
        match self.waiting.iter_mut().find(|w| w.alert.alert_id == alert_id) {
            Some(w) if w.alert.tts_text.is_some() => {
                w.alert.tts_audio = Some(media_id.to_string());
                true
            }
            _ => false,
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn playing(&self) -> Option<&Playing> {
        self.playing.as_ref()
    }

    pub fn waiting(&self) -> impl Iterator<Item = &Waiting> {
        self.waiting.iter()
    }

    pub fn len(&self) -> usize {
        self.waiting.len() + usize::from(self.playing.is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// When the engine should call `tick` next, if anything is pending.
    pub fn next_wake(&self) -> Option<Millis> {
        match &self.playing {
            Some(p) => Some(p.deadline),
            None if !self.waiting.is_empty() && !self.paused => Some(self.next_at),
            None => None,
        }
    }

    fn contains(&self, alert_id: &str) -> bool {
        self.playing.as_ref().is_some_and(|p| p.alert.alert_id == alert_id)
            || self.waiting.iter().any(|w| w.alert.alert_id == alert_id)
    }

    fn finish(&mut self, now: Millis) {
        self.playing = None;
        self.next_at = Millis(now.0 + self.tuning.gap_ms);
    }

    fn enforce_cap(&mut self) -> Vec<Action> {
        let mut folded = Vec::new();
        while self.waiting.len() > self.tuning.max_waiting.max(1) {
            // The back holds the lowest priority, newest last; fold the oldest of those.
            let lowest = self.waiting.iter().map(|w| w.priority).min().unwrap_or(0);
            if let Some(idx) = self.waiting.iter().position(|w| w.priority == lowest)
                && let Some(w) = self.waiting.remove(idx)
            {
                folded.push(w.alert.alert_id);
            }
        }
        if folded.is_empty() { Vec::new() } else { vec![Action::Folded { alert_ids: folded }] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{default_rules, render, select};
    use crate::sample::any_sample;

    fn alert(id: &str, ms: u32) -> RenderedAlert {
        let e = any_sample("follow", Millis(0)).unwrap();
        let rules = default_rules();
        let (r, v) = select(&rules, &e).unwrap();
        let mut a = render(id, r, v, &e);
        a.duration_ms = ms;
        a
    }

    fn shown(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Show { alert } => Some(alert.alert_id.clone()),
                _ => None,
            })
            .collect()
    }

    fn q() -> AlertQueue {
        AlertQueue::new(QueueTuning { ack_grace_ms: 1_000, gap_ms: 100, stale_after_ms: 60_000, max_waiting: 10 })
    }

    #[test]
    fn plays_one_at_a_time_in_order() {
        let mut q = q();
        assert_eq!(shown(&q.enqueue(alert("a", 2_000), 0, Millis(0))), vec!["a"]);
        assert!(shown(&q.enqueue(alert("b", 2_000), 0, Millis(10))).is_empty());
        assert!(shown(&q.ack("a", AckPhase::Finished, Millis(2_000))).is_empty(), "gap first");
        assert_eq!(shown(&q.tick(Millis(2_100))), vec!["b"]);
    }

    #[test]
    fn priority_jumps_the_line_but_never_interrupts() {
        let mut q = q();
        q.enqueue(alert("playing", 1_000), 0, Millis(0));
        q.enqueue(alert("low", 1_000), 0, Millis(1));
        q.enqueue(alert("high", 1_000), 5, Millis(2));
        assert_eq!(q.playing().unwrap().alert.alert_id, "playing");
        let order: Vec<_> = q.waiting().map(|w| w.alert.alert_id.as_str()).collect();
        assert_eq!(order, vec!["high", "low"]);
    }

    #[test]
    fn a_silent_overlay_times_out() {
        let mut q = q();
        q.enqueue(alert("a", 2_000), 0, Millis(0));
        q.enqueue(alert("b", 2_000), 0, Millis(0));
        assert!(q.tick(Millis(2_999)).is_empty());
        let acts = q.tick(Millis(3_000));
        assert!(matches!(&acts[0], Action::Hide { alert_id } if alert_id == "a"));
        assert_eq!(shown(&q.tick(Millis(3_100))), vec!["b"]);
    }

    #[test]
    fn started_ack_restarts_the_clock() {
        let mut q = q();
        q.enqueue(alert("a", 2_000), 0, Millis(0));
        q.ack("a", AckPhase::Started, Millis(2_500));
        assert!(q.tick(Millis(3_500)).is_empty(), "deadline moved to 5500");
        assert!(!q.tick(Millis(5_500)).is_empty());
    }

    #[test]
    fn forged_or_stale_acks_are_ignored() {
        let mut q = q();
        q.enqueue(alert("a", 2_000), 0, Millis(0));
        q.enqueue(alert("b", 2_000), 0, Millis(0));
        assert!(q.ack("b", AckPhase::Finished, Millis(10)).is_empty());
        assert!(q.ack("zzz", AckPhase::Failed, Millis(10)).is_empty());
        assert_eq!(q.playing().unwrap().alert.alert_id, "a");
    }

    #[test]
    fn duplicate_alert_ids_are_not_queued_twice() {
        let mut q = q();
        q.enqueue(alert("a", 1_000), 0, Millis(0));
        q.enqueue(alert("a", 1_000), 0, Millis(0));
        q.enqueue(alert("b", 1_000), 0, Millis(0));
        q.enqueue(alert("b", 1_000), 0, Millis(0));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn after_an_outage_stale_alerts_are_folded() {
        let mut q = q();
        q.pause();
        for i in 0..5 {
            q.enqueue(alert(&format!("old{i}"), 1_000), 0, Millis(0));
        }
        q.enqueue(alert("fresh", 1_000), 0, Millis(100_000));
        let acts = q.resume(Millis(100_000));
        assert!(matches!(&acts[0], Action::Folded { alert_ids } if alert_ids.len() == 5));
        assert_eq!(shown(&acts), vec!["fresh"]);
    }

    #[test]
    fn a_flood_is_capped() {
        let mut q = q();
        q.pause();
        let mut folded = 0;
        for i in 0..50 {
            for a in q.enqueue(alert(&format!("x{i}"), 1_000), 0, Millis(i)) {
                if let Action::Folded { alert_ids } = a {
                    folded += alert_ids.len();
                }
            }
        }
        assert_eq!(q.waiting().count(), 10);
        assert_eq!(folded, 40);
    }

    #[test]
    fn pause_skip_and_remove() {
        let mut q = q();
        q.enqueue(alert("a", 5_000), 0, Millis(0));
        q.enqueue(alert("b", 5_000), 0, Millis(0));
        q.enqueue(alert("c", 5_000), 0, Millis(0));
        assert!(q.remove("c"));
        assert!(!q.remove("c"));
        q.pause();
        let acts = q.skip(Millis(10));
        assert!(matches!(&acts[0], Action::Hide { .. }));
        assert!(shown(&acts).is_empty(), "paused: nothing new starts");
        assert_eq!(shown(&q.resume(Millis(200))), vec!["b"]);
        assert_eq!(q.next_wake(), Some(Millis(200 + 5_000 + 1_000)));
    }

    #[test]
    fn strip_tts_only_touches_that_event() {
        let mut q = q();
        q.pause();
        let mut a = alert("a", 1_000);
        a.event_id = "e1".into();
        a.tts_text = Some("read me".into());
        let mut b = alert("b", 1_000);
        b.event_id = "e2".into();
        b.tts_text = Some("keep me".into());
        q.enqueue(a, 0, Millis(0));
        q.enqueue(b, 0, Millis(0));
        assert_eq!(q.strip_tts("e1"), 1);
        assert_eq!(q.strip_tts("e1"), 0);
        let texts: Vec<_> = q.waiting().map(|w| w.alert.tts_text.clone()).collect();
        assert_eq!(texts, vec![None, Some("keep me".into())]);
    }

    #[test]
    fn survives_a_serde_round_trip() {
        let mut q = q();
        q.enqueue(alert("a", 1_000), 0, Millis(0));
        q.enqueue(alert("b", 1_000), 3, Millis(0));
        let json = serde_json::to_string(&q).unwrap();
        let back: AlertQueue = serde_json::from_str(&json).unwrap();
        assert_eq!(q, back);
    }
}
