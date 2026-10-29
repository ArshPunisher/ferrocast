//! Moments (API.md §4 "Studio: moments"): the "Don't miss" lane pinned on Home (big tips,
//! raids, big gifts, first-time chatters; the dashboard plays a sound only the streamer
//! hears) and markers on the stream's timeline (big moments, chat spikes, clips, manual)
//! that export as CSV or text for editing and show in the recap.

use ferrocast_core::event::{Event, EventKind, Platform};
use ferrocast_core::moments::{self, Marker, MomentKind, MomentsConfig};
use ferrocast_store::repo;
use serde::Serialize;

use super::{MOMENTS_KEY, save_setting};
use crate::{Engine, EngineError, now};

/// The lane keeps this many (oldest drop off; dismissed ones go at once).
const LANE_KEEP: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneItem {
    pub id: String,
    pub kind: MomentKind,
    pub title: String,
    pub detail: Option<String>,
    pub platform: Platform,
    pub event_id: String,
    pub at: i64,
}

const LANE_NOT_FOUND: EngineError = EngineError::Missing {
    code: "lane_item_not_found",
    message: "That moment isn't pinned any more.",
    hint: "It was dismissed already, maybe from another dashboard.",
};

const MARKER_NOT_FOUND: EngineError = EngineError::Missing {
    code: "marker_not_found",
    message: "That marker doesn't exist.",
    hint: "Reload the list; it may have been deleted.",
};

impl Engine {
    pub fn moments_config(&self) -> MomentsConfig {
        self.studio().moments.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub async fn patch_moments_config(
        &self,
        patch: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<MomentsConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let cfg = crate::patch::merge(&self.moments_config(), patch)?;
        cfg.validate().map_err(EngineError::Invalid)?;
        save_setting(self, MOMENTS_KEY, &cfg).await?;
        *self.studio().moments.write().unwrap_or_else(|p| p.into_inner()) = cfg.clone();
        Ok(cfg)
    }

    /// The lane, newest first.
    pub fn lane(&self) -> Vec<LaneItem> {
        self.studio().lane.lock().unwrap_or_else(|p| p.into_inner()).iter().rev().cloned().collect()
    }

    pub fn dismiss_lane(&self, id: &str) -> Result<Vec<LaneItem>, EngineError> {
        {
            let mut lane = self.studio().lane.lock().unwrap_or_else(|p| p.into_inner());
            let before = lane.len();
            lane.retain(|i| i.id != id);
            if lane.len() == before {
                return Err(LANE_NOT_FOUND);
            }
        }
        self.push_lane(None);
        Ok(self.lane())
    }

    pub fn dismiss_all_lane(&self) -> Vec<LaneItem> {
        self.studio().lane.lock().unwrap_or_else(|p| p.into_inner()).clear();
        self.push_lane(None);
        Vec::new()
    }

    /// `lane { items, added? }`: `added` is the new one (the dashboard plays the sound).
    fn push_lane(&self, added: Option<&LaneItem>) {
        let cfg = self.moments_config();
        self.studio_frame(
            "lane",
            &serde_json::json!({ "items": self.lane(), "added": added, "sound": cfg.sound, "volume": cfg.volume }),
        );
    }

    /// A marker now, tied to the stream that's live (if any).
    pub async fn add_marker_now(
        &self,
        kind: MomentKind,
        label: &str,
        platform: Option<Platform>,
        clip: bool,
    ) -> Result<Marker, EngineError> {
        let start = self.stream_started_at();
        let l = label.to_string();
        let m = self.db(move |s| repo::studio::add_marker(s, kind, &l, platform, start, clip)).await?;
        self.studio_frame("marker", &m);
        Ok(m)
    }

    /// Markers since `since` (default: this stream's start, or the last 24 hours).
    pub async fn markers(&self, since: Option<i64>) -> Result<Vec<Marker>, EngineError> {
        if since.is_some_and(|s| s < 0 || s > now().0) {
            return Err(EngineError::Invalid("since: a time in the past, in Unix milliseconds.".into()));
        }
        let since = since.or_else(|| self.stream_started_at()).unwrap_or(now().0 - 86_400_000);
        self.db(move |s| repo::studio::markers(s, since, 5_000)).await
    }

    pub async fn delete_marker(&self, id: i64) -> Result<(), EngineError> {
        match self.db(move |s| repo::studio::delete_marker(s, id)).await {
            Err(EngineError::Store(ferrocast_store::StoreError::NotFound(_))) => Err(MARKER_NOT_FOUND),
            other => other,
        }
    }

    /// The markers as a file: (`text/csv` or `text/plain`, file name, body).
    pub async fn export_markers(
        &self,
        format: &str,
        since: Option<i64>,
    ) -> Result<(&'static str, String, String), EngineError> {
        let list = self.markers(since).await?;
        let (y, m, d) = ferrocast_core::chatbot::ymd(now().0);
        match format {
            "csv" => Ok((
                "text/csv; charset=utf-8",
                format!("ferrocast-markers-{y:04}-{m:02}-{d:02}.csv"),
                moments::to_csv(&list),
            )),
            "txt" => Ok((
                "text/plain; charset=utf-8",
                format!("ferrocast-markers-{y:04}-{m:02}-{d:02}.txt"),
                moments::to_txt(&list),
            )),
            _ => Err(EngineError::Invalid("format: csv or txt.".into())),
        }
    }

    /// Big moments go in the lane and on the timeline; chat spikes on the timeline.
    pub(crate) async fn moments_saw(&self, e: &Event, first_message: bool) {
        let cfg = self.moments_config();
        let currency = self.settings().currency;
        if let Some(m) = moments::classify(&cfg, &currency, e, first_message) {
            let item = LaneItem {
                id: ferrocast_store::new_id(),
                kind: m.kind,
                title: m.title.clone(),
                detail: m.detail.clone(),
                platform: m.platform,
                event_id: e.id.clone(),
                at: now().0,
            };
            {
                let mut lane = self.studio().lane.lock().unwrap_or_else(|p| p.into_inner());
                lane.push_back(item.clone());
                while lane.len() > LANE_KEEP {
                    lane.pop_front();
                }
            }
            self.push_lane(Some(&item));
            if m.mark && cfg.markers {
                self.big_moment_marker(m.kind, &m.title, m.platform, &cfg).await;
            }
        }
        if let EventKind::ChatMessage { .. } = &e.kind
            && e.origin == ferrocast_core::event::Origin::Live
            && cfg.markers
            && self.stream_started_at().is_some()
        {
            let spike = self.studio().spike.lock().unwrap_or_else(|p| p.into_inner()).message(
                now().0,
                cfg.spike_factor,
                cfg.spike_min,
            );
            if let Some(n) = spike {
                let label = format!("Chat spike: {n} messages in 30 seconds");
                self.big_moment_marker(MomentKind::ChatSpike, &label, e.platform, &cfg).await;
            }
        }
    }

    /// A marker for a big moment, a replay save when the streamer wants clips of them, and
    /// a Twitch stream marker when asked. In the background: the pipeline never waits on OBS.
    async fn big_moment_marker(&self, kind: MomentKind, label: &str, platform: Platform, cfg: &MomentsConfig) {
        let clip = self.scenes_config().clip_on_big_moments;
        let marker = match self.add_marker_now(kind, label, Some(platform), false).await {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(%err, "couldn't save a marker");
                return;
            }
        };
        if clip {
            let engine = self.clone();
            let id = marker.id;
            tokio::spawn(async move {
                let Ok(rig) = engine.rig().await else { return };
                if let Ok(path) = engine.rig_save_replay(&rig).await {
                    engine.vertical_copy(path.as_deref());
                    if let Err(err) = engine.db(move |s| repo::studio::mark_clipped(s, id)).await {
                        tracing::warn!(%err, "couldn't mark a marker clipped");
                    }
                }
            });
        }
        if cfg.twitch_markers
            && let Some(c) = self.connector(Platform::Twitch)
        {
            let d = label.to_string();
            tokio::spawn(async move {
                if let Err(err) = c.create_marker(&d).await {
                    tracing::debug!(%err, "no Twitch marker");
                }
            });
        }
    }

    pub async fn save_moments_config(&self, cfg: MomentsConfig) -> Result<MomentsConfig, EngineError> {
        cfg.validate().map_err(EngineError::Invalid)?;
        save_setting(self, MOMENTS_KEY, &cfg).await?;
        *self.studio().moments.write().unwrap_or_else(|p| p.into_inner()) = cfg.clone();
        Ok(cfg)
    }
}
