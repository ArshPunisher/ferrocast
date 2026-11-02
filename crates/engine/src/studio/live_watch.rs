//! The live watch: every two seconds while the Ferrocast Studio streams, a look at what goes out
//! (the show's mics and cameras, Go live's destinations), fed to `ferrocast_core::show::LiveWatch`;
//! what it raises or clears goes to the dashboard (`live_alert`) and the log, and a black camera
//! can put the show on BRB by itself (`ScenesConfig::brb_when_black`).

use std::sync::atomic::Ordering;
use std::time::Duration;

use ferrocast_core::show::{LiveAlert, WatchSample};
use serde_json::Value;

use crate::Engine;
use crate::now;
use crate::studio_host::StudioAct;

/// How often the watch looks.
const EVERY: Duration = Duration::from_secs(2);

impl Engine {
    /// Starts the live watch's loop (once per engine). It looks only while the Studio streams.
    pub fn watch_live(&self) {
        if self.studio().live_watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let engine = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(EVERY);
            let mut was_live = false;
            loop {
                tick.tick().await;
                let live = engine.studio_streaming().await;
                if live != was_live {
                    // A new stream: everything starts fresh.
                    engine.studio().live_watch.lock().unwrap_or_else(|p| p.into_inner()).reset();
                    engine.studio().auto_brb.lock().unwrap_or_else(|p| p.into_inner()).clear();
                    *engine.studio().scene_now.lock().unwrap_or_else(|p| p.into_inner()) = None;
                    *engine.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner()) = Default::default();
                    *engine.studio().live_health.lock().unwrap_or_else(|p| p.into_inner()) = Default::default();
                    was_live = live;
                }
                if live {
                    engine.scene_chapter_tick().await;
                    engine.auto_quality_tick().await;
                    engine.health_tick().await;
                    engine.live_watch_tick().await;
                }
            }
        });
    }

    /// Whether the Studio is streaming now.
    async fn studio_streaming(&self) -> bool {
        let Ok(host) = self.studio_host() else { return false };
        host.stream_stats().await.ok().is_some_and(|v| v["active"].as_bool() == Some(true))
    }

    /// The scene on air, when it changed: a chapter marker (the first one starts the stream's).
    pub(crate) async fn scene_chapter_tick(&self) {
        if !self.moments_config().markers {
            return;
        }
        let Ok(v) = self.studio_act(StudioAct::Scenes).await else { return };
        let Some(scene) = v["current"].as_str().filter(|s| !s.is_empty()).map(String::from) else { return };
        {
            let mut now = self.studio().scene_now.lock().unwrap_or_else(|p| p.into_inner());
            if now.as_deref() == Some(scene.as_str()) {
                return;
            }
            *now = Some(scene.clone());
        }
        if let Err(e) = self.add_marker_now(ferrocast_core::moments::MomentKind::Scene, &scene, None, false).await {
            tracing::warn!(error = %e, "couldn't mark a scene change");
        }
    }

    /// Auto quality: trouble getting the stream out lowers its bitrate a step, calm raises it
    /// back (when Go live's switch is on).
    pub(crate) async fn auto_quality_tick(&self) {
        if !self.golive_config().auto_quality {
            return;
        }
        let Ok(host) = self.studio_host() else { return };
        let enc = host.stream_stats().await.ok().and_then(|v| v["dropped"].as_u64()).unwrap_or(0);
        let (net, paused) = self.golive_trouble();
        let s = ferrocast_core::auto_quality::Signals { net_dropped: net, enc_dropped: enc, paused };
        let changed = self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner()).tick(now().0, s);
        let Some(pct) = changed else { return };
        if let Err(e) = host.set_live_quality(pct).await {
            tracing::warn!(error = %e, "auto quality couldn't change the bitrate");
            return;
        }
        let (message, cleared) = if pct >= 100 {
            ("Your stream is back to full quality.".to_string(), true)
        } else {
            (
                format!(
                    "Your upload struggled: the stream's quality is at {pct}% to stay smooth. It goes back up by itself."
                ),
                false,
            )
        };
        self.studio_frame("live_alert", &LiveAlert { id: "auto_quality".into(), level: "warn", message, cleared });
    }

    /// The stream's health: the engine's and the network's counters into the score.
    pub(crate) async fn health_tick(&self) {
        let Ok(host) = self.studio_host() else { return };
        let Ok(v) = host.stats().await else { return };
        let (e, s) = (&v["engine"], &v["stream"]);
        let n = |x: &Value| x.as_u64().unwrap_or(0);
        let (net_dropped, paused) = self.golive_trouble();
        let sample = ferrocast_core::health::Sample {
            frames: n(&e["frames"]),
            lagged: n(&e["lagged"]),
            encoded: n(&e["encoded"]),
            skipped: n(&e["skipped"]),
            sent: n(&s["frames"]),
            dropped: n(&s["dropped"]),
            net_dropped,
            reconnecting: s["reconnecting"].as_bool() == Some(true),
            paused,
            quality_pct: self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner()).pct(),
            cpu: e["cpu"].as_f64().unwrap_or(0.0),
        };
        let mut h = self.studio().live_health.lock().unwrap_or_else(|p| p.into_inner());
        let now = h.0.push(sample);
        h.1 = Some(now);
    }

    /// Auto quality turned off: a lowered stream goes back to full quality.
    pub(crate) async fn auto_quality_off(&self) {
        let lowered = {
            let mut aq = self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner());
            std::mem::take(&mut *aq).pct() < 100
        };
        if !lowered {
            return;
        }
        if let Ok(host) = self.studio_host()
            && let Err(e) = host.set_live_quality(100).await
        {
            tracing::warn!(error = %e, "auto quality couldn't put the bitrate back");
        }
        self.studio_frame(
            "live_alert",
            &LiveAlert {
                id: "auto_quality".into(),
                level: "warn",
                message: "Your stream is back to full quality.".into(),
                cleared: true,
            },
        );
    }

    /// One look → the alerts it raises or clears, sent on.
    pub(crate) async fn live_watch_tick(&self) -> Vec<LiveAlert> {
        let cfg = self.show_config().watch;
        if !cfg.on {
            return Vec::new();
        }
        let show = self.show_config();
        let mut sample = WatchSample { at: now().0, ..Default::default() };
        if !show.mic_sources.is_empty()
            && let Ok(v) = self.studio_act(StudioAct::Levels { names: show.mic_sources.clone() }).await
        {
            for m in &show.mic_sources {
                let peak =
                    v[m]["peak"].as_array().into_iter().flatten().filter_map(Value::as_f64).fold(f64::MIN, f64::max);
                if peak > f64::MIN {
                    sample.mics.push((m.clone(), peak as f32));
                }
            }
        }
        if !show.cam_sources.is_empty()
            && let Ok(v) = self.studio_act(StudioAct::Look { names: show.cam_sources.clone() }).await
        {
            for c in &show.cam_sources {
                let (Some(luma), Some(spread), Some(sig)) =
                    (v[c]["luma"].as_u64(), v[c]["spread"].as_u64(), v[c]["sig"].as_u64())
                else {
                    continue;
                };
                sample.cameras.push((c.clone(), luma < 12 && spread < 6, sig));
            }
        }
        sample.destinations = self.golive_sending();
        let alerts = self.studio().live_watch.lock().unwrap_or_else(|p| p.into_inner()).tick(&cfg, &sample);
        for a in &alerts {
            tracing::info!(id = %a.id, cleared = a.cleared, "live watch: {}", a.message);
            self.studio_frame("live_alert", a);
        }
        self.brb_when_black(&alerts).await;
        alerts
    }

    /// BRB by itself while a watched camera is black (when asked), back once none is.
    pub(crate) async fn brb_when_black(&self, alerts: &[LiveAlert]) {
        if !self.scenes_config().brb_when_black {
            return;
        }
        let (was, now) = {
            let mut black = self.studio().auto_brb.lock().unwrap_or_else(|p| p.into_inner());
            let was = !black.is_empty();
            for a in alerts.iter().filter(|a| a.id.ends_with(":black")) {
                if a.cleared {
                    black.remove(&a.id);
                } else {
                    black.insert(a.id.clone());
                }
            }
            (was, !black.is_empty())
        };
        if was != now {
            tracing::info!(on = now, "live watch: BRB by itself (a camera went black, or came back)");
            if let Err(e) = self.set_brb(now).await {
                tracing::warn!(error = %e, "live watch: couldn't switch BRB");
            }
        }
    }

    /// The live watch's alerts raised now (their ids).
    pub fn live_alerts(&self) -> Vec<String> {
        self.studio().live_watch.lock().unwrap_or_else(|p| p.into_inner()).raised()
    }
}
