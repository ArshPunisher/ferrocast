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
const RECENT_MEDIA_KEEP: usize = 10;

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
    event_id: String,
    source_id: Option<String>,
    platform: Platform,
    actor_id: Option<String>,
    cancelled: bool,
}

#[derive(Debug, Clone)]
struct DispatchedChatTts {
    event_id: String,
    source_id: Option<String>,
    platform: Platform,
    actor_id: Option<String>,
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
    active: Option<ActiveChatTts>,
    last_dispatched: Option<DispatchedChatTts>,
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
        if self.active.is_some() || self.queue.is_empty() {
            return ReadyState::Empty;
        }
        let gap_ms = i64::from(global_cooldown_s).saturating_mul(1_000);
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
        self.active = Some(ActiveChatTts {
            generation,
            event_id: item.event_id.clone(),
            source_id: item.source_id.clone(),
            platform: item.platform,
            actor_id: item.actor_id.clone(),
            cancelled: false,
        });
        ReadyState::Ready { item, generation }
    }

    pub(crate) fn finish_active(
        &mut self,
        generation: u64,
        media_id: Option<String>,
        commit: bool,
        now_ms: i64,
    ) -> FinishOutcome {
        let Some(active) = self.active.take() else {
            return FinishOutcome { cancelled: true, retired_media_id: None };
        };
        if !commit || active.generation != generation || active.cancelled {
            return FinishOutcome { cancelled: true, retired_media_id: None };
        }
        self.last_spoken_at = Some(now_ms);
        self.last_dispatched = Some(DispatchedChatTts {
            event_id: active.event_id,
            source_id: active.source_id,
            platform: active.platform,
            actor_id: active.actor_id,
        });
        let retired_media_id = media_id.and_then(|id| self.remember_media(id));
        FinishOutcome { cancelled: false, retired_media_id }
    }

    pub(crate) fn remember_media(&mut self, media_id: String) -> Option<String> {
        self.recent_media.push_back(media_id);
        if self.recent_media.len() > RECENT_MEDIA_KEEP { self.recent_media.pop_front() } else { None }
    }

    pub(crate) fn purge_message(&mut self, platform: Platform, message_id: &str, event_id: Option<&str>) -> bool {
        let matches = |ev_id: &str, src_id: Option<&str>, plat: Platform| {
            plat == platform
                && (src_id == Some(message_id) || ev_id == message_id || event_id.is_some_and(|e| ev_id == e))
        };
        self.queue.retain(|item| !matches(&item.event_id, item.source_id.as_deref(), item.platform));
        let mut stop = false;
        if let Some(active) = self.active.as_mut()
            && matches(&active.event_id, active.source_id.as_deref(), active.platform)
            && !active.cancelled
        {
            active.cancelled = true;
            stop = true;
        }
        if self.last_dispatched.as_ref().is_some_and(|d| matches(&d.event_id, d.source_id.as_deref(), d.platform)) {
            self.last_dispatched = None;
            stop = true;
        }
        stop
    }

    pub(crate) fn purge_user(&mut self, platform: Platform, actor_id: &str) -> bool {
        if actor_id.is_empty() {
            return false;
        }
        let matches = |aid: Option<&str>, plat: Platform| plat == platform && aid == Some(actor_id);
        self.queue.retain(|item| !matches(item.actor_id.as_deref(), item.platform));
        let mut stop = false;
        if let Some(active) = self.active.as_mut()
            && matches(active.actor_id.as_deref(), active.platform)
            && !active.cancelled
        {
            active.cancelled = true;
            stop = true;
        }
        if self.last_dispatched.as_ref().is_some_and(|d| matches(d.actor_id.as_deref(), d.platform)) {
            self.last_dispatched = None;
            stop = true;
        }
        stop
    }

    pub(crate) fn stop_all(&mut self) -> usize {
        let dropped = self.queue.len();
        self.queue.clear();
        if let Some(active) = self.active.as_mut() {
            active.cancelled = true;
        }
        self.last_dispatched = None;
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
        let (decision, cfg_chat, paused_tts) = {
            let st = self.state();
            (decide_chat(&st.tts, &st.filter, e), st.tts.chat.clone(), st.tts_paused)
        };
        if paused_tts || self.inner.queue.lock().await.is_muted() {
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

    /// Clears all waiting chat TTS lines, cancels any in-flight utterance, and tells alert
    /// overlays to stop speech right away (`tts_stop`).
    pub(crate) fn stop_chat_tts(&self) {
        self.inner.chat_tts.lock().stop_all();
        self.inner.chat_tts.wake.notify_one();
        self.emit_tts_stop();
    }

    pub(crate) fn purge_chat_tts_message(&self, platform: Platform, message_id: &str, event_id: Option<&str>) {
        let stop = self.inner.chat_tts.lock().purge_message(platform, message_id, event_id);
        if stop {
            self.emit_tts_stop();
        }
    }

    pub(crate) fn purge_chat_tts_user(&self, platform: Platform, actor_id: &str) {
        let stop = self.inner.chat_tts.lock().purge_user(platform, actor_id);
        if stop {
            self.emit_tts_stop();
        }
    }

    pub(crate) fn remember_chat_tts_media(&self, media_id: String) -> Option<String> {
        self.inner.chat_tts.lock().remember_media(media_id)
    }

    pub(crate) fn chat_tts_media_ids(&self) -> Vec<String> {
        self.inner.chat_tts.lock().media_ids()
    }

    pub(crate) fn wake_chat_tts(&self) {
        self.inner.chat_tts.wake.notify_one();
    }

    fn emit_tts_stop(&self) {
        self.send_overlay(OverlayFrame::new(Audience::Alerts, self.next_seq(), "tts_stop", &json!({})));
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
                let media_id = engine.speak_local_to_media(&item.text).await;
                let still_allowed = {
                    let st = engine.state();
                    !st.tts_paused && st.tts.enabled && st.tts.chat.enabled
                } && !engine.inner.queue.lock().await.is_muted();

                let finish = engine.inner.chat_tts.lock().finish_active(
                    generation,
                    if still_allowed { media_id.clone() } else { None },
                    still_allowed,
                    now().0,
                );

                if let Some(retired) = finish.retired_media_id {
                    let _ = engine.db(move |s| repo::media::delete(s, &retired)).await;
                }
                if finish.cancelled || !still_allowed {
                    if let Some(unused_id) = media_id {
                        let _ = engine.db(move |s| repo::media::delete(s, &unused_id)).await;
                    }
                    continue;
                }

                crate::studio::voice_commands::reading_aloud(&item.text, now().0);
                engine.send_overlay(OverlayFrame::new(
                    Audience::Alerts,
                    engine.next_seq(),
                    "tts",
                    &json!({ "id": item.id, "text": item.text, "audio": media_id }),
                ));
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

        for i in 2..=12 {
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
        loop {
            let f = rx.recv().await.unwrap();
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
}
