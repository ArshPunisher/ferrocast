//! Scene tools (API.md §4 "Studio: OBS"): OBS's scenes and sources from the dashboard, `!scene`
//! for mods (only scenes on the allow-list), BRB by button, `!brb` or during Twitch ads,
//! "clip that" (the replay buffer, from the dashboard, `!clip` or big moments) and the
//! health watch (dropped frames, encoder and render lag, bitrate, CPU, disk) for Home.

use std::sync::atomic::Ordering;
use std::time::Duration;

use ferrocast_core::event::{Actor, Event, EventKind, Origin};
use ferrocast_core::moments::MomentKind;
use ferrocast_core::show::ScenesConfig;
use serde::Serialize;

use super::{SCENES_KEY, obs_error, save_setting};
use crate::{Engine, EngineError, now};

/// How often OBS is asked for its numbers while connected.
const HEALTH_EVERY: Duration = Duration::from_secs(5);
/// `!clip` at most this often (a chat full of mods can't fill the disk).
const CLIP_COOLDOWN_S: i64 = 20;

#[derive(Debug, Clone, Serialize)]
pub struct ClipResult {
    pub saved: bool,
    pub path: Option<String>,
    pub marker: ferrocast_core::moments::Marker,
}

impl Engine {
    pub fn scenes_config(&self) -> ScenesConfig {
        self.studio().scenes.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub async fn patch_scenes_config(
        &self,
        patch: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<ScenesConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut cfg = crate::patch::merge(&self.scenes_config(), patch)?;
        if cfg.brb_scene.as_deref().is_some_and(|s| s.trim().is_empty()) {
            cfg.brb_scene = None;
        }
        cfg.validate().map_err(EngineError::Invalid)?;
        // Vertical clips need ffmpeg: fetched now, in the background, when they're switched on.
        if cfg.vertical_clips && !self.scenes_config().vertical_clips && self.ffmpeg_path().is_none() {
            let engine = self.clone();
            tokio::spawn(async move {
                if let Err(err) = engine.get_ffmpeg().await {
                    tracing::warn!(%err, "ffmpeg couldn't be fetched for vertical clips");
                }
            });
        }
        save_setting(self, SCENES_KEY, &cfg).await?;
        *self.studio().scenes.write().unwrap_or_else(|p| p.into_inner()) = cfg.clone();
        Ok(cfg)
    }

    /// Every scene with its sources and whether each is shown.
    pub async fn obs_scenes(&self) -> Result<ferrocast_obs::control::Scenes, EngineError> {
        let c = self.obs_client().await?;
        ferrocast_obs::control::scenes(&c).await.map_err(obs_error)
    }

    /// OBS's inputs (name, kind), for picking the mic and camera.
    pub async fn obs_inputs(&self) -> Result<Vec<(String, String)>, EngineError> {
        let c = self.obs_client().await?;
        ferrocast_obs::control::inputs(&c).await.map_err(obs_error)
    }

    pub async fn switch_scene(&self, scene: &str) -> Result<(), EngineError> {
        if !ferrocast_core::show::valid_scene_name(scene) {
            return Err(EngineError::Invalid("scene: a scene name of 1 to 100 characters.".into()));
        }
        let c = self.obs_client().await?;
        ferrocast_obs::control::switch_scene(&c, scene).await.map_err(obs_error)
    }

    /// A scene switch from chat or a rule: in the Studio while it runs, else in OBS.
    pub(crate) async fn switch_scene_anywhere(&self, scene: &str) -> Result<(), EngineError> {
        if !ferrocast_core::show::valid_scene_name(scene) {
            return Err(EngineError::Invalid("scene: a scene name of 1 to 100 characters.".into()));
        }
        let rig = self.rig().await?;
        self.rig_switch(&rig, scene).await
    }

    pub async fn set_source_visible(&self, scene: &str, item_id: i64, visible: bool) -> Result<(), EngineError> {
        let c = self.obs_client().await?;
        ferrocast_obs::control::set_visible(&c, scene, item_id, visible).await.map_err(obs_error)
    }

    pub async fn set_input_muted(&self, input: &str, muted: bool) -> Result<(), EngineError> {
        let c = self.obs_client().await?;
        ferrocast_obs::control::set_muted(&c, input, muted).await.map_err(obs_error)
    }

    /// "Clip that": saves the replay buffer (the Studio's, or OBS's) and puts a marker on the
    /// timeline.
    pub async fn clip_that(&self, label: Option<&str>) -> Result<ClipResult, EngineError> {
        let rig = self.rig().await?;
        let path = self.rig_save_replay(&rig).await?;
        let label = label.map(str::trim).filter(|l| !l.is_empty()).unwrap_or("Clip saved").to_string();
        let marker = self.add_marker_now(MomentKind::Clip, &label, None, true).await?;
        self.vertical_copy(path.as_deref());
        Ok(ClipResult { saved: true, path, marker })
    }

    /// BRB on: remember the scene, switch to the BRB scene and mute the listed inputs.
    /// BRB off: back to the remembered scene (or the show's live scene) and unmute.
    pub async fn set_brb(&self, on: bool) -> Result<serde_json::Value, EngineError> {
        let cfg = self.scenes_config();
        let brb = cfg.brb_scene.clone().ok_or_else(|| EngineError::Refused {
            status: 422,
            code: "no_brb_scene",
            message: "No BRB scene is picked.".into(),
            hint: "Pick one in Studio, Show.".into(),
            retry_after: None,
        })?;
        let rig = self.rig().await?;
        if on {
            let (current, _) = self.rig_scenes(&rig).await?;
            self.rig_switch(&rig, &brb).await?;
            if current.as_deref() != Some(brb.as_str()) {
                *self.studio().brb_from.lock().unwrap_or_else(|p| p.into_inner()) = current;
            }
            for m in &cfg.brb_mute {
                if let Err(e) = self.rig_set_muted(&rig, m, true).await {
                    tracing::warn!(error = %e, input = %m, "couldn't mute for BRB");
                }
            }
            self.studio_frame("brb", &serde_json::json!({ "on": true, "scene": brb }));
            Ok(serde_json::json!({ "on": true, "scene": brb }))
        } else {
            let back = self
                .studio()
                .brb_from
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
                .or_else(|| self.show_config().live_scene)
                .ok_or_else(|| EngineError::Refused {
                    status: 422,
                    code: "no_return_scene",
                    message: "Ferrocast doesn't know which scene to go back to.".into(),
                    hint: "Switch the scene yourself, or pick a live scene in Studio, Show.".into(),
                    retry_after: None,
                })?;
            self.rig_switch(&rig, &back).await?;
            for m in &cfg.brb_mute {
                if let Err(e) = self.rig_set_muted(&rig, m, false).await {
                    tracing::warn!(error = %e, input = %m, "couldn't unmute after BRB");
                }
            }
            self.studio_frame("brb", &serde_json::json!({ "on": false, "scene": back }));
            Ok(serde_json::json!({ "on": false, "scene": back }))
        }
    }

    /// `!scene <name>`, `!brb`, `!back`, `!clip` from mods. Returns whether it was one.
    pub(crate) async fn obs_chat_command(
        &self,
        e: &Event,
        actor: &Actor,
        word: &str,
        rest: &str,
        is_mod: bool,
    ) -> bool {
        let cfg = self.scenes_config();
        // OBS can take seconds to answer (or time out): the work runs in the background so
        // the event pipeline never waits on it.
        let (engine, ev) = (self.clone(), e.clone());
        match word {
            "!scene" if cfg.chat_enabled => {
                if !is_mod {
                    return true;
                }
                let asked = rest.to_string();
                tokio::spawn(async move {
                    let reply = match cfg.chat_scene(&asked) {
                        Err(why) => why,
                        Ok(scene) => match engine.switch_scene_anywhere(scene).await {
                            Ok(()) => format!("Switched to {scene}."),
                            Err(err) => format!("Couldn't switch: {err}"),
                        },
                    };
                    engine.studio_reply(&ev, &reply);
                });
                true
            }
            "!brb" | "!back" if cfg.chat_enabled && cfg.brb_scene.is_some() => {
                if is_mod {
                    let on = word == "!brb";
                    tokio::spawn(async move {
                        if let Err(err) = engine.set_brb(on).await {
                            engine.studio_reply(&ev, &format!("Couldn't switch: {err}"));
                        }
                    });
                }
                true
            }
            "!clip" if cfg.clip_command => {
                if !is_mod || !self.studio_cooldown("!clip:*", CLIP_COOLDOWN_S) {
                    return true;
                }
                let label = format!("Clip by {}", actor.display_name);
                tokio::spawn(async move {
                    let reply = match engine.clip_that(Some(&label)).await {
                        Ok(_) => "Clipped! It's saved in the recordings folder.".to_string(),
                        Err(err) => format!("Couldn't clip: {err}"),
                    };
                    engine.studio_reply(&ev, &reply);
                });
                true
            }
            _ => false,
        }
    }

    /// BRB while a Twitch ad runs, back when it ends (in the background).
    pub(crate) async fn ads_saw(&self, e: &Event) {
        let EventKind::AdBreak { seconds } = e.kind else { return };
        let cfg = self.scenes_config();
        if e.origin != Origin::Live || !cfg.brb_on_ads || cfg.brb_scene.is_none() {
            return;
        }
        let engine = self.clone();
        let wait = Duration::from_secs(u64::from(seconds.clamp(15, 600)));
        tokio::spawn(async move {
            if let Err(err) = engine.set_brb(true).await {
                tracing::warn!(%err, "couldn't switch to BRB for an ad");
                return;
            }
            tokio::time::sleep(wait).await;
            if let Err(err) = engine.set_brb(false).await {
                tracing::warn!(%err, "couldn't switch back after an ad");
            }
        });
    }

    /// The last health reading (`None` before OBS answered once).
    pub fn obs_health(&self) -> Option<ferrocast_core::show::Health> {
        self.studio().health.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|(_, h)| h.clone())
    }

    /// Reads OBS's numbers once and updates the health (the watcher calls this every 5 s).
    pub async fn check_obs_health(&self) -> Result<ferrocast_core::show::Health, EngineError> {
        let c = self.obs_client().await?;
        self.check_health_with(&c).await
    }

    async fn check_health_with(&self, c: &ferrocast_obs::Client) -> Result<ferrocast_core::show::Health, EngineError> {
        let sample = ferrocast_obs::control::health_sample(c, now().0).await.map_err(obs_error)?;
        let prev = self.studio().health.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|(s, _)| s.clone());
        // OBS just started streaming: YouTube only looks for a live broadcast now and
        // then, so ask it to look now rather than up to ten minutes later.
        if sample.streaming && !prev.as_ref().is_some_and(|p| p.streaming) {
            self.wake_youtube();
        }
        let h = ferrocast_core::show::health(prev.as_ref(), &sample);
        *self.studio().health.lock().unwrap_or_else(|p| p.into_inner()) = Some((sample, h.clone()));
        self.studio_frame("obs_health", &h);
        Ok(h)
    }

    /// Keeps one connection to OBS and reads its health every 5 seconds while connected.
    /// Started with the real app (never in tests unless a test asks, with a fake OBS).
    pub fn watch_obs_health(&self) {
        if self.studio().health_watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let engine = self.clone();
        tokio::spawn(async move {
            let mut client: Option<ferrocast_obs::Client> = None;
            loop {
                let connected = engine.state().connectors.get("obs").is_some_and(|c| c.state == "connected");
                if !connected {
                    client = None;
                    *engine.studio().health.lock().unwrap_or_else(|p| p.into_inner()) = None;
                } else {
                    if client.is_none() {
                        client = engine.obs_client().await.ok();
                    }
                    if let Some(c) = &client
                        && engine.check_health_with(c).await.is_err()
                    {
                        client = None;
                    }
                }
                tokio::time::sleep(HEALTH_EVERY).await;
            }
        });
    }
}
