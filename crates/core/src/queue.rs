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
    /// Alerts play without their sound or speech (the dashboard's "Mute alerts").
    #[serde(default)]
    muted: bool,
    /// Earliest time the next alert may start (gap after the previous one).
    next_at: Millis,
}

impl AlertQueue {
    pub fn new(tuning: QueueTuning) -> Self {
        AlertQueue { tuning, ..Default::default() }
    }

    /// Adds an alert. Higher priority goes first; equal priority keeps arrival order.
    pub fn enqueue(&mut self, alert: RenderedAlert, priority: i32, now: Millis) -> Vec<Action> {
        self.enqueue_with(alert, priority, false, now)
    }

    /// Like [`enqueue`](Self::enqueue). With `interrupt`, an alert that is playing ends now
    /// (a `Hide`, no gap) and this one goes to the front and shows at once. While paused it
    /// just waits, and with nothing playing it is an ordinary enqueue.
    pub fn enqueue_with(&mut self, alert: RenderedAlert, priority: i32, interrupt: bool, now: Millis) -> Vec<Action> {
        if self.contains(&alert.alert_id) {
            return Vec::new();
        }
        if interrupt
            && !self.paused
            && let Some(p) = &self.playing
        {
            let mut actions = vec![Action::Hide { alert_id: p.alert.alert_id.clone() }];
            self.finish(now);
            self.next_at = now;
            self.waiting.push_front(Waiting { alert, priority, enqueued_at: now });
            actions.extend(self.tick(now));
            // The new alert is already playing, so the cap only folds waiting ones.
            actions.extend(self.enforce_cap(None));
            return actions;
        }
        let pos = self.waiting.iter().position(|w| w.priority < priority).unwrap_or(self.waiting.len());
        self.waiting.insert(pos, Waiting { alert, priority, enqueued_at: now });
        let mut actions = self.enforce_cap(None);
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
            let mut next = next;
            if self.muted {
                next.alert.sound = None;
                next.alert.tts_text = None;
                next.alert.tts_audio = None;
                // A design brings its own sound and, if on, synthesised impact hits.
                if let Some(d) = &mut next.alert.design {
                    d.look.sound.media_ref = None;
                    d.look.sound.volume = 0;
                    d.look.motion.impact_sounds = false;
                }
            }
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

    /// Mutes or unmutes the sound and speech of alerts that start from now on.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    pub fn is_muted(&self) -> bool {
        self.muted
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

    /// Folds the oldest lowest-priority alerts past the cap; never `keep` (an interrupting alert).
    fn enforce_cap(&mut self, keep: Option<&str>) -> Vec<Action> {
        let mut folded = Vec::new();
        while self.waiting.len() > self.tuning.max_waiting.max(1) {
            // Fold the oldest of the lowest priority, never `keep`.
            let candidates =
                || self.waiting.iter().enumerate().filter(|(_, w)| Some(w.alert.alert_id.as_str()) != keep);
            let Some(lowest) = candidates().map(|(_, w)| w.priority).min() else { break };
            let Some(idx) = candidates().find(|(_, w)| w.priority == lowest).map(|(i, _)| i) else { break };
            if let Some(w) = self.waiting.remove(idx) {
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

    fn hidden(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Hide { alert_id } => Some(alert_id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn interrupt_ends_the_playing_alert_and_shows_at_once() {
        let mut q = q();
        q.enqueue(alert("a", 5_000), 0, Millis(0));
        q.enqueue(alert("w", 5_000), 0, Millis(1));
        let acts = q.enqueue_with(alert("i", 5_000), 0, true, Millis(100));
        assert_eq!(hidden(&acts), vec!["a"]);
        assert_eq!(shown(&acts), vec!["i"], "no gap, no waiting");
        assert_eq!(q.playing().unwrap().alert.alert_id, "i");
        let order: Vec<_> = q.waiting().map(|w| w.alert.alert_id.clone()).collect();
        assert_eq!(order, vec!["w"], "the one already waiting keeps its place");
    }

    #[test]
    fn interrupt_with_nothing_playing_is_a_normal_enqueue() {
        let mut q = q();
        let acts = q.enqueue_with(alert("i", 1_000), 0, true, Millis(0));
        assert!(hidden(&acts).is_empty());
        assert_eq!(shown(&acts), vec!["i"]);
    }

    #[test]
    fn interrupt_while_paused_just_waits() {
        let mut q = q();
        q.enqueue(alert("a", 5_000), 0, Millis(0));
        q.pause();
        let acts = q.enqueue_with(alert("i", 1_000), 0, true, Millis(10));
        assert!(acts.is_empty());
        assert_eq!(q.playing().unwrap().alert.alert_id, "a");
        assert_eq!(q.waiting().count(), 1);
    }

    #[test]
    fn a_non_interrupting_alert_never_ends_the_playing_one() {
        let mut q = q();
        q.enqueue(alert("a", 5_000), 0, Millis(0));
        let acts = q.enqueue_with(alert("b", 5_000), 9, false, Millis(10));
        assert!(acts.is_empty());
    }

    #[test]
    fn a_duplicate_interrupting_alert_changes_nothing() {
        let mut q = q();
        q.enqueue(alert("a", 5_000), 0, Millis(0));
        assert!(q.enqueue_with(alert("a", 5_000), 0, true, Millis(10)).is_empty());
        assert_eq!(q.playing().unwrap().alert.alert_id, "a");
    }

    #[test]
    fn two_interrupts_in_a_row_each_replace_the_last() {
        let mut q = q();
        q.enqueue(alert("a", 5_000), 0, Millis(0));
        q.enqueue_with(alert("i1", 5_000), 0, true, Millis(10));
        let acts = q.enqueue_with(alert("i2", 5_000), 0, true, Millis(20));
        assert_eq!((hidden(&acts), shown(&acts)), (vec!["i1".into()], vec!["i2".into()]));
    }

    #[test]
    fn mute_strips_sound_and_speech_when_the_alert_starts() {
        let mut q = q();
        q.set_muted(true);
        let mut a = alert("a", 1_000);
        a.sound = Some("builtin:chime".into());
        a.tts_text = Some("hi".into());
        a.tts_audio = Some("m1".into());
        let acts = q.enqueue(a, 0, Millis(0));
        let Action::Show { alert } = &acts[0] else { panic!("no show") };
        assert_eq!((&alert.sound, &alert.tts_text, &alert.tts_audio), (&None, &None, &None));
        assert_eq!(q.playing().unwrap().alert.sound, None);
    }

    #[test]
    fn unmuting_lets_the_next_alert_have_its_sound() {
        let mut q = q();
        q.set_muted(true);
        q.set_muted(false);
        let mut a = alert("a", 1_000);
        a.sound = Some("builtin:chime".into());
        let acts = q.enqueue(a, 0, Millis(0));
        let Action::Show { alert } = &acts[0] else { panic!("no show") };
        assert_eq!(alert.sound.as_deref(), Some("builtin:chime"));
    }

    #[test]
    fn pause_and_mute_survive_a_round_trip_and_old_saves_load() {
        let mut q = q();
        q.pause();
        q.set_muted(true);
        let back: AlertQueue = serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
        assert!(back.is_paused() && back.is_muted());
        // A queue saved before mute existed has no `muted` key.
        let mut v = serde_json::to_value(&q).unwrap();
        v.as_object_mut().unwrap().remove("muted");
        let old: AlertQueue = serde_json::from_value(v).unwrap();
        assert!(old.is_paused() && !old.is_muted());
    }

    #[test]
    fn an_interrupting_alert_is_never_folded_by_the_cap() {
        let mut q =
            AlertQueue::new(QueueTuning { ack_grace_ms: 1_000, gap_ms: 100, stale_after_ms: 0, max_waiting: 2 });
        q.enqueue(alert("p", 5_000), 0, Millis(0));
        q.enqueue(alert("w1", 5_000), 5, Millis(1));
        q.enqueue(alert("w2", 5_000), 5, Millis(2));
        // Full, and the interrupting alert has the lowest priority of all.
        let acts = q.enqueue_with(alert("i", 5_000), -1, true, Millis(10));
        assert_eq!(shown(&acts), vec!["i"]);
        assert_eq!(q.playing().unwrap().alert.alert_id, "i");
        assert!(q.waiting().count() <= 2);
    }

    #[test]
    fn a_cap_of_one_with_only_the_kept_alert_cannot_loop() {
        let mut q =
            AlertQueue::new(QueueTuning { ack_grace_ms: 1_000, gap_ms: 100, stale_after_ms: 0, max_waiting: 1 });
        q.enqueue(alert("p", 5_000), 0, Millis(0));
        let acts = q.enqueue_with(alert("i", 5_000), 0, true, Millis(10));
        assert_eq!(shown(&acts), vec!["i"]);
    }

    /// A muted alert with an alert design still carried the design's own sound and impact hits.
    #[test]
    fn mute_also_silences_the_designs_sound_and_impact_hits() {
        use crate::design::{BuiltinSound, DesignContent, EventKey, Sound, pick, render_onto, tests::alert_design};
        let e = any_sample("tip", Millis(0)).unwrap();
        let rules = default_rules();
        let (rule, var) = select(&rules, &e).unwrap();
        let mut a = render("d1", rule, var, &e);
        let mut d = alert_design("dz");
        if let DesignContent::Alert(c) = &mut d.content {
            let l = &mut c.events.get_mut(&EventKey::Tip).unwrap().variations[0].look;
            l.sound = Sound { media_ref: Some("01hzsound".into()), builtin: BuiltinSound::Coin, volume: 55 };
            l.motion.impact_sounds = true;
        }
        let (key, v) = pick(&d, &e).unwrap();
        render_onto(&mut a, &d, key, v, &e);
        assert!(a.design.as_ref().unwrap().look.motion.impact_sounds, "set up with sound");
        let mut q = q();
        q.set_muted(true);
        let acts = q.enqueue(a, 0, Millis(0));
        let Action::Show { alert } = &acts[0] else { panic!("no show") };
        let look = &alert.design.as_ref().unwrap().look;
        assert_eq!((look.sound.media_ref.as_deref(), look.sound.volume, look.motion.impact_sounds), (None, 0, false));
        assert_eq!(
            q.playing().unwrap().alert.design.as_ref().unwrap().look.sound.volume,
            0,
            "what is kept is silent too"
        );
    }
}
