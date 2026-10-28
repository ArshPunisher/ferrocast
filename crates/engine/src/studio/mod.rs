//! Studio: the streamer's production tools, all running on their own PC (Free, R-05).
//!
//! - [`chatbot`]: counters (`!deaths +`), quotes (`!quote add`) and the built-in modules
//!   (8ball, roulette, duel, dice, followage, watchtime, uptime, game/title, shoutout).
//! - [`stream_info`]: one title/category/tags/language form pushed to every platform, with
//!   presets and history; partial success is normal and reported per platform.
//! - [`show`]: "Start show" (preset, Starting Soon, countdown to the live scene, alerts,
//!   Discord, bot, replay buffer) with pre-flight checks, and "End show".
//! - [`obs_tools`]: scene switching (dashboard, `!scene` from mods with an allow-list,
//!   BRB and ads), source visibility, the replay buffer ("clip that") and OBS health.
//! - [`moments`]: the "Don't miss" lane on Home and stream markers with CSV/TXT export.
//!
//! - [`autoreplies`]: the bot answers chat that mentions something ("discord" → the link).
//!
//! The pipeline calls three hooks: [`Engine::answer_studio_command`] for chat commands,
//! [`Engine::answer_autoreply`] for other chat, and [`Engine::studio_saw`] for every event. Settings live as JSON in `settings` (keys below);
//! counter values, quotes, markers and watch time have tables.

pub mod autoreplies;
pub mod chatbot;
pub mod moments;
pub mod obs_tools;
pub(crate) mod rig;
pub mod show;
pub mod stream_info;

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use ferrocast_core::chatbot::{CounterDef, Duels, ModulesConfig};
use ferrocast_core::event::{Event, Platform};
use ferrocast_core::moments::{MomentsConfig, SpikeMeter};
use ferrocast_core::show::{Health, HealthSample, ScenesConfig, ShowConfig};
use ferrocast_store::repo;
use tokio::sync::Notify;

use crate::frames::LiveFrame;
use crate::{Engine, EngineError, now};

pub(crate) const COUNTERS_KEY: &str = "studio_counters";
pub(crate) const MODULES_KEY: &str = "studio_modules";
pub(crate) const SCENES_KEY: &str = "studio_scenes";
pub(crate) const MOMENTS_KEY: &str = "studio_moments";
pub(crate) const SHOW_KEY: &str = "studio_show";
pub(crate) const STREAM_INFO_KEY: &str = "stream_info";

/// How often watch time is credited (minutes for everyone who chatted meanwhile).
const WATCH_TICK: Duration = Duration::from_secs(300);

/// Studio's runtime state, one per engine.
#[derive(Debug, Default)]
pub(crate) struct Studio {
    pub counters: Mutex<BTreeMap<String, i64>>,
    pub counter_defs: RwLock<Vec<CounterDef>>,
    pub modules: RwLock<ModulesConfig>,
    pub scenes: RwLock<ScenesConfig>,
    pub moments: RwLock<MomentsConfig>,
    pub show_cfg: RwLock<ShowConfig>,
    pub duels: Mutex<Duels>,
    /// Last use of a command, by `word:platform:user` (and `word` for the global one).
    pub cooldowns: Mutex<ferrocast_core::chatbot::KeyedCooldowns>,
    /// Accounts that chatted since the last watch-time tick.
    pub watching: Mutex<HashSet<(Platform, String)>>,
    pub lane: Mutex<VecDeque<moments::LaneItem>>,
    pub spike: Mutex<SpikeMeter>,
    pub show: Mutex<Option<show::ShowRun>>,
    /// Wakes the countdown when a show starts, is skipped or cancelled.
    pub show_wake: Notify,
    /// The last OBS reading and what it means.
    pub health: Mutex<Option<(HealthSample, Health)>>,
    /// The scene to go back to after BRB.
    pub brb_from: Mutex<Option<String>>,
    /// One health watcher per engine.
    pub health_watching: std::sync::atomic::AtomicBool,
    /// Auto-replies: the rules, compiled, their cooldowns and the send budget.
    pub autoreplies: autoreplies::AutoReplies,
}

impl Engine {
    pub(crate) fn studio(&self) -> &Studio {
        &self.inner.studio
    }

    /// Loads Studio's settings and counter values. A setting that no longer parses keeps
    /// its default here and is logged (startup must not fail over a tool setting).
    pub(crate) async fn load_studio(&self) {
        use repo::settings::get_or_default;
        let loaded = self
            .db(|s| {
                Ok((
                    get_or_default::<Vec<CounterDef>>(s, COUNTERS_KEY),
                    get_or_default::<ModulesConfig>(s, MODULES_KEY),
                    get_or_default::<ScenesConfig>(s, SCENES_KEY),
                    get_or_default::<MomentsConfig>(s, MOMENTS_KEY),
                    get_or_default::<ShowConfig>(s, SHOW_KEY),
                    repo::studio::counter_values(s),
                ))
            })
            .await;
        let Ok((defs, modules, scenes, moments, show, values)) = loaded else { return };
        let st = self.studio();
        let ok = |what: &str, e: &ferrocast_store::StoreError| tracing::error!(error = %e, what, "a Studio setting couldn't be read");
        match defs {
            Ok(v) => *st.counter_defs.write().unwrap_or_else(|p| p.into_inner()) = v,
            Err(e) => ok("counters", &e),
        }
        match modules {
            Ok(v) => *st.modules.write().unwrap_or_else(|p| p.into_inner()) = v,
            Err(e) => ok("modules", &e),
        }
        match scenes {
            Ok(v) => *st.scenes.write().unwrap_or_else(|p| p.into_inner()) = v,
            Err(e) => ok("scenes", &e),
        }
        match moments {
            Ok(v) => *st.moments.write().unwrap_or_else(|p| p.into_inner()) = v,
            Err(e) => ok("moments", &e),
        }
        match show {
            Ok(v) => *st.show_cfg.write().unwrap_or_else(|p| p.into_inner()) = v,
            Err(e) => ok("show", &e),
        }
        if let Ok(v) = values {
            *st.counters.lock().unwrap_or_else(|p| p.into_inner()) = v.into_iter().collect();
        }
        self.load_autoreplies().await;
    }

    /// Every event, after the pipeline handled it: the "Don't miss" lane, markers, chat
    /// spikes, clips on big moments, BRB during ads and watch time.
    pub(crate) async fn studio_saw(&self, e: &Event, first_message: bool) {
        self.moments_saw(e, first_message).await;
        self.ads_saw(e).await;
        if let ferrocast_core::event::EventKind::ChatMessage { actor, .. } = &e.kind
            && e.origin == ferrocast_core::event::Origin::Live
            && e.channel.is_none()
            && let Some(id) = &actor.id
        {
            let live = self.state().live.get(&e.platform).copied().unwrap_or(false);
            if live {
                let mut w = self.studio().watching.lock().unwrap_or_else(|p| p.into_inner());
                if w.len() < 100_000 {
                    w.insert((e.platform, id.clone()));
                }
            }
        }
    }

    /// Whether `key` (a command and who used it) is past its cooldown; records the use.
    pub(crate) fn studio_cooldown(&self, key: &str, seconds: i64) -> bool {
        self.studio().cooldowns.lock().unwrap_or_else(|p| p.into_inner()).take(key, seconds, now().0)
    }

    /// Replies in the chat a command came from (as a reply where the platform has them).
    pub(crate) fn studio_reply(&self, e: &Event, text: &str) {
        let reply = ferrocast_core::command::safe_reply(text);
        if reply.is_empty() {
            return;
        }
        let reply_to = e.source_id.clone();
        self.spawn_action(e.platform, "answer a command", move |c| {
            Box::pin(async move { c.send_chat(&reply, reply_to.as_deref()).await })
        });
    }

    /// Sends a Studio frame to every dashboard.
    pub(crate) fn studio_frame(&self, kind: &str, data: &impl serde::Serialize) {
        self.send_live(LiveFrame::new(kind, data));
    }

    /// Opens a connection to OBS with the details in use (Settings → OBS). 422 with what
    /// to do when OBS can't be reached.
    pub(crate) async fn obs_client(&self) -> Result<ferrocast_obs::Client, EngineError> {
        let target = self.obs_target().await?;
        ferrocast_obs::connect(&target.cfg, target.password.as_deref()).await.map_err(obs_error)
    }

    /// When the current stream started (any platform live), for markers and recaps.
    pub(crate) fn stream_started_at(&self) -> Option<i64> {
        let st = self.state();
        st.live.values().any(|l| *l).then_some(()).and(st.session.started_at).map(|m| m.0)
    }
}

/// OBS's failures as answers the dashboard can show: 422 with a hint.
pub(crate) fn obs_error(e: ferrocast_obs::ObsError) -> EngineError {
    use ferrocast_obs::ObsError;
    let (code, hint) = match &e {
        ObsError::NotReachable => ("obs_unreachable", "Open OBS on this computer, then check Settings, OBS."),
        ObsError::WrongPassword => ("obs_password", "Open Settings, OBS and use the details OBS shows."),
        ObsError::Refused(m) if m.contains("stream key") => {
            ("obs_no_stream_key", "In OBS open Settings, Stream and connect your account or paste the stream key.")
        }
        ObsError::Refused(_) => ("obs_refused", "Check the name in OBS, or pick it again in Studio."),
    };
    EngineError::Refused { status: 422, code, message: e.to_string(), hint: hint.into(), retry_after: None }
}

/// Saves one Studio setting as JSON.
pub(crate) async fn save_setting<T: serde::Serialize + Send + Sync + Clone + 'static>(
    engine: &Engine,
    key: &'static str,
    value: &T,
) -> Result<(), EngineError> {
    let v = value.clone();
    engine.db(move |s| repo::settings::set(s, key, &v)).await
}

/// Background work: watch time every 5 minutes and the show countdown.
pub(crate) async fn run(engine: Engine) {
    tokio::spawn(show::countdown_loop(engine.clone()));
    loop {
        tokio::time::sleep(WATCH_TICK).await;
        engine.credit_watch_time(5).await;
    }
}

impl Engine {
    /// Adds `minutes` for everyone who chatted while live since the last tick.
    pub(crate) async fn credit_watch_time(&self, minutes: i64) -> usize {
        let who: Vec<(Platform, String)> =
            std::mem::take(&mut *self.studio().watching.lock().unwrap_or_else(|p| p.into_inner()))
                .into_iter()
                .collect();
        if who.is_empty() {
            return 0;
        }
        match self.db(move |s| repo::studio::add_watch_time(s, &who, minutes)).await {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(%err, "couldn't save watch time");
                0
            }
        }
    }
}

#[cfg(test)]
mod tests;
