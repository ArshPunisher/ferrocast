//! Bounded serial queue and dispatcher for reading live chat messages aloud.
//!
//! Abuse filtering and text cleanup run synchronously in [`ferrocast_core::tts::decide_chat`]
//! before items reach this module. This module enforces per-user cooldowns, queue depth caps,
//! global pacing between spoken lines, moderation cancellation (Panic pause, queue mute,
//! message deletion, user ban/clear/timeout), local-only synthesis (to protect Pro Cloud TTS
//! quotas), and ephemeral media cleanup.

use crate::frames::{Audience, OverlayFrame};
use crate::{Engine, now, platform_key};
use ferrocast_core::event::{Event, EventKind, Platform};
use ferrocast_core::tts::{Decision, decide_chat};
use ferrocast_store::repo;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Notify;

const MAX_TRACKED_USERS: usize = 2_000;
const RECENT_MEDIA_KEEP: usize = 24;
const MAX_DISPATCHED_KEEP: usize = 24;
const DISPATCHED_TTL_MS: i64 = 120_000;

/// One approved chat message waiting to be spoken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatTtsItem {
    pub id: String,
    pub event_id: String,
    pub source_id: Option<String>,
    pub platform: Platform,
    pub actor_id: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone)]
struct ActiveChatTts {
    generation: u64,
    is_auto: bool,
    id: String,
    event_id: String,
    source_id: Option<String>,
    platform: Platform,
    actor_id: Option<String>,
    cancelled: bool,
}

#[derive(Debug, Clone)]
struct DispatchedChatTts {
    id: String,
    event_id: String,
    source_id: Option<String>,
    platform: Platform,
    actor_id: Option<String>,
    dispatched_at_ms: i64,
}

/// Result of attempting to add a chat line to the in-memory queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueOutcome {
    Queued,
    QueueFull,
    Cooldown,
}

/// Result of polling [`ChatTtsState`] for the next ready utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadyState {
    Empty,
    Wait(Duration),
    Ready { item: ChatTtsItem, generation: u64 },
}

/// Result of completing an active synthesis attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinishOutcome {
    pub cancelled: bool,
    pub retired_media_id: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct ChatTtsState {
    queue: VecDeque<ChatTtsItem>,
    per_user_last: HashMap<String, i64>,
    last_spoken_at: Option<i64>,
    last_spoken_duration_ms: u64,
    in_flight: Vec<ActiveChatTts>,
    recent_dispatched: VecDeque<DispatchedChatTts>,
    recent_media: VecDeque<String>,
    next_generation: u64,
}

impl ChatTtsState {
    pub(crate) fn try_enqueue(
        &mut self,
        item: ChatTtsItem,
        user_key: String,
        queue_limit: usize,
        cooldown_s: u32,
        now_ms: i64,
    ) -> EnqueueOutcome {
        let limit = queue_limit.max(1);
        if self.queue.len() >= limit {
            return EnqueueOutcome::QueueFull;
        }
        if !self.check_and_mark_user_cooldown(user_key, cooldown_s, now_ms) {
            return EnqueueOutcome::Cooldown;
        }
        self.queue.push_back(item);
        EnqueueOutcome::Queued
    }

    pub(crate) fn check_and_mark_user_cooldown(&mut self, user_key: String, cooldown_s: u32, now_ms: i64) -> bool {
        let cd_ms = i64::from(cooldown_s).saturating_mul(1_000);
        if cd_ms > 0 && self.per_user_last.get(&user_key).is_some_and(|&prev| now_ms.saturating_sub(prev) < cd_ms) {
            return false;
        }
        self.per_user_last.insert(user_key, now_ms);
        if self.per_user_last.len() > MAX_TRACKED_USERS
            && let Some(oldest_key) = self.per_user_last.iter().min_by_key(|(_, ts)| *ts).map(|(k, _)| k.clone())
        {
            self.per_user_last.remove(&oldest_key);
        }
        true
    }

    pub(crate) fn take_ready(&mut self, now_ms: i64, global_cooldown_s: u32) -> ReadyState {
        if self.in_flight.iter().any(|a| a.is_auto) || self.queue.is_empty() {
            return ReadyState::Empty;
        }
        let gap_ms = i64::from(global_cooldown_s)
            .saturating_mul(1_000)
            .max(i64::try_from(self.last_spoken_duration_ms).unwrap_or(0));
        if gap_ms > 0
            && let Some(last) = self.last_spoken_at
        {
            let elapsed = now_ms.saturating_sub(last);
            if elapsed < gap_ms {
                let wait_ms = u64::try_from(gap_ms - elapsed).unwrap_or(0);
                return ReadyState::Wait(Duration::from_millis(wait_ms));
            }
        }
        let Some(item) = self.queue.pop_front() else {
            return ReadyState::Empty;
        };
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.in_flight.push(ActiveChatTts {
            generation,
            is_auto: true,
            id: item.id.clone(),
            event_id: item.event_id.clone(),
            source_id: item.source_id.clone(),
            platform: item.platform,
            actor_id: item.actor_id.clone(),
            cancelled: false,
        });
        ReadyState::Ready { item, generation }
    }

    pub(crate) fn start_approved(&mut self, item: &ChatTtsItem) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.in_flight.push(ActiveChatTts {
            generation,
            is_auto: false,
            id: item.id.clone(),
            event_id: item.event_id.clone(),
            source_id: item.source_id.clone(),
            platform: item.platform,
            actor_id: item.actor_id.clone(),
            cancelled: false,
        });
        generation
    }

    #[cfg(test)]
    pub(crate) fn finish_active(
        &mut self,
        generation: u64,
        media_id: Option<String>,
        commit: bool,
        now_ms: i64,
    ) -> FinishOutcome {
        self.finish_active_with_duration(generation, media_id, commit, now_ms, 0)
    }

    pub(crate) fn finish_active_with_duration(
        &mut self,
        generation: u64,
        media_id: Option<String>,
        commit: bool,
        now_ms: i64,
        duration_ms: u64,
    ) -> FinishOutcome {
        let Some(idx) = self.in_flight.iter().position(|a| a.generation == generation) else {
            return FinishOutcome { cancelled: true, retired_media_id: None };
        };
        let active = self.in_flight.remove(idx);
        if !commit || active.cancelled {
            return FinishOutcome { cancelled: true, retired_media_id: None };
        }
        if active.is_auto {
            self.last_spoken_at = Some(now_ms);
            self.last_spoken_duration_ms = duration_ms;
        }
        self.prune_dispatched(now_ms);
        self.recent_dispatched.push_back(DispatchedChatTts {
            id: active.id,
            event_id: active.event_id,
            source_id: active.source_id,
            platform: active.platform,
            actor_id: active.actor_id,
            dispatched_at_ms: now_ms,
        });
        while self.recent_dispatched.len() > MAX_DISPATCHED_KEEP {
            self.recent_dispatched.pop_front();
        }
        let retired_media_id = media_id.and_then(|id| self.remember_media(id));
        FinishOutcome { cancelled: false, retired_media_id }
    }

    fn prune_dispatched(&mut self, now_ms: i64) {
        while self
            .recent_dispatched
            .front()
            .is_some_and(|d| now_ms.saturating_sub(d.dispatched_at_ms) > DISPATCHED_TTL_MS)
        {
            self.recent_dispatched.pop_front();
        }
    }

    pub(crate) fn remember_media(&mut self, media_id: String) -> Option<String> {
        self.recent_media.push_back(media_id);
        if self.recent_media.len() > RECENT_MEDIA_KEEP { self.recent_media.pop_front() } else { None }
    }

    pub(crate) fn purge_message_stop(
        &mut self,
        platform: Platform,
        message_id: &str,
        event_id: Option<&str>,
        now_ms: i64,
    ) -> Option<Vec<String>> {
        let matches = |ev_id: &str, src_id: Option<&str>, plat: Platform| {
            plat == platform
                && (src_id == Some(message_id) || ev_id == message_id || event_id.is_some_and(|e| ev_id == e))
        };
        self.queue.retain(|item| !matches(&item.event_id, item.source_id.as_deref(), item.platform));
        let mut stop_ids = Vec::new();
        let mut stopped = false;
        for active in &mut self.in_flight {
            if matches(&active.event_id, active.source_id.as_deref(), active.platform) && !active.cancelled {
                active.cancelled = true;
                stop_ids.push(active.id.clone());
                stopped = true;
            }
        }
        self.prune_dispatched(now_ms);
        self.recent_dispatched.retain(|d| {
            if matches(&d.event_id, d.source_id.as_deref(), d.platform) {
                stop_ids.push(d.id.clone());
                stopped = true;
                false
            } else {
                true
            }
        });
        stopped.then_some(stop_ids)
    }

    pub(crate) fn purge_user_stop(&mut self, platform: Platform, actor_id: &str, now_ms: i64) -> Option<Vec<String>> {
        if actor_id.is_empty() {
            return None;
        }
        let matches = |aid: Option<&str>, plat: Platform| plat == platform && aid == Some(actor_id);
        self.queue.retain(|item| !matches(item.actor_id.as_deref(), item.platform));
        let mut stop_ids = Vec::new();
        let mut stopped = false;
        for active in &mut self.in_flight {
            if matches(active.actor_id.as_deref(), active.platform) && !active.cancelled {
                active.cancelled = true;
                stop_ids.push(active.id.clone());
                stopped = true;
            }
        }
        self.prune_dispatched(now_ms);
        self.recent_dispatched.retain(|d| {
            if matches(d.actor_id.as_deref(), d.platform) {
                stop_ids.push(d.id.clone());
                stopped = true;
                false
            } else {
                true
            }
        });
        stopped.then_some(stop_ids)
    }

    #[cfg(test)]
    pub(crate) fn purge_message(&mut self, platform: Platform, message_id: &str, event_id: Option<&str>) -> bool {
        self.purge_message_stop(platform, message_id, event_id, i64::MIN / 4).is_some()
    }

    #[cfg(test)]
    pub(crate) fn purge_user(&mut self, platform: Platform, actor_id: &str) -> bool {
        self.purge_user_stop(platform, actor_id, i64::MIN / 4).is_some()
    }

    pub(crate) fn stop_all(&mut self) -> usize {
        let dropped = self.queue.len();
        self.queue.clear();
        for active in &mut self.in_flight {
            active.cancelled = true;
        }
        self.recent_dispatched.clear();
        self.last_spoken_duration_ms = 0;
        dropped
    }

    pub(crate) fn media_ids(&self) -> Vec<String> {
        self.recent_media.iter().cloned().collect()
    }
}

/// Shared synchronization slot inside `Inner`.
#[derive(Debug, Default)]
pub(crate) struct ChatTtsSlot {
    state: Mutex<ChatTtsState>,
    wake: Notify,
}

impl ChatTtsSlot {
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, ChatTtsState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Engine {
    /// Evaluates a moderated, visible live chat message for Chat TTS and either enqueues it
    /// or places it into the approval list (when `hold_for_approval` is on).
    pub(crate) async fn maybe_enqueue_chat_tts(&self, e: &Event) {
        let EventKind::ChatMessage { actor, .. } = &e.kind else {
            return;
        };
        let _op = self.inner.tts_op.lock().await;
        if self.inner.queue.lock().await.is_muted() {
            return;
        }
        let (decision, cfg_chat, paused_tts) = {
            let st = self.state();
            (decide_chat(&st.tts, &st.filter, e), st.tts.chat.clone(), st.tts_paused)
        };
        if paused_tts {
            return;
        }
        let at_ms = now().0;
        let user_key = format!(
            "{}:{}",
            platform_key(e.platform),
            actor
                .id
                .as_deref()
                .filter(|s| !s.is_empty())
                .or_else(|| actor.login.as_deref().filter(|s| !s.is_empty()))
                .unwrap_or(&actor.display_name)
        );
        match decision {
            Decision::Skip { .. } => {}
            Decision::Hold { text } => {
                let pending_count = self.db(repo::events::pending_chat_tts_count).await.unwrap_or(0);
                if pending_count >= cfg_chat.queue_limit.max(1) {
                    return;
                }
                // Revalidate live policy under tts_op + queue lock right before committing Hold insert.
                if self.inner.queue.lock().await.is_muted() {
                    return;
                }
                let still_ok = {
                    let st = self.state();
                    !st.tts_paused && st.tts.enabled && st.tts.chat.enabled && st.tts.hold_for_approval
                };
                if !still_ok {
                    return;
                }
                let allowed =
                    self.inner.chat_tts.lock().check_and_mark_user_cooldown(user_key, cfg_chat.cooldown_s, at_ms);
                if !allowed {
                    return;
                }
                let p = repo::events::PendingTts {
                    id: ferrocast_store::new_id(),
                    event_id: e.id.clone(),
                    text,
                    created_at: at_ms,
                    platform: Some(platform_key(e.platform).to_string()),
                    name: Some(actor.display_name.clone()),
                    amount: None,
                };
                if self.db(move |s| repo::events::add_pending_tts(s, &p)).await.is_ok() {
                    self.push_tts_pending().await;
                }
            }
            Decision::Speak { text } => {
                let item = ChatTtsItem {
                    id: format!("chat-tts-{}", ferrocast_store::new_id()),
                    event_id: e.id.clone(),
                    source_id: e.source_id.clone(),
                    platform: e.platform,
                    actor_id: actor.id.clone(),
                    text,
                };
                let outcome = self.inner.chat_tts.lock().try_enqueue(
                    item,
                    user_key,
                    cfg_chat.queue_limit,
                    cfg_chat.cooldown_s,
                    at_ms,
                );
                if outcome == EnqueueOutcome::Queued {
                    self.inner.chat_tts.wake.notify_one();
                }
            }
        }
    }

    pub(crate) fn start_approved_chat_tts(&self, item: &ChatTtsItem) -> u64 {
        self.inner.chat_tts.lock().start_approved(item)
    }

    pub(crate) async fn publish_chat_tts_after_synth(
        &self,
        generation: u64,
        item: &ChatTtsItem,
        media_id: Option<String>,
        duration_ms: u64,
    ) {
        let finish = {
            let q = self.inner.queue.lock().await;
            let st = self.state();
            let still_allowed = !q.is_muted() && !st.tts_paused && st.tts.enabled && st.tts.chat.enabled;
            let mut guard = self.inner.chat_tts.lock();
            let finish = guard.finish_active_with_duration(
                generation,
                if still_allowed { media_id.clone() } else { None },
                still_allowed,
                now().0,
                duration_ms,
            );
            if !finish.cancelled && still_allowed {
                crate::studio::voice_commands::reading_aloud(&item.text, now().0);
                self.send_overlay(OverlayFrame::new(
                    Audience::Alerts,
                    self.next_seq(),
                    "tts",
                    &json!({ "id": item.id, "text": item.text, "audio": media_id }),
                ));
            }
            finish
        };
        if let Some(retired) = finish.retired_media_id {
            let _ = self.db(move |s| repo::media::delete(s, &retired)).await;
        }
        if finish.cancelled
            && let Some(unused_id) = media_id
        {
            let _ = self.db(move |s| repo::media::delete(s, &unused_id)).await;
        }
    }

    /// Clears all waiting chat TTS lines, cancels any in-flight utterance, and tells alert
    /// overlays to stop speech right away (`tts_stop`).
    pub(crate) fn stop_chat_tts(&self) {
        self.inner.chat_tts.lock().stop_all();
        self.inner.chat_tts.wake.notify_one();
        self.emit_tts_stop(None);
    }

    pub(crate) fn purge_chat_tts_message(&self, platform: Platform, message_id: &str, event_id: Option<&str>) {
        if let Some(ids) = self.inner.chat_tts.lock().purge_message_stop(platform, message_id, event_id, now().0) {
            self.emit_tts_stop(Some(ids));
        }
    }

    pub(crate) fn purge_chat_tts_user(&self, platform: Platform, actor_id: &str) {
        if let Some(ids) = self.inner.chat_tts.lock().purge_user_stop(platform, actor_id, now().0) {
            self.emit_tts_stop(Some(ids));
        }
    }

    pub(crate) fn chat_tts_media_ids(&self) -> Vec<String> {
        self.inner.chat_tts.lock().media_ids()
    }

    pub(crate) fn wake_chat_tts(&self) {
        self.inner.chat_tts.wake.notify_one();
    }

    fn emit_tts_stop(&self, ids: Option<Vec<String>>) {
        let payload = match ids {
            Some(ids) if !ids.is_empty() => json!({ "ids": ids }),
            _ => json!({}),
        };
        self.send_overlay(OverlayFrame::new(Audience::Alerts, self.next_seq(), "tts_stop", &payload));
    }
}

/// Serial background worker that paces and synthesizes queued Chat TTS messages one by one.
pub(crate) async fn run(engine: Engine) {
    loop {
        let (global_cd, paused, enabled) = {
            let st = engine.state();
            (st.tts.chat.global_cooldown_s, st.tts_paused, st.tts.enabled && st.tts.chat.enabled)
        };
        if paused || !enabled || engine.inner.queue.lock().await.is_muted() {
            engine.inner.chat_tts.wake.notified().await;
            continue;
        }
        let ready = engine.inner.chat_tts.lock().take_ready(now().0, global_cd);
        match ready {
            ReadyState::Empty => {
                engine.inner.chat_tts.wake.notified().await;
            }
            ReadyState::Wait(delay) => {
                tokio::select! {
                    _ = engine.inner.chat_tts.wake.notified() => {}
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            ReadyState::Ready { item, generation } => {
                // Local system voice only so live chat never spends Pro Cloud TTS characters.
                let (media_id, duration_ms) = engine.speak_local_to_media(&item.text).await;
                engine.publish_chat_tts_after_synth(generation, &item, media_id, duration_ms).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::voice_commands::{Command, from_a_viewer};
    use ferrocast_core::event::{Actor, Content, Millis, Origin, Roles};
    use ferrocast_core::tts::ChatTtsMode;

    fn item(id: &str, uid: &str, src: &str, text: &str) -> ChatTtsItem {
        ChatTtsItem {
            id: id.into(),
            event_id: format!("ev-{id}"),
            source_id: Some(src.into()),
            platform: Platform::Twitch,
            actor_id: Some(uid.into()),
            text: text.into(),
        }
    }

    fn chat_event(source: &str, uid: &str, roles: Roles, origin: Origin, channel: Option<&str>, text: &str) -> Event {
        let mut e = Event::live(
            Platform::Twitch,
            Some(source.into()),
            Millis(100),
            EventKind::ChatMessage {
                actor: Actor { id: Some(uid.into()), roles, ..Actor::named(Platform::Twitch, uid) },
                content: Content::plain(text),
                reply_to: None,
                is_action: false,
            },
        );
        e.origin = origin;
        e.channel = channel.map(|s| ferrocast_core::event::ChannelRef {
            platform: Platform::Twitch,
            id: s.to_string(),
            name: s.to_string(),
        });
        e
    }

    #[test]
    fn queue_enforces_per_user_cooldown_cap_dropping_newest_and_prunes_user_table() {
        let mut st = ChatTtsState::default();
        assert_eq!(
            st.try_enqueue(item("1", "u1", "s1", "one"), "twitch:u1".into(), 2, 15, 10_000),
            EnqueueOutcome::Queued
        );
        assert_eq!(
            st.try_enqueue(item("2", "u1", "s2", "two"), "twitch:u1".into(), 2, 15, 20_000),
            EnqueueOutcome::Cooldown
        );
        assert_eq!(
            st.try_enqueue(item("3", "u2", "s3", "three"), "twitch:u2".into(), 2, 15, 20_500),
            EnqueueOutcome::Queued
        );
        // Queue limit is 2: newest is dropped and does not burn u3's cooldown.
        assert_eq!(
            st.try_enqueue(item("4", "u3", "s4", "four"), "twitch:u3".into(), 2, 15, 21_000),
            EnqueueOutcome::QueueFull
        );
        // After u1's 15s cooldown passes and one item drains, u1 & u3 can both enqueue.
        assert!(matches!(st.take_ready(25_000, 0), ReadyState::Ready { .. }));
        assert_eq!(
            st.try_enqueue(item("5", "u3", "s5", "five"), "twitch:u3".into(), 2, 15, 25_000),
            EnqueueOutcome::Queued
        );
        assert_eq!(st.queue.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["3", "5"]);

        // Per-user cooldown table is bounded to 2,000 entries.
        for i in 0..2_050 {
            st.check_and_mark_user_cooldown(format!("twitch:bulk-{i}"), 0, 100_000 + i64::from(i));
        }
        assert!(st.per_user_last.len() <= MAX_TRACKED_USERS);
    }

    #[test]
    fn global_cooldown_spaces_utterances_and_recent_media_ring_retires_old_ids() {
        let mut st = ChatTtsState::default();
        st.try_enqueue(item("1", "u1", "s1", "one"), "twitch:u1".into(), 5, 0, 1_000);
        st.try_enqueue(item("2", "u2", "s2", "two"), "twitch:u2".into(), 5, 0, 1_000);

        let ReadyState::Ready { generation: g1, .. } = st.take_ready(1_000, 3) else {
            panic!("first item should be ready immediately");
        };
        let f1 = st.finish_active(g1, Some("m1".into()), true, 1_200);
        assert!(!f1.cancelled);

        assert_eq!(st.take_ready(2_500, 3), ReadyState::Wait(Duration::from_millis(1_700)));
        assert!(matches!(st.take_ready(4_200, 3), ReadyState::Ready { .. }));

        for i in 2..=(RECENT_MEDIA_KEEP + 2) {
            let retired = st.remember_media(format!("m{i}"));
            if i <= RECENT_MEDIA_KEEP {
                assert!(retired.is_none());
            } else {
                assert!(retired.is_some());
            }
        }
        assert_eq!(st.media_ids().len(), RECENT_MEDIA_KEEP);
    }

    #[test]
    fn purge_and_stop_all_cancel_in_flight_and_dispatched_items() {
        let mut st = ChatTtsState::default();
        st.try_enqueue(item("1", "u1", "src-1", "hi"), "twitch:u1".into(), 5, 0, 1_000);
        st.try_enqueue(item("2", "u1", "src-2", "bye"), "twitch:u1".into(), 5, 0, 2_000);
        st.try_enqueue(item("3", "u2", "src-3", "other"), "twitch:u2".into(), 5, 0, 3_000);

        // Purging by message removes queued item 2 without stopping queued item 1/3.
        assert!(!st.purge_message(Platform::Twitch, "src-2", None));
        assert_eq!(st.queue.len(), 2);

        // Make item 1 in-flight, then ban u1 while item 1 is synthesizing.
        let ReadyState::Ready { generation: g1, .. } = st.take_ready(4_000, 0) else {
            panic!("expected ready item");
        };
        assert!(st.purge_user(Platform::Twitch, "u1"), "in-flight utterance for banned user asks for tts_stop");
        let finish = st.finish_active(g1, Some("orphan-media".into()), true, 4_100);
        assert!(finish.cancelled);
        assert!(finish.retired_media_id.is_none());

        // Make item 3 finish dispatch, then delete src-3 -> emits tts_stop.
        let ReadyState::Ready { generation: g3, .. } = st.take_ready(5_000, 0) else {
            panic!("expected ready item 3");
        };
        let f3 = st.finish_active(g3, Some("m3".into()), true, 5_100);
        assert!(!f3.cancelled);
        assert!(st.purge_message(Platform::Twitch, "src-3", None));
        assert!(!st.purge_message(Platform::Twitch, "src-3", None), "idempotent once stopped");

        // Re-enqueue and verify stop_all drops everything.
        st.try_enqueue(item("4", "u2", "src-4", "stay"), "twitch:u2".into(), 5, 0, 6_000);
        assert_eq!(st.stop_all(), 1);
        assert_eq!(st.take_ready(7_000, 0), ReadyState::Empty);
    }

    async fn test_engine() -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let secrets: std::sync::Arc<dyn ferrocast_secrets::Secrets> =
            std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default());
        let engine = Engine::start_with_secrets(dir.path(), secrets).await.unwrap();
        let mut tts = engine.state().tts.clone();
        tts.enabled = true;
        tts.voice = Some("missing-local-voice-falls-back-fast".into());
        tts.chat.enabled = true;
        tts.chat.mode = ChatTtsMode::Everyone;
        tts.chat.cooldown_s = 0;
        tts.chat.global_cooldown_s = 0;
        engine.save_tts(tts).await.unwrap();
        (engine, dir)
    }

    async fn next_overlay_of(rx: &mut tokio::sync::broadcast::Receiver<OverlayFrame>, kind: &str) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let f = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for overlay frame `{kind}`"))
                .expect("overlay channel closed");
            let v: serde_json::Value = serde_json::from_str(&f.text).unwrap();
            if v["type"] == kind {
                return v;
            }
        }
    }

    #[tokio::test]
    async fn engine_ignores_backfill_shared_chat_respects_voice_echo_guard_and_stops_on_panic_and_mute() {
        let (engine, _dir) = test_engine().await;
        let mut rx = engine.subscribe_overlays();

        // Backfill & Shared Chat messages must never enqueue.
        engine.ingest(chat_event("s1", "u1", Roles::empty(), Origin::Backfill, None, "old message")).await;
        engine
            .ingest(chat_event("s2", "u1", Roles::empty(), Origin::Live, Some("partner-room"), "shared message"))
            .await;
        assert!(engine.inner.chat_tts.lock().queue.is_empty());

        // Live message containing a scene-switch phrase marks voice_commands::reading_aloud.
        engine
            .ingest(chat_event("s3", "u1", Roles::empty(), Origin::Live, None, "switch to chatechoscene right now"))
            .await;
        let frame = next_overlay_of(&mut rx, "tts").await;
        assert_eq!(frame["data"]["text"], "u1 says: switch to chatechoscene right now");
        assert!(
            from_a_viewer(&Command::Scene("ChatEchoScene".into()), &["Main".into(), "ChatEchoScene".into()], now().0),
            "reading_aloud must block echoed voice commands from chat TTS"
        );

        // Panic mode with pause_tts clears queued lines and emits tts_stop.
        let mut panic_cfg = engine.panic_state().1;
        panic_cfg.pause_tts = true;
        engine.save_panic(panic_cfg).await.unwrap();
        engine.panic(true).await.unwrap();
        assert!(engine.state().tts_paused);
        let stop_frame = next_overlay_of(&mut rx, "tts_stop").await;
        assert_eq!(stop_frame["type"], "tts_stop");

        // While paused, incoming chat does not enqueue.
        engine.ingest(chat_event("s4", "u2", Roles::empty(), Origin::Live, None, "while paused")).await;
        assert!(engine.inner.chat_tts.lock().queue.is_empty());
        engine.panic(false).await.unwrap();

        // A live line after resuming plays, and deleting it triggers tts_stop.
        engine.ingest(chat_event("s3b", "u1", Roles::empty(), Origin::Live, None, "delete me soon")).await;
        let active_frame = next_overlay_of(&mut rx, "tts").await;
        assert_eq!(active_frame["data"]["text"], "u1 says: delete me soon");
        engine
            .ingest(Event::live(
                Platform::Twitch,
                None,
                Millis(150),
                EventKind::MessageDeleted { message_id: "s3b".into() },
            ))
            .await;
        let stop_del = next_overlay_of(&mut rx, "tts_stop").await;
        assert_eq!(stop_del["type"], "tts_stop");

        // Alert queue mute also blocks & emits tts_stop.
        engine.queue_set_muted(true).await;
        let stop_mute = next_overlay_of(&mut rx, "tts_stop").await;
        assert_eq!(stop_mute["type"], "tts_stop");
        engine.ingest(chat_event("s5", "u2", Roles::empty(), Origin::Live, None, "while muted")).await;
        assert!(engine.inner.chat_tts.lock().queue.is_empty());

        // Disabling chat TTS via save_tts also emits tts_stop.
        engine.queue_set_muted(false).await;
        let mut disabled = engine.state().tts.clone();
        disabled.chat.enabled = false;
        engine.save_tts(disabled).await.unwrap();
        let stop_off = next_overlay_of(&mut rx, "tts_stop").await;
        assert_eq!(stop_off["type"], "tts_stop");
    }

    #[tokio::test]
    async fn engine_hold_for_approval_caps_pending_chat_items_and_delete_ban_purge_them() {
        let (engine, _dir) = test_engine().await;
        let mut tts = engine.state().tts.clone();
        tts.hold_for_approval = true;
        tts.chat.queue_limit = 2;
        engine.save_tts(tts).await.unwrap();

        for i in 1..=3 {
            engine
                .ingest(chat_event(
                    &format!("src-{i}"),
                    &format!("u{i}"),
                    Roles::empty(),
                    Origin::Live,
                    None,
                    &format!("pending line {i}"),
                ))
                .await;
        }
        assert_eq!(engine.tts_pending().await.unwrap().len(), 2);

        // MessageDeleted drops matching pending item.
        engine
            .ingest(Event::live(
                Platform::Twitch,
                None,
                Millis(200),
                EventKind::MessageDeleted { message_id: "src-1".into() },
            ))
            .await;
        assert_eq!(engine.tts_pending().await.unwrap().len(), 1);

        // Ban drops matching user's pending item.
        engine
            .ingest(Event::live(
                Platform::Twitch,
                None,
                Millis(201),
                EventKind::Ban {
                    target: Actor { id: Some("u2".into()), ..Actor::named(Platform::Twitch, "u2") },
                    reason: None,
                    until: None,
                },
            ))
            .await;
        assert!(engine.tts_pending().await.unwrap().is_empty());

        // Approving a held chat item speaks via local voice and emits tts frame.
        engine.ingest(chat_event("src-approve", "u9", Roles::empty(), Origin::Live, None, "approve me")).await;
        let pending = engine.tts_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        let pid = pending[0]["id"].as_str().unwrap().to_string();
        let mut rx = engine.subscribe_overlays();
        engine.decide_tts(&pid, true).await.unwrap();
        let frame = next_overlay_of(&mut rx, "tts").await;
        assert_eq!(frame["data"]["id"], pid);
        assert_eq!(frame["data"]["text"], "u9 says: approve me");
        assert!(engine.tts_pending().await.unwrap().is_empty());

        // Turn off hold_for_approval so subsequent live chat speaks immediately, then test Ban -> tts_stop.
        tts_disable_hold(&engine).await;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                None,
                Millis(202),
                EventKind::Ban {
                    target: Actor { id: Some("u9".into()), ..Actor::named(Platform::Twitch, "u9") },
                    reason: None,
                    until: None,
                },
            ))
            .await;
        engine.ingest(chat_event("src-u10", "u10", Roles::empty(), Origin::Live, None, "ban target line")).await;
        let _spoken = next_overlay_of(&mut rx, "tts").await;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                None,
                Millis(203),
                EventKind::Ban {
                    target: Actor { id: Some("u10".into()), ..Actor::named(Platform::Twitch, "u10") },
                    reason: None,
                    until: None,
                },
            ))
            .await;
        let stop = next_overlay_of(&mut rx, "tts_stop").await;
        assert_eq!(stop["type"], "tts_stop");
    }

    async fn tts_disable_hold(engine: &Engine) {
        let mut tts = engine.state().tts.clone();
        tts.hold_for_approval = false;
        engine.save_tts(tts).await.unwrap();
    }

    #[test]
    fn recent_dispatched_ring_tracks_earlier_speaking_messages_and_media_ring_retains_queue() {
        let mut st = ChatTtsState::default();
        const { assert!(RECENT_MEDIA_KEEP >= 24, "media retention ring must outlast max queued overlay playback") };

        st.try_enqueue(item("1", "u1", "src-1", "long abusive message"), "twitch:u1".into(), 5, 0, 1_000);
        let ReadyState::Ready { generation: g1, .. } = st.take_ready(1_000, 3) else {
            panic!("first ready");
        };
        assert!(!st.finish_active(g1, Some("m1".into()), true, 1_200).cancelled);

        // 4 s later, a second message is synthesized and dispatched while msg 1 is still reading aloud.
        st.try_enqueue(item("2", "u2", "src-2", "gg"), "twitch:u2".into(), 5, 0, 5_000);
        let ReadyState::Ready { generation: g2, .. } = st.take_ready(5_000, 3) else {
            panic!("second ready");
        };
        assert!(!st.finish_active(g2, Some("m2".into()), true, 5_200).cancelled);

        // Deleting msg 1 (or banning u1) must still match msg 1 and emit stop.
        assert!(st.purge_message(Platform::Twitch, "src-1", None), "earlier active msg 1 must still purge");
    }

    #[tokio::test]
    async fn approved_chat_honors_playback_controls_and_preserves_pending_item() {
        let (engine, _dir) = test_engine().await;
        let mut tts = engine.state().tts.clone();
        tts.hold_for_approval = true;
        engine.save_tts(tts).await.unwrap();

        engine.ingest(chat_event("src-hold-ctrl", "u-ctrl", Roles::empty(), Origin::Live, None, "held line")).await;
        let pending = engine.tts_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        let pid = pending[0]["id"].as_str().unwrap().to_string();

        // While alert queue is muted, approving a held chat line must fail without consuming it or emitting tts.
        let mut rx = engine.subscribe_overlays();
        engine.queue_set_muted(true).await;
        let _ = next_overlay_of(&mut rx, "tts_stop").await;
        assert!(engine.decide_tts(&pid, true).await.is_err(), "muted queue must refuse chat approval");
        assert_eq!(engine.tts_pending().await.unwrap().len(), 1, "refused approval must leave pending item");
        assert!(rx.try_recv().is_err(), "refused approval must not emit tts");

        // Same when Panic pauses TTS.
        engine.queue_set_muted(false).await;
        let mut panic_cfg = engine.panic_state().1;
        panic_cfg.pause_tts = true;
        engine.save_panic(panic_cfg).await.unwrap();
        engine.panic(true).await.unwrap();
        let _ = next_overlay_of(&mut rx, "tts_stop").await;
        assert!(engine.decide_tts(&pid, true).await.is_err(), "paused TTS must refuse chat approval");
        assert_eq!(engine.tts_pending().await.unwrap().len(), 1);
    }
}
