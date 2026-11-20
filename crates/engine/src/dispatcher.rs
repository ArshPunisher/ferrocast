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

/// Collects every `tts_audio` media id currently referenced by `q` (playing or waiting).
fn referenced_tts(q: &AlertQueue) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = q.playing().and_then(|p| p.alert.tts_audio.as_ref()) {
        ids.push(id.clone());
    }
    for w in q.waiting() {
        if let Some(id) = w.alert.tts_audio.as_ref() {
            ids.push(id.clone());
        }
    }
    ids
}

impl Engine {
    /// Runs one queue operation, carries out its actions, persists and tells the dashboard.
    pub(crate) async fn queue_op(
        &self,
        op: impl FnOnce(&mut AlertQueue, ferrocast_core::event::Millis) -> Vec<Action>,
    ) -> QueueView {
        let (actions, changed, v, retired_tts) = {
            let mut q = self.inner.queue.lock().await;
            let before = q.clone();
            let before_tts = referenced_tts(&before);
            let actions = op(&mut q, now());
            let changed = *q != before;
            let retired_tts: Vec<String> = if changed && !before_tts.is_empty() {
                let after_tts = referenced_tts(&q);
                before_tts.into_iter().filter(|id| !after_tts.iter().any(|a| a == id)).collect()
            } else {
                Vec::new()
            };
            // Save while still holding the queue, so saves land in the order the changes
            // happened and an older state can never overwrite a newer one (R-31). An idle
            // tick changes nothing and writes nothing.
            if changed {
                let snapshot = q.clone();
                if let Err(err) = self.db(move |s| repo::rules::save_queue(s, &snapshot)).await {
                    tracing::error!(%err, "couldn't save the alert queue");
                }
            }
            (actions, changed, view(&q), retired_tts)
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
                        &Notice::new(
                            NoticeLevel::Info,
                            format!(
                                "{} queued alert{} went to the event list instead.",
                                alert_ids.len(),
                                if alert_ids.len() == 1 { "" } else { "s" }
                            ),
                        )
                        .key("alerts-folded"),
                    ));
                }
            }
        }
        if changed {
            self.send_live(LiveFrame::new("queue", &v));
        }
        if !retired_tts.is_empty() {
            // Delete retired ephemeral TTS files after notifying overlays so alert dispatch pays
            // zero extra latency.
            let _ = self
                .db(move |s| {
                    for id in retired_tts {
                        let _ = repo::media::delete(s, &id);
                    }
                    Ok(())
                })
                .await;
        }
        v
    }

    pub(crate) async fn enqueue_alert(&self, alert: RenderedAlert, priority: i32) {
        let speak = alert.tts_text.clone().map(|t| (alert.alert_id.clone(), t));
        self.queue_op(move |q, t| q.enqueue(alert, priority, t)).await;
        self.inner.queue_changed.notify_one();
        if let Some((alert_id, text)) = speak {
            // Read aloud: what it says can't be taken for the streamer's voice command.
            crate::studio::voice_commands::reading_aloud(&text, crate::now().0);
            let engine = self.clone();
            tokio::spawn(async move {
                if let Some(media) = engine.speak_to_media(&text).await {
                    let attached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let attached_flag = attached.clone();
                    let media_for_op = media.clone();
                    engine
                        .queue_op(move |q, _| {
                            if q.set_tts_audio(&alert_id, &media_for_op) {
                                attached_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                            vec![]
                        })
                        .await;
                    if !attached.load(std::sync::atomic::Ordering::Relaxed) {
                        // Alert already started/finished/dropped before synthesis finished:
                        // delete the orphaned TTS audio file instead of leaking it on disk.
                        let _ = engine.db(move |s| Ok(repo::media::delete(s, &media).ok())).await;
                    }
                }
            });
        }
    }

    /// Synthesises `text` into the media library (a Ferrocast voice with Pro, else this
    /// computer's, see `speech::cloud`); `None` if neither can (the overlay then uses the
    /// browser's voice).
    pub(crate) async fn speak_to_media(&self, text: &str) -> Option<String> {
        self.speak_line(text, None).await.map(|s| s.media_id)
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

#[cfg(test)]
mod tts_cleanup_tests {
    use std::sync::Arc;

    use ferrocast_core::alert::RenderedAlert;
    use ferrocast_core::event::Platform;
    use ferrocast_core::queue::AckPhase;
    use ferrocast_store::repo;

    use crate::Engine;

    const WAV: &[u8] = b"RIFF\0\0\0\0WAVEfmt ";

    fn sample_alert(id: &str, tts_media: Option<String>) -> RenderedAlert {
        RenderedAlert {
            alert_id: id.into(),
            event_id: format!("ev-{id}"),
            rule_id: "r1".into(),
            variation_id: "v1".into(),
            kind: "subscription".into(),
            platform: Platform::Twitch,
            title: "Sub".into(),
            body: "hi".into(),
            image: None,
            sound: None,
            volume: 100,
            duration_ms: 3_000,
            layout: "above".into(),
            css: String::new(),
            tts_text: Some("hi".into()),
            tts_audio: tts_media,
            test: false,
            design: None,
        }
    }

    #[tokio::test]
    async fn tts_media_is_deleted_when_alerts_finish_skip_or_clear() {
        let dir = tempfile::tempdir().unwrap();
        let secrets: Arc<dyn ferrocast_secrets::Secrets> = Arc::new(ferrocast_secrets::MemorySecrets::default());
        let engine = Engine::start_with_secrets(dir.path(), secrets).await.unwrap();
        let store = engine.store();
        let m1 = repo::media::save(&store, "speech.wav", WAV).unwrap();
        let m2 = repo::media::save(&store, "speech.ogg", b"OggS\0\0\0\0").unwrap();
        let m3 = repo::media::save(&store, "speech.wav", WAV).unwrap();

        // Pause so we can queue 3 alerts with attached TTS media, then drive each exit path.
        engine.queue_pause().await;
        for (id, mid) in [("a1", &m1.id), ("a2", &m2.id), ("a3", &m3.id)] {
            let a = sample_alert(id, Some(mid.clone()));
            engine.queue_op(move |q, t| q.enqueue(a, 0, t)).await;
        }
        assert_eq!(repo::media::list(&store).unwrap().len(), 3);

        // Resume starts a1; its TTS stays on disk while playing.
        engine.queue_resume().await;
        assert!(repo::media::get(&store, &m1.id).unwrap().is_some());

        // Ack Finished on a1 -> m1 deleted immediately; a2 & a3 remain.
        engine.ack("a1", AckPhase::Finished).await;
        assert!(repo::media::get(&store, &m1.id).unwrap().is_none(), "finished alert TTS deleted");
        assert!(repo::media::get(&store, &m2.id).unwrap().is_some());

        // Clear waiting -> remaining waiting alerts' TTS deleted immediately.
        engine.queue_clear().await;
        assert!(repo::media::list(&store).unwrap().is_empty(), "cleared alerts' TTS deleted");
    }
}
