//! Drives the alert queue: sends alerts to overlays, hides them, and persists every change
//! so a crash never loses a paid alert (R-31).

use std::time::Duration;

use ferrocast_core::alert::RenderedAlert;
use ferrocast_core::queue::{AckPhase, Action, AlertQueue};
use ferrocast_store::repo;
use serde::Serialize;
use serde_json::json;

use crate::frames::{Audience, LiveFrame, Notice, NoticeLevel, OverlayFrame};
use crate::{Engine, EngineError, now};

/// The longest the dispatcher sleeps without checking, as a safety net.
const MAX_IDLE: Duration = Duration::from_secs(30);

pub(crate) async fn run(engine: Engine) {
    loop {
        let wake = engine.inner.queue.lock().await.next_wake();
        let sleep = match wake {
            Some(at) => Duration::from_millis(u64::try_from((at.0 - now().0).max(0)).unwrap_or(0)).min(MAX_IDLE),
            None => MAX_IDLE,
        };
        tokio::select! {
            _ = engine.inner.queue_changed.notified() => {}
            _ = tokio::time::sleep(sleep) => {}
        }
        engine.queue_op(|q, t| q.tick(t)).await;
    }
}

/// The queue as the dashboard shows it (API.md `GET /api/v1/queue`).
#[derive(Debug, Clone, Serialize)]
pub struct QueueView {
    pub paused: bool,
    pub playing: Option<PlayingView>,
    pub waiting: Vec<WaitingView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayingView {
    pub alert: RenderedAlert,
    pub started_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WaitingView {
    pub alert: RenderedAlert,
    pub priority: i32,
    pub enqueued_at: i64,
}

fn view(q: &AlertQueue) -> QueueView {
    QueueView {
        paused: q.is_paused(),
        playing: q.playing().map(|p| PlayingView { alert: p.alert.clone(), started_at: p.started_at.0 }),
        waiting: q
            .waiting()
            .map(|w| WaitingView { alert: w.alert.clone(), priority: w.priority, enqueued_at: w.enqueued_at.0 })
            .collect(),
    }
}

impl Engine {
    /// Runs one queue operation, carries out its actions, persists and tells the dashboard.
    pub(crate) async fn queue_op(
        &self,
        op: impl FnOnce(&mut AlertQueue, ferrocast_core::event::Millis) -> Vec<Action>,
    ) -> QueueView {
        let (actions, changed, v) = {
            let mut q = self.inner.queue.lock().await;
            let before = q.clone();
            let actions = op(&mut q, now());
            let changed = *q != before;
            // Save while still holding the queue, so saves land in the order the changes
            // happened and an older state can never overwrite a newer one (R-31). An idle
            // tick changes nothing and writes nothing.
            if changed {
                let snapshot = q.clone();
                if let Err(err) = self.db(move |s| repo::rules::save_queue(s, &snapshot)).await {
                    tracing::error!(%err, "couldn't save the alert queue");
                }
            }
            (actions, changed, view(&q))
        };
        for a in actions {
            match a {
                Action::Show { alert } => {
                    self.send_overlay(OverlayFrame::new(Audience::Alerts, self.next_seq(), "alert", &alert));
                }
                Action::Hide { alert_id } => {
                    self.send_overlay(OverlayFrame::new(
                        Audience::Alerts,
                        self.next_seq(),
                        "alert_hide",
                        &json!({ "alert_id": alert_id }),
                    ));
                }
                Action::Folded { alert_ids } => {
                    self.send_live(LiveFrame::new(
                        "notice",
                        &Notice {
                            key: Some("alerts-folded".into()),
                            level: NoticeLevel::Info,
                            message: format!(
                                "{} alert{} waited too long and went to the event list instead.",
                                alert_ids.len(),
                                if alert_ids.len() == 1 { "" } else { "s" }
                            ),
                            action: None,
                        },
                    ));
                }
            }
        }
        if changed {
            self.send_live(LiveFrame::new("queue", &v));
        }
        v
    }

    pub(crate) async fn enqueue_alert(&self, alert: RenderedAlert, priority: i32) {
        let speak = alert.tts_text.clone().map(|t| (alert.alert_id.clone(), t));
        self.queue_op(move |q, t| q.enqueue(alert, priority, t)).await;
        self.inner.queue_changed.notify_one();
        if let Some((alert_id, text)) = speak {
            let engine = self.clone();
            tokio::spawn(async move {
                if let Some(media) = engine.speak_to_media(&text).await {
                    engine
                        .queue_op(move |q, _| {
                            q.set_tts_audio(&alert_id, &media);
                            vec![]
                        })
                        .await;
                }
            });
        }
    }

    /// Synthesises `text` into the media library; `None` if this computer can't (the
    /// overlay then uses the browser's voice).
    pub(crate) async fn speak_to_media(&self, text: &str) -> Option<String> {
        let voice = self.state().tts.voice.clone();
        let work = self.store().dir().join("tmp");
        let out = work.join(format!("tts-{}.wav", ferrocast_store::new_id()));
        if let Err(e) = crate::speech::synthesize(text, voice.as_deref(), &work, &out).await {
            tracing::warn!(error = %e, "text-to-speech failed; the overlay will use the browser voice");
            return None;
        }
        let bytes = tokio::fs::read(&out).await.ok()?;
        let _ = tokio::fs::remove_file(&out).await;
        let saved = self.db(move |s| Ok(repo::media::save(s, "speech.wav", &bytes).ok())).await.ok().flatten()?;
        Some(saved.id)
    }

    /// An overlay reports progress on an alert. Only the alert that is playing counts.
    pub async fn ack(&self, alert_id: &str, phase: AckPhase) {
        let id = alert_id.to_string();
        self.queue_op(move |q, t| q.ack(&id, phase, t)).await;
        self.inner.queue_changed.notify_one();
    }

    pub async fn queue(&self) -> QueueView {
        view(&*self.inner.queue.lock().await)
    }

    pub async fn queue_pause(&self) -> QueueView {
        self.queue_op(|q, _| {
            q.pause();
            vec![]
        })
        .await
    }

    pub async fn queue_resume(&self) -> QueueView {
        let v = self.queue_op(|q, t| q.resume(t)).await;
        self.inner.queue_changed.notify_one();
        v
    }

    pub async fn queue_skip(&self) -> QueueView {
        let v = self.queue_op(|q, t| q.skip(t)).await;
        self.inner.queue_changed.notify_one();
        v
    }

    pub async fn queue_clear(&self) -> QueueView {
        self.queue_op(|q, _| {
            q.clear_waiting();
            vec![]
        })
        .await
    }

    pub async fn queue_remove(&self, alert_id: &str) -> Result<QueueView, EngineError> {
        let id = alert_id.to_string();
        let mut found = false;
        let v = self
            .queue_op(|q, _| {
                found = q.remove(&id);
                vec![]
            })
            .await;
        if !found {
            return Err(EngineError::NotFound("waiting alert"));
        }
        Ok(v)
    }
}
