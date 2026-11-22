//! Interactive overlay widgets backed by the engine: End Credits session data and
//! Chat Highlight (pinning one chat message on stream until dismissed or timed out).

use std::time::Duration;

use ferrocast_core::credits::{Credits, MAX_CREDITS_ROWS, build_credits};
use ferrocast_core::event::{Actor, Millis, Platform};
use ferrocast_core::overlay::{ChatHighlight, WidgetKind};
use ferrocast_store::repo;
use serde_json::json;

use crate::frames::{Audience, LiveFrame, OverlayFrame};
use crate::{Engine, EngineError, now};

const MIN_HIGHLIGHT_S: u32 = 5;
const MAX_HIGHLIGHT_S: u32 = 3600;

#[derive(Debug, Default)]
pub(crate) struct HighlightSlot {
    pub(crate) current: Option<ChatHighlight>,
    seq: u64,
}

impl Engine {
    /// Session end-credits data for the End Credits overlay widget and editor preview.
    pub async fn credits(&self) -> Result<Credits, EngineError> {
        let (started, session_credits, live) = {
            let st = self.state();
            (st.session.started_at, st.session.credits.clone(), st.live.values().any(|l| *l))
        };
        if started.is_some() || !session_credits.is_empty() || live {
            return Ok(session_credits);
        }
        let Some(start) = self.db(repo::recap::last_online).await? else {
            return Ok(Credits::default());
        };
        let (support, ended, mods) = self
            .db(move |s| {
                let support = repo::recap::support_since(s, start)?;
                let ended = repo::recap::offline_after(s, start)?;
                let to = ended.unwrap_or(i64::MAX);
                let mods = repo::recap::mods_between(s, start, to, MAX_CREDITS_ROWS)?;
                Ok((support, ended, mods))
            })
            .await?;
        // Support that came in after the stream went offline belongs to no stream.
        let support: Vec<_> = support.into_iter().filter(|e| ended.is_none_or(|end| e.occurred_at.0 <= end)).collect();
        let mut credits = build_credits(&support, Some(Millis(start)));
        for m in mods {
            if credits.mods.len() < MAX_CREDITS_ROWS
                && !credits.mods.iter().any(|x| x.platform == m.platform && x.name.eq_ignore_ascii_case(&m.name))
            {
                credits.mods.push(m);
            }
        }
        credits.mods.sort_by_cached_key(|a| a.name.to_ascii_lowercase());
        if ended.is_some() {
            credits.started_at = Some(Millis(start));
        }
        Ok(credits)
    }

    pub(crate) fn push_credits(&self) {
        let credits = self.state().session.credits.clone();
        self.send_overlay(OverlayFrame::new(
            Audience::Widget(WidgetKind::EndCredits),
            self.next_seq(),
            "state",
            &json!({ "credits": credits }),
        ));
    }

    /// Currently pinned chat message, or `None` if nothing is highlighted (or expired).
    pub fn chat_highlight(&self) -> Option<ChatHighlight> {
        let mut slot = self.inner.highlight.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(h) = &slot.current
            && let Some(exp) = h.expires_at
            && now() >= exp
        {
            slot.current = None;
        }
        slot.current.clone()
    }

    /// Pins one chat message (`event_id`) to display in Chat Highlight widgets until dismissed
    /// or `duration_s` elapses.
    pub async fn pin_chat_highlight(
        &self,
        event_id: &str,
        duration_s: Option<u32>,
    ) -> Result<ChatHighlight, EngineError> {
        let id = event_id.trim();
        if id.is_empty() || id.len() > 128 {
            return Err(EngineError::Invalid("event_id: choose a chat message to show on stream.".into()));
        }
        if let Some(d) = duration_s
            && !(MIN_HIGHLIGHT_S..=MAX_HIGHLIGHT_S).contains(&d)
        {
            return Err(EngineError::Invalid(
                "duration_s: leave empty to stay until dismissed, or pick 5 to 3600 seconds.".into(),
            ));
        }
        let lookup = id.to_string();
        let ev = self.db(move |s| repo::events::get_visible_chat(s, &lookup)).await?.ok_or(EngineError::Missing {
            code: "highlight_not_found",
            message: "That chat message could not be found.",
            hint: "Refresh chat and pick a visible message.",
        })?;
        if ev.source_id.as_deref().is_some_and(|sid| self.is_hidden(ev.platform, sid)) {
            return Err(EngineError::Missing {
                code: "highlight_not_found",
                message: "That chat message was removed.",
                hint: "Pick another visible message.",
            });
        }
        let pinned_at = now();
        let expires_at = duration_s.map(|d| Millis(pinned_at.0 + i64::from(d) * 1000));
        let (highlight, hid) = {
            let mut slot = self.inner.highlight.lock().unwrap_or_else(|p| p.into_inner());
            slot.seq += 1;
            let hid = format!("hl-{}", slot.seq);
            let highlight =
                ChatHighlight::from_event(hid.clone(), &ev, pinned_at, expires_at).ok_or(EngineError::Missing {
                    code: "highlight_not_found",
                    message: "Only chat messages can be highlighted on stream.",
                    hint: "Pick a chat message from the feed.",
                })?;
            slot.current = Some(highlight.clone());
            (highlight, hid)
        };
        self.broadcast_highlight(Some(&highlight));
        if let Some(d) = duration_s {
            let engine = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(u64::from(d))).await;
                engine.expire_chat_highlight(&hid);
            });
        }
        Ok(highlight)
    }

    /// Dismisses the currently pinned chat highlight, if any.
    pub fn clear_chat_highlight(&self) {
        let changed = {
            let mut slot = self.inner.highlight.lock().unwrap_or_else(|p| p.into_inner());
            slot.current.take().is_some()
        };
        if changed {
            self.broadcast_highlight(None);
        }
    }

    fn expire_chat_highlight(&self, hid: &str) {
        let changed = {
            let mut slot = self.inner.highlight.lock().unwrap_or_else(|p| p.into_inner());
            if slot.current.as_ref().is_some_and(|h| h.id == hid) {
                slot.current = None;
                true
            } else {
                false
            }
        };
        if changed {
            self.broadcast_highlight(None);
        }
    }

    pub(crate) fn clear_highlight_if_message(&self, platform: Platform, message_id: &str) {
        let changed = {
            let mut slot = self.inner.highlight.lock().unwrap_or_else(|p| p.into_inner());
            let matches = slot.current.as_ref().is_some_and(|h| {
                h.platform == platform && (h.source_id.as_deref() == Some(message_id) || h.event_id == message_id)
            });
            if matches {
                slot.current = None;
                true
            } else {
                false
            }
        };
        if changed {
            self.broadcast_highlight(None);
        }
    }

    pub(crate) fn clear_highlight_if_user_id(&self, platform: Platform, user_id: &str) {
        if user_id.is_empty() {
            return;
        }
        let changed = {
            let mut slot = self.inner.highlight.lock().unwrap_or_else(|p| p.into_inner());
            let matches =
                slot.current.as_ref().is_some_and(|h| h.platform == platform && h.actor.id.as_deref() == Some(user_id));
            if matches {
                slot.current = None;
                true
            } else {
                false
            }
        };
        if changed {
            self.broadcast_highlight(None);
        }
    }

    pub(crate) fn clear_highlight_if_user(&self, platform: Platform, target: &Actor) {
        let changed = {
            let mut slot = self.inner.highlight.lock().unwrap_or_else(|p| p.into_inner());
            let matches = slot.current.as_ref().is_some_and(|h| {
                h.platform == platform
                    && match (&target.id, &h.actor.id) {
                        (Some(a), Some(b)) => a == b,
                        _ => {
                            (target.login.is_some() && h.actor.login == target.login)
                                || h.actor.display_name.eq_ignore_ascii_case(&target.display_name)
                        }
                    }
            });
            if matches {
                slot.current = None;
                true
            } else {
                false
            }
        };
        if changed {
            self.broadcast_highlight(None);
        }
    }

    fn broadcast_highlight(&self, highlight: Option<&ChatHighlight>) {
        self.send_live(LiveFrame::new("chat_highlight", &json!({ "highlight": highlight })));
        self.send_overlay(OverlayFrame::new(
            Audience::Widget(WidgetKind::ChatHighlight),
            self.next_seq(),
            "state",
            &json!({ "chat_highlight": highlight }),
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrocast_core::event::{
        Content, Event, EventKind, Millis, Origin, PaymentStatus, Platform, Roles, Tier, TipProvider,
    };
    use ferrocast_core::money::{Money, PlatformValue, ValueUnit};
    use ferrocast_secrets::MemorySecrets;

    use super::*;

    async fn test_engine() -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::start_with_secrets(dir.path(), Arc::new(MemorySecrets::default())).await.unwrap();
        (e, dir)
    }

    fn viewer(p: Platform, name: &str, roles: Roles) -> Actor {
        Actor {
            platform: p,
            id: Some(format!("{name}-id")),
            login: Some(name.to_ascii_lowercase()),
            display_name: name.into(),
            avatar_url: None,
            color: None,
            roles,
            badges: Vec::new(),
            anonymous: false,
        }
    }

    #[tokio::test]
    async fn session_credits_collect_and_hydrate_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let engine = Engine::start_with_secrets(dir.path(), secrets.clone()).await.unwrap();
        let t0 = now().0 - 60_000;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("on-1".into()),
                Millis(t0),
                EventKind::StreamOnline { title: Some("Live".into()) },
            ))
            .await;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("f-1".into()),
                Millis(t0 + 1000),
                EventKind::Follow { actor: viewer(Platform::Twitch, "FollowerOne", Roles::empty()) },
            ))
            .await;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("s-1".into()),
                Millis(t0 + 2000),
                EventKind::Subscription {
                    actor: viewer(Platform::Twitch, "SubOne", Roles::SUBSCRIBER),
                    tier: Tier::Tier1,
                    months: 4,
                    message: None,
                    gifted_by: None,
                },
            ))
            .await;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("g-1".into()),
                Millis(t0 + 3000),
                EventKind::GiftBatch {
                    gifter: viewer(Platform::Twitch, "GifterOne", Roles::empty()),
                    tier: Tier::Tier1,
                    count: 5,
                },
            ))
            .await;
        engine
            .ingest(Event::live(
                Platform::Tips,
                Some("t-1".into()),
                Millis(t0 + 4000),
                EventKind::Tip {
                    actor: viewer(Platform::Tips, "TipperOne", Roles::empty()),
                    money: Money::new(50_000, "INR"),
                    message: None,
                    provider: TipProvider::Razorpay,
                    payment_ref: "pay-1".into(),
                    status: PaymentStatus::Captured,
                },
            ))
            .await;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("b-1".into()),
                Millis(t0 + 5000),
                EventKind::PlatformGift {
                    actor: viewer(Platform::Twitch, "BitsOne", Roles::empty()),
                    value: PlatformValue { unit: ValueUnit::Bits, amount: 250 },
                    message: None,
                },
            ))
            .await;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("r-1".into()),
                Millis(t0 + 6000),
                EventKind::Raid { from: viewer(Platform::Twitch, "RaiderOne", Roles::empty()), viewers: 35 },
            ))
            .await;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("c-mod".into()),
                Millis(t0 + 7000),
                EventKind::ChatMessage {
                    actor: viewer(Platform::Twitch, "ModOne", Roles::MODERATOR),
                    content: Content::plain("hello chat"),
                    reply_to: None,
                    is_action: false,
                },
            ))
            .await;

        let live_credits = engine.credits().await.unwrap();
        assert_eq!(live_credits.followers.len(), 1);
        assert_eq!(live_credits.subs.len(), 1);
        assert_eq!(live_credits.gifters[0].gifts, 5);
        assert_eq!(live_credits.tippers.len(), 2);
        assert_eq!(live_credits.raiders[0].viewers, 35);
        assert_eq!(live_credits.mods.len(), 1);

        // Simulate app restart after stream ended: hydrated cleanly from DB.
        drop(engine);
        let restarted = Engine::start_with_secrets(dir.path(), secrets).await.unwrap();
        let saved = restarted.credits().await.unwrap();
        assert_eq!(saved.followers.len(), 1);
        assert_eq!(saved.subs.len(), 1);
        assert_eq!(saved.gifters.len(), 1);
        assert_eq!(saved.tippers.len(), 2);
        assert_eq!(saved.raiders.len(), 1);
        assert_eq!(saved.mods.len(), 1);
        assert_eq!(saved.mods[0].name, "ModOne");
    }

    /// Overlays drop frames at or below the snapshot's seq, so live widget frames
    /// must carry a fresh, increasing number (a hard-coded 0 was silently dropped).
    #[tokio::test]
    async fn live_widget_frames_carry_increasing_seq() {
        let (engine, _dir) = test_engine().await;
        let mut rx = engine.subscribe_overlays();
        let snap_seq = engine.current_seq();
        let t0 = now().0 - 10_000;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("seq-f".into()),
                Millis(t0),
                EventKind::Follow { actor: viewer(Platform::Twitch, "SeqFollower", Roles::empty()) },
            ))
            .await;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("seq-m".into()),
                Millis(t0 + 10),
                EventKind::ChatMessage {
                    actor: viewer(Platform::Twitch, "SeqChatter", Roles::empty()),
                    content: Content::plain("hi"),
                    reply_to: None,
                    is_action: false,
                },
            ))
            .await;
        let id = engine.db(|s| repo::events::by_source(s, Platform::Twitch, "seq-m")).await.unwrap().unwrap();
        engine.pin_chat_highlight(&id, None).await.unwrap();
        engine.clear_chat_highlight();
        let mut seen = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if matches!(f.audience, Audience::Widget(_)) {
                seen.push(f.seq);
            }
        }
        assert!(seen.len() >= 3, "expected credits and highlight frames, got {seen:?}");
        assert!(seen.iter().all(|s| *s > snap_seq), "frame seq must exceed snapshot seq {snap_seq}: {seen:?}");
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "frame seqs must increase: {seen:?}");
    }

    #[tokio::test]
    async fn a_refunded_tip_leaves_the_live_credits_and_the_restart_rebuild() {
        let (engine, _dir) = test_engine().await;
        let t0 = now().0 - 30_000;
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("on-r".into()),
                Millis(t0),
                EventKind::StreamOnline { title: None },
            ))
            .await;
        let tip = |src: &str, who: &str, minor: i64, at: i64| {
            Event::live(
                Platform::Tips,
                Some(src.into()),
                Millis(at),
                EventKind::Tip {
                    actor: viewer(Platform::Tips, who, Roles::empty()),
                    money: Money::new(minor, "INR"),
                    message: None,
                    provider: TipProvider::Razorpay,
                    payment_ref: src.into(),
                    status: PaymentStatus::Captured,
                },
            )
        };
        // The session starts "now", so the payments land after it.
        let after = now().0 + 1000;
        engine.ingest(tip("pay-bad", "Chargeback", 900_000, after)).await;
        engine.ingest(tip("pay-ok", "Honest", 5_000, after + 1000)).await;
        assert_eq!(engine.credits().await.unwrap().tippers.len(), 2);
        let mut rx = engine.subscribe_overlays();
        engine
            .ingest(Event::live(
                Platform::Tips,
                Some("rev-1".into()),
                Millis(t0 + 3000),
                EventKind::Reversal {
                    original_source_id: "pay-bad".into(),
                    reason: ferrocast_core::event::ReversalReason::Chargeback,
                },
            ))
            .await;
        let c = engine.credits().await.unwrap();
        assert_eq!(c.tippers.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Honest"]);
        let mut pushed = false;
        while let Ok(f) = rx.try_recv() {
            pushed |= matches!(f.audience, Audience::Widget(WidgetKind::EndCredits));
        }
        assert!(pushed, "open End Credits sources are told");
    }

    #[tokio::test]
    async fn after_a_restart_editing_an_overlay_keeps_the_last_streams_credits_and_drops_later_ones() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let engine = Engine::start_with_secrets(dir.path(), secrets.clone()).await.unwrap();
        let t0 = now().0 - 120_000;
        let at = |ms: i64, id: &str, kind: EventKind| Event::live(Platform::Twitch, Some(id.into()), Millis(t0 + ms), kind);
        engine.ingest(at(0, "on", EventKind::StreamOnline { title: None })).await;
        engine
            .ingest(at(1000, "f-in", EventKind::Follow { actor: viewer(Platform::Twitch, "DuringStream", Roles::empty()) }))
            .await;
        engine.ingest(at(2000, "off", EventKind::StreamOffline)).await;
        engine
            .ingest(at(3000, "f-late", EventKind::Follow { actor: viewer(Platform::Twitch, "AfterOffline", Roles::empty()) }))
            .await;
        drop(engine);

        let engine = Engine::start_with_secrets(dir.path(), secrets).await.unwrap();
        let names: Vec<_> = engine.credits().await.unwrap().followers.into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["DuringStream"], "a follow after the stream ended is not in its credits");

        let o = engine.create_overlay("Credits", ferrocast_core::overlay::Preset::Blank).await.unwrap();
        let mut full = engine.overlay(&o.id).await.unwrap();
        full.widgets = vec![
            serde_json::from_value(
                json!({ "id": "ec", "kind": "end_credits", "x": 0, "y": 0, "w": 800, "h": 600, "config": {} }),
            )
            .unwrap(),
        ];
        let mut rx = engine.subscribe_overlays();
        engine.update_overlay(full).await.unwrap();
        let mut sent = None;
        while let Ok(f) = rx.try_recv() {
            if f.text.contains("\"type\":\"state\"") && f.text.contains("credits") {
                sent = Some(f.text.to_string());
            }
        }
        let sent = sent.expect("a credits state frame follows the config");
        assert!(sent.contains("DuringStream"), "saving a layout must not blank the credits: {sent}");
        assert!(!sent.contains("AfterOffline"), "{sent}");
    }

    #[tokio::test]
    async fn chat_highlight_lifecycle_clears_on_delete_ban_and_new_stream() {
        let (engine, _dir) = test_engine().await;
        let t0 = now().0 - 10_000;
        let mut msg = Event::live(
            Platform::Twitch,
            Some("tw-msg-1".into()),
            Millis(t0),
            EventKind::ChatMessage {
                actor: viewer(Platform::Twitch, "ChatterA", Roles::empty()),
                content: Content::plain("pin me please"),
                reply_to: None,
                is_action: false,
            },
        );
        msg.id = "ev-pin-1".into();
        engine.ingest(msg.clone()).await;
        let id1 = engine.db(|s| repo::events::by_source(s, Platform::Twitch, "tw-msg-1")).await.unwrap().unwrap();

        let mut msg2 = Event::live(
            Platform::Twitch,
            Some("tw-msg-2".into()),
            Millis(t0 + 100),
            EventKind::ChatMessage {
                actor: viewer(Platform::Twitch, "ChatterB", Roles::empty()),
                content: Content::plain("ban me next"),
                reply_to: None,
                is_action: false,
            },
        );
        msg2.id = "ev-pin-2".into();
        engine.ingest(msg2).await;
        let id2 = engine.db(|s| repo::events::by_source(s, Platform::Twitch, "tw-msg-2")).await.unwrap().unwrap();

        // Invalid input rejected.
        assert!(engine.pin_chat_highlight("   ", None).await.is_err());
        assert!(engine.pin_chat_highlight(&id1, Some(2)).await.is_err());
        assert!(engine.pin_chat_highlight("missing-id", None).await.is_err());

        // Pin -> delete message -> cleared automatically.
        let pinned = engine.pin_chat_highlight(&id1, None).await.unwrap();
        assert_eq!(pinned.content.text, "pin me please");
        assert!(engine.chat_highlight().is_some());
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("del-1".into()),
                Millis(t0 + 200),
                EventKind::MessageDeleted { message_id: "tw-msg-1".into() },
            ))
            .await;
        assert!(engine.chat_highlight().is_none(), "deleted message clears highlight");
        // Deleted/hidden message cannot be re-pinned!
        assert!(engine.pin_chat_highlight(&id1, None).await.is_err());

        // Pin second message -> user banned -> cleared automatically.
        engine.pin_chat_highlight(&id2, None).await.unwrap();
        assert!(engine.chat_highlight().is_some());
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("ban-1".into()),
                Millis(t0 + 300),
                EventKind::Ban {
                    target: viewer(Platform::Twitch, "ChatterB", Roles::empty()),
                    reason: None,
                    until: None,
                },
            ))
            .await;
        assert!(engine.chat_highlight().is_none(), "banning author clears highlight");

        // Fresh StreamOnline resets previous highlight and credits.
        let mut msg3 = Event::live(
            Platform::Kick,
            Some("kick-3".into()),
            Millis(t0 + 400),
            EventKind::ChatMessage {
                actor: viewer(Platform::Kick, "ModKick", Roles::MODERATOR),
                content: Content::plain("ready"),
                reply_to: None,
                is_action: false,
            },
        );
        msg3.id = "ev-pin-3".into();
        engine.ingest(msg3).await;
        let id3 = engine.db(|s| repo::events::by_source(s, Platform::Kick, "kick-3")).await.unwrap().unwrap();
        engine.pin_chat_highlight(&id3, Some(60)).await.unwrap();
        assert!(!engine.credits().await.unwrap().is_empty());
        engine
            .ingest(Event::live(
                Platform::Twitch,
                Some("on-2".into()),
                Millis(t0 + 500),
                EventKind::StreamOnline { title: Some("Fresh stream".into()) },
            ))
            .await;
        assert!(engine.chat_highlight().is_none(), "new stream clears stale highlight");
        assert!(engine.credits().await.unwrap().is_empty(), "new stream resets session credits");
        let _ = Origin::Live;
    }
}
