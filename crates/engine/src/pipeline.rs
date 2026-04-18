//! The event pipeline: normalise → dedupe → store → moderate → goals → alerts → fan out.
//!
//! One task runs this for every event in arrival order, so alerts never overtake each
//! other. Anything slow (talking to a platform) is spawned off, never awaited inline.

use ferrocast_core::alert::{render, select};
use ferrocast_core::command::{self, Skip};
use ferrocast_core::event::{Event, EventKind, Origin, Platform};
use ferrocast_core::filter::{self, Action};
use ferrocast_core::goal;
use ferrocast_core::template::Vars;
use ferrocast_core::tts::{self, Decision};
use ferrocast_store::repo;
use serde_json::json;
use tokio::sync::mpsc;

use crate::frames::{Audience, LiveFrame, Notice, NoticeLevel, OverlayFrame};
use crate::state::Why;
use crate::{Engine, now, platform_key};

/// Runs until every sender is gone.
pub(crate) async fn run(engine: Engine, mut rx: mpsc::Receiver<Event>) {
    while let Some(event) = rx.recv().await {
        engine.ingest(event).await;
    }
}

impl Engine {
    /// Processes one event. Public for tests and the replay endpoint; adapters use the sink.
    pub async fn ingest(&self, mut e: Event) {
        let at = now();
        e.id = ferrocast_store::new_id();
        e.seq = self.next_seq();
        e.received_at = at;
        if e.occurred_at.0 == 0 {
            e.occurred_at = at;
        }

        // A payment arrives once per status change with the same id. Only the captured
        // event keeps the bare id (goals and refunds refer to it); other statuses get their
        // own key, so "pending" never blocks the later "captured" as a duplicate.
        if let EventKind::Tip { status, .. } = &e.kind
            && *status != ferrocast_core::event::PaymentStatus::Captured
            && let Some(id) = &e.source_id
        {
            e.source_id = Some(format!("{id}#{}", tip_status_key(*status)));
        }

        if e.origin != Origin::Test {
            let first = self
                .inner
                .dedupe
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .first_time(&e, at);
            if !first {
                tracing::debug!(platform = %e.platform, source = ?e.source_id, "duplicate dropped");
                return;
            }
        }

        // Health and viewer counts are state, not history.
        match &e.kind {
            EventKind::Connector(status) => {
                let key = platform_key(e.platform).to_string();
                let view = {
                    let mut st = self.state_mut();
                    let v = st
                        .connectors
                        .entry(key.clone())
                        .or_insert_with(crate::state::ConnectorView::off);
                    v.apply(status);
                    v.last_event_at = Some(at.0);
                    st.connectors.clone()
                };
                self.send_live(LiveFrame::new("status", &json!({ "connectors": view })));
                return;
            }
            EventKind::ViewerCount { count } => {
                let viewers = {
                    let mut st = self.state_mut();
                    st.viewers.insert(e.platform, *count);
                    st.viewers
                        .iter()
                        .map(|(p, c)| (platform_key(*p).to_string(), *c))
                        .collect::<std::collections::HashMap<_, _>>()
                };
                self.send_overlay(OverlayFrame::new(
                    Audience::All,
                    e.seq,
                    "state",
                    &json!({ "viewers": viewers }),
                ));
                return;
            }
            _ => {}
        }

        if e.origin != Origin::Test {
            let stored = e.clone();
            match self.db(move |s| repo::events::insert(s, &stored)).await {
                Ok(Some(_)) => {}
                Ok(None) => return, // the platform delivered this one before (a restart ago)
                Err(err) => tracing::error!(%err, "couldn't log an event; continuing"),
            }
            if let Some(view) = self
                .state_mut()
                .connectors
                .get_mut(platform_key(e.platform))
            {
                view.last_event_at = Some(at.0);
            }
        }

        let mut show_in_overlays = true;
        match &e.kind {
            EventKind::ChatMessage { .. } => show_in_overlays = self.moderate_and_answer(&e).await,
            EventKind::MessageDeleted { message_id } => {
                self.hide_message(e.platform, message_id);
                let id = message_id.clone();
                let p = e.platform;
                let deleted_event = self
                    .db(move |s| {
                        let ev = repo::events::by_source(s, p, &id)?;
                        if let Some(ev) = &ev {
                            repo::events::drop_pending_tts_for_event(s, ev)?;
                        }
                        Ok(ev)
                    })
                    .await
                    .ok()
                    .flatten();
                // A queued alert for this message must not read it aloud either.
                if let Some(ev) = deleted_event {
                    self.queue_op(move |q, _| {
                        q.strip_tts(&ev);
                        vec![]
                    })
                    .await;
                    self.push_tts_pending().await;
                }
                self.send_overlay(OverlayFrame::new(
                    Audience::Chat,
                    e.seq,
                    "delete_message",
                    &json!({ "platform": e.platform, "message_id": message_id }),
                ));
            }
            EventKind::UserMessagesCleared { target } | EventKind::Ban { target, .. } => {
                if let Some(uid) = &target.id {
                    self.hide_user(e.platform, uid);
                }
                self.send_overlay(OverlayFrame::new(
                    Audience::Chat,
                    e.seq,
                    "delete_user",
                    &json!({ "platform": e.platform, "user_id": target.id }),
                ));
            }
            EventKind::StreamOnline { .. } => {
                let mut st = self.state_mut();
                let anyone_live = st.live.values().any(|l| *l);
                st.live.insert(e.platform, true);
                if !anyone_live {
                    st.session = crate::state::Session {
                        started_at: Some(at),
                        ..Default::default()
                    };
                }
            }
            EventKind::StreamOffline => {
                self.state_mut().live.insert(e.platform, false);
            }
            EventKind::Reversal {
                original_source_id,
                reason,
            } => {
                self.reverse(e.platform, original_source_id, *reason).await;
            }
            _ => {}
        }

        if e.kind.is_alertable() || matches!(e.kind, EventKind::Tip { .. }) {
            self.support_event(&e).await;
        }

        self.send_live(LiveFrame::new("event", &e));
        if show_in_overlays {
            let audience = match e.kind {
                EventKind::ChatMessage { .. } => Some(Audience::Chat),
                _ if e.kind.is_alertable() && e.origin == Origin::Live => Some(Audience::Alerts),
                _ => None,
            };
            if let Some(a) = audience {
                let mut public = e.clone();
                public.raw = None;
                self.send_overlay(OverlayFrame::new(a, e.seq, "event", &public));
            }
        }
    }

    /// Ban-on-sight, the chat filter, and bot commands. Returns whether the message may
    /// appear in overlays.
    async fn moderate_and_answer(&self, e: &Event) -> bool {
        let EventKind::ChatMessage { actor, content, .. } = &e.kind else {
            return true;
        };
        let a = actor.clone();
        let _ = self.db(move |s| repo::viewers::touch(s, &a, true)).await;
        if e.origin == Origin::Live {
            self.state_mut().session.record(e);
        }
        let message_id = e.source_id.clone().unwrap_or_default();

        let (on_sight, verdict, bot_on) = {
            let st = self.state();
            let on_sight = st.settings.auto_ban_on_sight && st.bans.on_sight(actor).is_some();
            let emotes = content
                .fragments
                .iter()
                .filter(|f| matches!(f, ferrocast_core::event::Fragment::Emote { .. }))
                .count();
            (
                on_sight,
                filter::check(&st.filter, actor, &content.text, emotes),
                st.settings.bot_enabled,
            )
        };

        let user_id = actor.id.clone();
        if on_sight {
            if let Some(uid) = user_id.clone() {
                self.spawn_action(e.platform, "ban someone on your ban list", move |c| {
                    Box::pin(async move { c.ban(&uid, Some("on the Ferrocast ban list")).await })
                });
            }
            self.hide_message(e.platform, &message_id);
            return false;
        }

        match verdict.action {
            Action::Allow => {}
            Action::Hold => {
                self.hide_message(e.platform, &message_id);
                self.send_live(LiveFrame::new(
                    "held",
                    &json!({ "event_id": e.id, "reason": verdict.reason }),
                ));
                return false;
            }
            Action::Delete | Action::Timeout { .. } | Action::Ban => {
                self.hide_message(e.platform, &message_id);
                let reason = verdict.reason.clone();
                let mid = message_id.clone();
                match verdict.action {
                    Action::Delete if !mid.is_empty() => {
                        self.spawn_action(e.platform, "delete a message", move |c| {
                            Box::pin(async move { c.delete_message(&mid).await })
                        })
                    }
                    Action::Timeout { seconds } => {
                        if let Some(uid) = user_id {
                            self.spawn_action(e.platform, "time someone out", move |c| {
                                Box::pin(
                                    async move { c.timeout(&uid, seconds, Some(&reason)).await },
                                )
                            });
                        }
                    }
                    Action::Ban => {
                        if let Some(uid) = user_id {
                            self.spawn_action(e.platform, "ban someone", move |c| {
                                Box::pin(async move { c.ban(&uid, Some(&reason)).await })
                            });
                        }
                    }
                    _ => {}
                }
                return false;
            }
        }

        if bot_on && content.text.trim_start().starts_with('!') {
            self.answer_command(e).await;
        }
        true
    }

    async fn answer_command(&self, e: &Event) {
        let EventKind::ChatMessage { actor, content, .. } = &e.kind else {
            return;
        };
        let mut extra = Vars::new();
        if let Some(started) = self.state().session.started_at {
            let mins = now().seconds_since(started) / 60;
            extra.insert("uptime".into(), format!("{}h {}m", mins / 60, mins % 60));
        }
        let result = {
            let commands = self.state().commands.clone();
            let mut cd = self
                .inner
                .cooldowns
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            command::run(
                &commands,
                &mut cd,
                e.platform,
                actor,
                &content.text,
                &extra,
                now(),
            )
        };
        match result {
            Ok(reply) if !reply.is_empty() => {
                let reply_to = e.source_id.clone();
                self.spawn_action(e.platform, "answer a command", move |c| {
                    Box::pin(async move { c.send_chat(&reply, reply_to.as_deref()).await })
                });
            }
            Ok(_) | Err(Skip::NotACommand | Skip::Unknown) => {}
            Err(skip) => tracing::debug!(?skip, "command not run"),
        }
    }

    /// Goals, session numbers, TTS and the alert for a support event.
    async fn support_event(&self, e: &Event) {
        let mut reasons = Vec::new();
        if e.origin == Origin::Live {
            self.state_mut().session.record(e);
            self.count_goals(e).await;
        }

        let (rules, test_alerts, tts_decision, paused_tts) = {
            let st = self.state();
            (
                st.rules.clone(),
                st.settings.test_alerts,
                tts::decide(&st.tts, &st.filter, e),
                st.tts_paused,
            )
        };
        let decision = if paused_tts {
            Decision::Skip {
                reason: "panic mode paused TTS".into(),
            }
        } else {
            tts_decision
        };

        let mut alerted = false;
        if !e.may_alert() {
            reasons.push(match (&e.kind, e.origin) {
                (EventKind::Tip { .. }, _) => {
                    "The payment isn't confirmed yet, so it didn't alert.".into()
                }
                (_, Origin::Backfill) => {
                    "It arrived after a reconnect, so it went to the feed without an alert.".into()
                }
                _ => "This kind of event doesn't show an alert.".into(),
            });
        } else if e.origin == Origin::Test && !test_alerts {
            reasons.push("Test alerts are turned off in settings.".into());
        } else if let Some((rule, variation)) = select(&rules, e) {
            let mut alert = render(&ferrocast_store::new_id(), rule, variation, e);
            if let Decision::Speak { text } = &decision {
                alert.tts_text = Some(text.clone());
            } else {
                alert.tts_text = None;
            }
            let priority = if e.origin == Origin::Test {
                0
            } else {
                variation.priority
            };
            self.enqueue_alert(alert, priority).await;
            alerted = true;
        } else {
            reasons.push(format!(
                "No turned-on alert rule covers {} on {}.",
                e.kind.name().replace('_', " "),
                e.platform.label()
            ));
        }
        if let Decision::Hold { text } = &decision {
            let p = repo::events::PendingTts {
                id: ferrocast_store::new_id(),
                event_id: e.id.clone(),
                text: text.clone(),
                created_at: now().0,
            };
            let _ = self.db(move |s| repo::events::add_pending_tts(s, &p)).await;
            self.push_tts_pending().await;
            reasons.push("The message is waiting for TTS approval.".into());
        }
        self.state_mut()
            .remember_why(&e.id, Why { alerted, reasons });
    }

    async fn count_goals(&self, e: &Event) {
        let goals = self.state().goals.clone();
        let source = e.source_id.clone().unwrap_or_else(|| e.id.clone());
        let mut changed = false;
        for g in goals {
            let amount = goal::contribution(&g, e);
            if amount == 0 {
                continue;
            }
            let (gid, p, src) = (g.id.clone(), e.platform, source.clone());
            match self
                .db(move |s| repo::goals::contribute(s, &gid, p, &src, amount))
                .await
            {
                Ok(Some(updated)) => {
                    let mut st = self.state_mut();
                    if let Some(slot) = st.goals.iter_mut().find(|x| x.id == updated.id) {
                        *slot = updated;
                    }
                    changed = true;
                }
                Ok(None) => {}
                Err(err) => tracing::error!(%err, "couldn't count a goal"),
            }
        }
        if changed {
            self.broadcast_goals(e.seq);
        }
    }

    async fn reverse(
        &self,
        platform: Platform,
        source_id: &str,
        reason: ferrocast_core::event::ReversalReason,
    ) {
        let src = source_id.to_string();
        match self
            .db(move |s| repo::goals::reverse(s, platform, &src))
            .await
        {
            Ok(changed) if !changed.is_empty() => {
                {
                    let mut st = self.state_mut();
                    for g in changed {
                        if let Some(slot) = st.goals.iter_mut().find(|x| x.id == g.id) {
                            *slot = g;
                        }
                    }
                }
                self.broadcast_goals(self.current_seq());
            }
            Ok(_) => {}
            Err(err) => tracing::error!(%err, "couldn't reverse goal contributions"),
        }
        let what = match reason {
            ferrocast_core::event::ReversalReason::Chargeback => "A chargeback",
            ferrocast_core::event::ReversalReason::Refund => "A refund",
            ferrocast_core::event::ReversalReason::Cancelled => "A cancelled payment",
        };
        self.send_live(LiveFrame::new(
            "notice",
            &Notice {
                level: NoticeLevel::Warn,
                message: format!("{what} took a payment back; goals were updated."),
                action: None,
            },
        ));
    }

    pub(crate) fn broadcast_goals(&self, seq: u64) {
        let goals = self.state().goals.clone();
        self.send_overlay(OverlayFrame::new(
            Audience::All,
            seq,
            "state",
            &json!({ "goals": goals }),
        ));
        self.send_live(LiveFrame::new("goals", &goals));
    }

    pub(crate) async fn push_tts_pending(&self) {
        if let Ok(list) = self.db(repo::events::pending_tts).await {
            self.send_live(LiveFrame::new("tts_pending", &list));
        }
    }
}

fn tip_status_key(s: ferrocast_core::event::PaymentStatus) -> &'static str {
    use ferrocast_core::event::PaymentStatus;
    match s {
        PaymentStatus::Pending => "pending",
        PaymentStatus::Captured => "captured",
        PaymentStatus::Refunded => "refunded",
        PaymentStatus::Disputed => "disputed",
    }
}
