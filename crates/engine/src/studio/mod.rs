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
pub mod away;
pub mod chatbot;
pub mod clipboard;
pub mod encoder_bench;
pub(crate) mod game_scenes;
pub(crate) mod live_watch;
pub mod moments;
pub mod obs_tools;
pub mod push_hold;
pub(crate) mod record_watch;
pub(crate) mod rig;
pub(crate) mod rules;
pub mod show;
pub mod stream_info;
pub mod transcript;
pub mod vertical_clip;
pub(crate) mod voice_clip;
pub(crate) mod voice_commands;

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

/// Who currently holds BRB on air, so automatic releases (ads, away, black camera) never
/// pull the streamer off a manual BRB or another active hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BrbHold {
    Manual,
    AdBreak,
    Away,
    BlackCam,
}

/// Studio's runtime state, one per engine.
#[derive(Debug)]
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
    /// Single locked state machine for the Show, countdown, and delayed stream stop.
    pub machine: Mutex<show::BroadcastMachine>,
    ///
    /// Lock order, wherever more than one is taken: `auto_config_op`, then `output_lock`, then
    /// `output_gate` (innermost). Nothing waits for an outer lock while holding an inner one,
    /// and a start that a running start would only queue behind is refused from the
    /// `machine` state first (`Engine::refuse_if_show_busy`), before any of the three is awaited.
    /// Serializes stream start/stop, delayed stop, and Studio stop so no click order races.
    pub output_lock: tokio::sync::Mutex<()>,
    /// Mutual exclusion gate between starting any output and claiming a benchmark run (C2).
    pub output_gate: tokio::sync::Mutex<()>,
    /// Set once `request_update_restart` admits restart under `output_gate` so late recordings cannot start before exit.
    pub update_committing: std::sync::atomic::AtomicBool,
    /// Serializes `auto_config_benchmark` / `cancel` / `apply` / `undo` state transitions (C2/H7).
    pub auto_config_op: tokio::sync::Mutex<()>,
    /// Monotonic origin for Show timers (`away`, `live_watch`) so wall-clock jumps never fire them.
    pub mono_origin: std::time::Instant,
    /// Last monotonic sample timestamp seen by `live_watch_tick` (for wake-jump detection).
    pub last_watch_mono: Mutex<Option<i64>>,
    /// Wakes the countdown when a show starts, is skipped or cancelled.
    pub show_wake: Notify,
    /// The last OBS reading and what it means.
    pub health: Mutex<Option<(HealthSample, Health)>>,
    /// The scene to go back to after BRB.
    pub brb_from: Mutex<Option<String>>,
    /// Active BRB holds (Manual, AdBreak, Away, BlackCam).
    pub brb_holds: Mutex<HashSet<BrbHold>>,
    /// One health watcher per engine.
    pub health_watching: std::sync::atomic::AtomicBool,
    /// The live watch (a silent mic, a black or frozen camera, a destination down) and its
    /// one loop.
    pub live_watch: Mutex<ferrocast_core::show::LiveWatch>,
    /// What was said about the recording now (a stop, low room), so each is said once.
    pub record_watch: Mutex<record_watch::RecordWatch>,
    /// Web page sources already said to have stopped drawing (said once, and again when they
    /// draw).
    pub stalled_noted: Mutex<HashSet<String>>,
    /// What was said about broken sidechain loops, so each loop is reported once.
    pub sidechain_watch: Mutex<record_watch::SidechainWatch>,
    pub live_watching: std::sync::atomic::AtomicBool,
    /// Cameras black now that put the show on BRB by itself (it comes back when none is).
    pub auto_brb: Mutex<std::collections::BTreeSet<String>>,
    /// The stream's health now (None while it isn't live) and its memory.
    pub live_health: Mutex<(ferrocast_core::health::HealthMeter, Option<ferrocast_core::health::Health>)>,
    /// Auto quality's state for the stream now.
    pub auto_quality: Mutex<ferrocast_core::auto_quality::AutoQuality>,
    /// The scene on air while the Studio streams (a change is a chapter marker).
    pub scene_now: Mutex<Option<String>>,
    /// Auto-replies: the rules, compiled, their cooldowns and the send budget.
    pub autoreplies: autoreplies::AutoReplies,
    /// Event rules running now (by their place in the list).
    pub rules_busy: Mutex<HashSet<usize>>,
    /// Items and filters held shown, hidden, on or off by a key held down (never saved).
    pub holds: Mutex<push_hold::Holds>,
    /// The Studio's copy and paste of sources and filters: kept while another collection is
    /// opened, gone when the app quits.
    pub clipboard: Mutex<Option<clipboard::Clip>>,
    /// The clips library's state: its folder override (tests) and the background jobs.
    pub clips: crate::clips::ClipsState,
    /// Encoder stress benchmark and setting rollback state machine.
    pub auto_config: Mutex<encoder_bench::AutoConfigSlot>,
    /// Persisted hardware/software encoder throughput ceiling (`settings["studio_bench"]`).
    pub studio_bench: RwLock<Option<crate::golive_plan::PersistedStudioBench>>,
    /// Pre-apply encoder settings queued on startup after an unclean exit, applied when `StudioHost` attaches.
    pub pending_startup_restore: Mutex<Option<encoder_bench::PendingApply>>,
}

impl Default for Studio {
    fn default() -> Self {
        Self {
            counters: Mutex::default(),
            counter_defs: RwLock::default(),
            modules: RwLock::default(),
            scenes: RwLock::default(),
            moments: RwLock::default(),
            show_cfg: RwLock::default(),
            duels: Mutex::default(),
            cooldowns: Mutex::default(),
            watching: Mutex::default(),
            lane: Mutex::default(),
            spike: Mutex::default(),
            machine: Mutex::default(),
            output_lock: tokio::sync::Mutex::new(()),
            output_gate: tokio::sync::Mutex::new(()),
            update_committing: std::sync::atomic::AtomicBool::new(false),
            auto_config_op: tokio::sync::Mutex::new(()),
            mono_origin: std::time::Instant::now(),
            last_watch_mono: Mutex::default(),
            show_wake: Notify::new(),
            health: Mutex::default(),
            brb_from: Mutex::default(),
            brb_holds: Mutex::default(),
            health_watching: std::sync::atomic::AtomicBool::default(),
            live_watch: Mutex::default(),
            record_watch: Mutex::default(),
            stalled_noted: Mutex::default(),
            sidechain_watch: Mutex::default(),
            live_watching: std::sync::atomic::AtomicBool::default(),
            auto_brb: Mutex::default(),
            live_health: Mutex::default(),
            auto_quality: Mutex::default(),
            scene_now: Mutex::default(),
            autoreplies: autoreplies::AutoReplies::default(),
            rules_busy: Mutex::default(),
            holds: Mutex::default(),
            clipboard: Mutex::default(),
            clips: crate::clips::ClipsState::default(),
            auto_config: Mutex::default(),
            studio_bench: RwLock::default(),
            pending_startup_restore: Mutex::default(),
        }
    }
}

impl Engine {
    pub(crate) fn studio(&self) -> &Studio {
        &self.inner.studio
    }

    /// Monotonic milliseconds since this engine started (immune to wall-clock / NTP jumps).
    pub(crate) fn mono_ms(&self) -> i64 {
        i64::try_from(self.studio().mono_origin.elapsed().as_millis()).unwrap_or(i64::MAX)
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
                    repo::settings::get::<crate::golive_plan::PersistedStudioBench>(
                        s,
                        crate::golive_plan::STUDIO_BENCH_KEY,
                    ),
                    repo::studio::counter_values(s),
                ))
            })
            .await;
        let Ok((defs, modules, scenes, moments, show, bench, values)) = loaded else { return };
        let st = self.studio();
        let ok = |what: &str, e: &ferrocast_store::StoreError| tracing::error!(error = %e, what, "a Studio setting couldn't be read");
        if let Ok(b) = bench {
            *st.studio_bench.write().unwrap_or_else(|p| p.into_inner()) = b;
        } else if let Err(e) = bench {
            ok("studio_bench", &e);
        }
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
        self.rules_saw(e).await;
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

    /// The oldest chat to show (the chat panel, chat overlays): this stream's (and the five
    /// minutes before it, the waiting room), or the last six hours while offline. A new stream
    /// never opens with the last one's messages (owner, 30 Oct).
    pub fn chat_cutoff(&self) -> i64 {
        match self.stream_started_at() {
            Some(start) => start - 5 * 60_000,
            None => crate::now().0 - 6 * 3_600_000,
        }
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
