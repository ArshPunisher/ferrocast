//! "Start show" and "End show" (API.md §4 "Studio: show"): the go-live checklist the
//! streamer sets up once, pre-flight checks with one-click fixes, and the locked broadcast
//! state machine (`Idle → Preparing → Starting/Countdown → Live → Ending`) that serializes
//! countdowns, delayed stops, hotkeys, and Studio stop in the Studio (or OBS, see `rig`).
//! Every step reports what it did; one step failing never stops the others (the Studio
//! stopped shouldn't stop the stream info going out).

use std::time::Duration;

use ferrocast_core::event::Platform;
use ferrocast_core::show::{self, Check, CheckStatus, ShowConfig, fix};
use ferrocast_core::template::Template;
use serde::Serialize;

use super::rig::Rig;
use super::{SHOW_KEY, save_setting};
use crate::{Engine, EngineError, now, platform_key};

/// Public wire phases of a Show run (kept to the 4 strings the dashboard renders).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Starting Soon, waiting for the streamer (no countdown).
    Starting,
    Countdown,
    Live,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    pub id: String,
    pub label: String,
    /// `done`, `failed`, `skipped`, `waiting`.
    pub status: String,
    pub detail: String,
}

impl Step {
    pub(crate) fn new(id: &str, label: &str, status: &str, detail: impl Into<String>) -> Step {
        Step { id: id.into(), label: label.into(), status: status.into(), detail: detail.into() }
    }
}

/// The show in progress (or the last one), as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShowRun {
    pub id: String,
    pub phase: Phase,
    pub started_at: i64,
    pub countdown_ends_at: Option<i64>,
    pub live_scene: Option<String>,
    pub ended_at: Option<i64>,
    pub steps: Vec<Step>,
}

/// Per-run options for `start_show_with` (without mutating saved `ShowConfig`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ShowStartOpts {
    pub force: bool,
    /// Override `ShowConfig.start_stream` for this run only (None = use saved config).
    pub stream: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preflight {
    pub checks: Vec<Check>,
    pub ready: bool,
}

/// Internal phases of the locked broadcast state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum BroadcastPhase {
    #[default]
    Idle,
    /// `start_show` checklist is running.
    Preparing,
    /// On Starting Soon without an active countdown (or after cancelling countdown).
    Starting,
    /// Counting down on a monotonic timer before switching to the live scene.
    Countdown,
    /// On air on the live scene.
    Live,
    /// `end_show` steps are running or a delayed stop is waiting.
    Ending,
}

/// A gap longer than this between 1-second countdown ticks means the system slept or froze:
/// clamp the deducted time so waking from sleep never fires `go_live_now` prematurely.
const COUNTDOWN_WAKE_JUMP: Duration = Duration::from_secs(5);
const COUNTDOWN_WAKE_CLAMP: Duration = Duration::from_secs(1);

/// Monotonic countdown timer driven by `tokio::time::Instant` (immune to wall-clock jumps).
#[derive(Debug, Clone)]
pub(crate) struct CountdownTimer {
    remaining: Duration,
    last_tick: tokio::time::Instant,
}

impl CountdownTimer {
    fn start(remaining: Duration, now: tokio::time::Instant) -> Self {
        Self { remaining, last_tick: now }
    }

    /// Advances the timer to `now`. Returns `(remaining, wake_jumped)`.
    fn advance(&mut self, now: tokio::time::Instant) -> (Duration, bool) {
        let raw = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        let jumped = raw > COUNTDOWN_WAKE_JUMP;
        let step = if jumped { COUNTDOWN_WAKE_CLAMP } else { raw };
        self.remaining = self.remaining.saturating_sub(step);
        (self.remaining, jumped)
    }
}

/// Single locked state machine for the Show, countdown, and delayed stream stop.
/// Protected by a fast `std::sync::Mutex` and NEVER held across `.await`.
#[derive(Debug, Default)]
pub(crate) struct BroadcastMachine {
    pub phase: BroadcastPhase,
    /// Incremented on every Show transition; in-flight async work checks its epoch
    /// before side-effects and before committing state.
    pub epoch: u64,
    /// Incremented whenever any stream starts (Show or plain Go live) or Studio stops.
    /// A delayed stop only fires if BOTH `epoch` and `stream_epoch` still match.
    pub stream_epoch: u64,
    pub run: Option<ShowRun>,
    pub countdown: Option<CountdownTimer>,
    pub stop_task: Option<tokio::task::AbortHandle>,
    /// Epoch of an in-flight `go_live_now` transition (blocks duplicate switches/chat
    /// and lets `cancel_countdown` / `studio_stop` invalidate it cleanly).
    pub transitioning_live: Option<u64>,
    pub ending_in_flight: bool,
    pub stop_waiting: bool,
    pub started_stream: bool,
}

/// Rolls back `Preparing → Idle` if `start_show_with` exits early or is dropped mid-await.
struct PreparingGuard<'a> {
    engine: &'a Engine,
    epoch: u64,
    committed: bool,
}

impl Drop for PreparingGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut m = self.engine.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
        if m.epoch == self.epoch && m.phase == BroadcastPhase::Preparing {
            m.phase = BroadcastPhase::Idle;
            m.started_stream = false;
        }
    }
}

/// Clears `ending_in_flight` if `end_show_opts` exits early or is dropped mid-await.
struct EndingGuard<'a> {
    engine: &'a Engine,
    epoch: u64,
    committed: bool,
}

impl Drop for EndingGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut m = self.engine.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
        if m.epoch == self.epoch && m.ending_in_flight {
            m.ending_in_flight = false;
            if !m.stop_waiting && m.phase == BroadcastPhase::Ending {
                m.phase = BroadcastPhase::Idle;
            }
        }
    }
}

/// Clears `transitioning_live` if `go_live_now` exits early or is dropped mid-await.
struct LiveTransitionGuard<'a> {
    engine: &'a Engine,
    epoch: u64,
    committed: bool,
}

impl Drop for LiveTransitionGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut m = self.engine.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
        if m.transitioning_live == Some(self.epoch) {
            m.transitioning_live = None;
        }
    }
}

impl Engine {
    pub fn show_config(&self) -> ShowConfig {
        self.studio().show_cfg.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub async fn patch_show_config(
        &self,
        patch: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<ShowConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut cfg = crate::patch::merge(&self.show_config(), patch)?;
        for s in [&mut cfg.starting_scene, &mut cfg.live_scene, &mut cfg.ending_scene, &mut cfg.preset_id] {
            if s.as_deref().is_some_and(|v| v.trim().is_empty()) {
                *s = None;
            }
        }
        cfg.validate().map_err(EngineError::Invalid)?;
        if let Some(id) = &cfg.preset_id {
            self.preset(id)
                .await
                .map_err(|_| EngineError::Invalid("preset_id: that preset doesn't exist any more.".into()))?;
        }
        save_setting(self, SHOW_KEY, &cfg).await?;
        *self.studio().show_cfg.write().unwrap_or_else(|p| p.into_inner()) = cfg.clone();
        Ok(cfg)
    }

    pub fn show_run(&self) -> Option<ShowRun> {
        self.studio().machine.lock().unwrap_or_else(|p| p.into_inner()).run.clone()
    }

    /// True while a Show is actively starting, counting down, live, or running its ending
    /// steps (used to refuse scene-collection swaps mid-show).
    pub fn is_show_active(&self) -> bool {
        let m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
        matches!(
            m.phase,
            BroadcastPhase::Preparing | BroadcastPhase::Starting | BroadcastPhase::Countdown | BroadcastPhase::Live
        ) || m.ending_in_flight
    }

    fn is_show_epoch(&self, epoch: u64) -> bool {
        self.studio().machine.lock().unwrap_or_else(|p| p.into_inner()).epoch == epoch
    }

    fn is_live_transition(&self, epoch: u64) -> bool {
        let m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
        m.epoch == epoch && m.transitioning_live == Some(epoch)
    }

    fn emit_show(&self, run: Option<ShowRun>) {
        self.studio_frame("show", &serde_json::json!({ "show": run }));
        // Timers on screen follow Start show's countdown (they reach zero with the switch).
        self.send_overlay(crate::frames::OverlayFrame::new(
            crate::frames::Audience::All,
            self.next_seq(),
            "state",
            &serde_json::json!({ "show": self.show_overlay_state() }),
        ));
        self.studio().show_wake.notify_one();
    }

    /// What overlays get about the show: when its countdown ends (ms), or null.
    pub(crate) fn show_overlay_state(&self) -> serde_json::Value {
        let ends = self.show_run().and_then(|r| r.countdown_ends_at);
        serde_json::json!({ "countdown_ends_at": ends })
    }

    #[cfg(test)]
    pub(crate) fn set_countdown_remaining_for_test(&self, remaining: Duration) {
        let run = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if m.phase == BroadcastPhase::Countdown {
                m.countdown = Some(CountdownTimer::start(remaining, tokio::time::Instant::now()));
                if let Some(r) = m.run.as_mut() {
                    r.countdown_ends_at = Some(now().0 + i64::try_from(remaining.as_millis()).unwrap_or(0));
                }
            }
            m.run.clone()
        };
        self.emit_show(run);
    }

    /// Called whenever a stream starts (Show or plain Go live): advances `stream_gen` and
    /// aborts any pending delayed stop from an earlier Show so it can never kill this stream.
    pub(crate) fn on_stream_started(&self) {
        let emit = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            m.stream_epoch += 1;
            if let Some(h) = m.stop_task.take() {
                h.abort();
            }
            if m.stop_waiting {
                m.stop_waiting = false;
                if m.phase == BroadcastPhase::Ending && !m.ending_in_flight {
                    m.phase = BroadcastPhase::Idle;
                }
                if let Some(r) = m.run.as_mut() {
                    if let Some(s) = r.steps.iter_mut().find(|s| s.id == "stop_stream" && s.status == "waiting") {
                        s.status = "skipped".into();
                        s.detail = "A new stream started.".into();
                    }
                    Some(r.clone())
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(run) = emit {
            self.emit_show(Some(run));
        }
    }

    /// Cancels any active Show, countdown, or pending delayed stop when the Studio stops.
    pub(crate) fn cancel_show_on_studio_stop(&self, reason: &str) {
        let emit = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = m.stop_task.take() {
                h.abort();
            }
            let was_waiting = m.stop_waiting;
            m.stop_waiting = false;
            m.countdown = None;
            m.transitioning_live = None;
            m.started_stream = false;
            m.ending_in_flight = false;
            m.epoch += 1;
            m.stream_epoch += 1;
            m.phase = BroadcastPhase::Idle;
            if let Some(r) = m.run.as_mut() {
                if r.phase != Phase::Ended {
                    r.phase = Phase::Ended;
                    r.ended_at = Some(now().0);
                    r.countdown_ends_at = None;
                    if let Some(s) = r.steps.iter_mut().find(|s| s.id == "countdown" && s.status == "waiting") {
                        s.status = "skipped".into();
                        s.detail = reason.into();
                    }
                    if let Some(s) = r.steps.iter_mut().find(|s| s.id == "stop_stream" && s.status == "waiting") {
                        s.status = "done".into();
                        s.detail = reason.into();
                    }
                    Some(r.clone())
                } else if was_waiting {
                    if let Some(s) = r.steps.iter_mut().find(|s| s.id == "stop_stream" && s.status == "waiting") {
                        s.status = "done".into();
                        s.detail = reason.into();
                    }
                    Some(r.clone())
                } else {
                    None
                }
            } else {
                None
            }
        };
        *self.studio().brb_from.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.studio().brb_holds.lock().unwrap_or_else(|p| p.into_inner()).clear();
        self.studio().auto_brb.lock().unwrap_or_else(|p| p.into_inner()).clear();
        if let Some(run) = emit {
            self.emit_show(Some(run));
        } else {
            self.studio().show_wake.notify_one();
        }
    }

    /// Everything that could go wrong at go-live, each with how to fix it.
    pub async fn preflight(&self) -> Preflight {
        let cfg = self.show_config();
        let mut checks = Vec::new();
        let rig = self.rig().await;
        match &rig {
            Ok(Rig::Studio) => checks.push(Check::new("obs", "Studio", CheckStatus::Ok, "Running.", None)),
            Ok(Rig::Obs(_)) => checks.push(Check::new("obs", "OBS", CheckStatus::Ok, "Connected.", None)),
            // OBS is the fallback: the Studio is what to start.
            Err(_) => checks.push(Check::new(
                "obs",
                "Studio",
                CheckStatus::Fail,
                "Studio isn't started.",
                fix("Open the Studio", "#/studio/golive"),
            )),
        }
        if let Ok(r) = &rig {
            let app = r.name();
            let mut on_air = None;
            match self.rig_scenes(r).await {
                Ok((current, names)) => {
                    on_air = current.filter(|s| !s.is_empty());
                    for (id, label, scene) in [
                        ("starting_scene", "Starting scene", &cfg.starting_scene),
                        ("live_scene", "Live scene", &cfg.live_scene),
                        ("ending_scene", "Ending scene", &cfg.ending_scene),
                    ] {
                        let Some(s) = scene else { continue };
                        checks.push(if names.iter().any(|n| n == s) {
                            Check::new(id, label, CheckStatus::Ok, format!("\"{s}\" is in {app}."), None)
                        } else {
                            Check::new(
                                id,
                                label,
                                CheckStatus::Fail,
                                format!("{} has no scene \"{s}\": renamed or deleted?", r.title()),
                                fix("Pick the scene again", "#/studio/show"),
                            )
                        });
                    }
                }
                Err(e) => checks.push(Check::new("scenes", "Scenes", CheckStatus::Warn, e.to_string(), None)),
            }
            // What the mics actually hear, and what the cameras actually show (the Studio).
            let loudest = self.rig_loudest(r, &cfg.mic_sources).await;
            let looks = self.rig_look(r, &cfg.cam_sources).await;
            for m in &cfg.mic_sources {
                let hears = loudest.as_ref().and_then(|l| l.get(m)).map(|db| *db > show::SILENT_DB);
                checks.push(match self.rig_muted(r, m).await {
                    Ok(false) if hears == Some(false) => Check::new(
                        &format!("mic:{m}"),
                        m,
                        CheckStatus::Warn,
                        "Not muted, but silent: check the cable and the input.",
                        fix("Check the mic", "#/studio/engine"),
                    ),
                    Ok(false) => Check::new(&format!("mic:{m}"), m, CheckStatus::Ok, "Not muted.", None),
                    Ok(true) => Check::new(
                        &format!("mic:{m}"),
                        m,
                        CheckStatus::Fail,
                        format!("Muted in {app}: nobody will hear you."),
                        fix("Unmute", format!("action:unmute:{m}")),
                    ),
                    Err(e) => Check::new(
                        &format!("mic:{m}"),
                        m,
                        CheckStatus::Warn,
                        e.to_string(),
                        fix("Pick the mic again", "#/studio/show"),
                    ),
                });
            }
            let scene = cfg.live_scene.clone().or(on_air.clone()).or(cfg.starting_scene.clone());
            for cam in &cfg.cam_sources {
                let Some(scene) = &scene else { break };
                let black = looks.as_ref().is_some_and(|l| {
                    let (luma, spread) = (l[cam]["luma"].as_u64(), l[cam]["spread"].as_u64());
                    matches!((luma, spread), (Some(y), Some(s)) if y < 12 && s < 6)
                });
                checks.push(match self.rig_visible(r, scene, cam).await {
                    Ok(Some(true)) if black => Check::new(
                        &format!("cam:{cam}"),
                        cam,
                        CheckStatus::Fail,
                        format!("Shown in \"{scene}\" but black: lens covered, or used by another app?"),
                        fix("Check the camera", "#/studio/engine"),
                    ),
                    Ok(Some(true)) => {
                        Check::new(&format!("cam:{cam}"), cam, CheckStatus::Ok, format!("Shown in \"{scene}\"."), None)
                    }
                    Ok(Some(false)) => Check::new(
                        &format!("cam:{cam}"),
                        cam,
                        CheckStatus::Warn,
                        format!("Hidden in \"{scene}\"."),
                        fix("Show it", format!("action:show:{scene}/{cam}")),
                    ),
                    Ok(None) => Check::new(
                        &format!("cam:{cam}"),
                        cam,
                        CheckStatus::Warn,
                        format!("Not in \"{scene}\"."),
                        fix("Pick the camera again", "#/studio/show"),
                    ),
                    Err(e) => Check::new(&format!("cam:{cam}"), cam, CheckStatus::Warn, e.to_string(), None),
                });
            }
            self.preflight_picture_and_mics(r, &cfg, on_air.as_deref(), &mut checks).await;
            if cfg.replay_buffer {
                checks.push(match self.rig_replay_active(r).await {
                    Ok(true) => Check::new("replay", "Replay buffer", CheckStatus::Ok, "Running.", None),
                    Ok(false) => Check::new("replay", "Replay buffer", CheckStatus::Ok, "Starts with the show.", None),
                    Err(e) => Check::new("replay", "Replay buffer", CheckStatus::Warn, e.to_string(), None),
                });
            }
        }
        let disk = match &rig {
            Ok(r) => self.rig_disk_mb(r).await,
            Err(_) => None,
        };
        checks.push(show::disk_check(disk, cfg.min_disk_gb));

        // Platforms
        let connectors = self.state().connectors.clone();
        let wanted: Vec<Platform> = if cfg.platforms.is_empty() {
            Platform::CHAT.iter().copied().filter(|p| self.connector(*p).is_some()).collect()
        } else {
            cfg.platforms.clone()
        };
        if wanted.is_empty() {
            checks.push(Check::new(
                "platforms",
                "Platforms",
                CheckStatus::Warn,
                "No platform connected: chat, alerts and stream info won't work.",
                fix("Connect", "#/settings/accounts"),
            ));
        }
        for p in &wanted {
            let state = connectors.get(platform_key(*p)).map(|c| c.state.clone()).unwrap_or_else(|| "off".into());
            let connected = self.connector(*p).is_some();
            let (status, detail) = match state.as_str() {
                _ if !connected => (CheckStatus::Fail, format!("{} isn't connected.", p.label())),
                "sign_in_needed" => (CheckStatus::Fail, format!("{} needs you to sign in again.", p.label())),
                "scopes_missing" => (CheckStatus::Warn, format!("{} is missing a permission.", p.label())),
                "reconnecting" | "disconnected" => (CheckStatus::Warn, format!("{} is reconnecting.", p.label())),
                _ => (CheckStatus::Ok, format!("{} is connected.", p.label())),
            };
            let f = (status != CheckStatus::Ok)
                .then(|| show::Fix { label: "Accounts".into(), href: "#/settings/accounts".into() });
            checks.push(Check::new(&format!("platform:{}", platform_key(*p)), p.label(), status, detail, f));
        }
        if cfg.preset_id.is_some()
            && let Ok(access) = self.stream_info_access().await
        {
            for a in access.iter().filter(|a| a.connected && !a.can_edit) {
                checks.push(Check::new(
                    &format!("stream_info:{}", platform_key(a.platform)),
                    "Stream info",
                    CheckStatus::Warn,
                    a.needs.clone().unwrap_or_default(),
                    fix("Reconnect", "#/settings/accounts"),
                ));
            }
        }
        if cfg.discord {
            let has = self.discord().await.map(|d| d.has_webhook && d.enabled).unwrap_or(false);
            if !has {
                checks.push(Check::new(
                    "discord",
                    "Discord post",
                    CheckStatus::Warn,
                    "No Discord webhook set (or posts off): nothing is posted.",
                    fix("Discord settings", "#/settings/discord"),
                ));
            }
        }
        checks.extend(self.golive_preflight().await);
        let ready = show::ready(&checks);
        Preflight { checks, ready }
    }

    /// Runs a pre-flight fix: `unmute:<input>`, `show:<scene>/<source>`, `start_replay_buffer`.
    pub async fn preflight_fix(&self, action: &str) -> Result<Preflight, EngineError> {
        let rig = self.rig().await?;
        if let Some(input) = action.strip_prefix("unmute:") {
            self.rig_set_muted(&rig, input, false).await?;
        } else if let Some(rest) = action.strip_prefix("show:") {
            let (scene, source) = rest
                .split_once('/')
                .ok_or_else(|| EngineError::Invalid("action: use show:<scene>/<source>.".into()))?;
            self.rig_set_visible(&rig, scene, source, true).await.map_err(|e| match e {
                EngineError::Invalid(m) => EngineError::Invalid(format!("action: {m}")),
                e => e,
            })?;
        } else if action == "start_replay_buffer" {
            self.rig_start_replay(&rig).await?;
        } else {
            return Err(EngineError::Invalid(
                "action: unmute:<input>, show:<scene>/<source> or start_replay_buffer.".into(),
            ));
        }
        Ok(self.preflight().await)
    }

    /// Unified Go live action (hotkeys & backend state machine): runs the Show when a Show
    /// style is configured (without mutating saved `ShowConfig`), advances an active countdown
    /// or Starting Soon to the live scene, or starts plain streaming for "Just go live".
    pub async fn go_live(&self) -> Result<(), EngineError> {
        let phase = {
            let m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if m.ending_in_flight {
                return Err(EngineError::Conflict("The show is ending. Wait a moment and try again.".into()));
            }
            m.phase
        };
        match phase {
            BroadcastPhase::Preparing => Ok(()),
            BroadcastPhase::Countdown => self.skip_countdown().await.map(drop),
            BroadcastPhase::Starting => self.go_live_scene().await.map(drop),
            BroadcastPhase::Live => {
                if !self.studio_stream_stats().await.ok().is_some_and(|v| v["active"].as_bool() == Some(true)) {
                    self.studio_stream(true).await?;
                }
                Ok(())
            }
            BroadcastPhase::Idle | BroadcastPhase::Ending => {
                if self.show_config().is_show_style() {
                    self.start_show_with(ShowStartOpts { force: true, stream: Some(true) }).await.map(drop)
                } else {
                    self.studio_stream(true).await.map(drop)
                }
            }
        }
    }

    /// Unified End stream action (hotkeys & backend state machine): ends an active Show
    /// (stopping the stream), stops immediately if already waiting on a stop delay, or stops
    /// plain streaming when no Show is active.
    pub async fn end_live(&self) -> Result<(), EngineError> {
        let (phase, ending_in_flight, stop_waiting) = {
            let m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            (m.phase, m.ending_in_flight, m.stop_waiting)
        };
        if ending_in_flight {
            return Ok(());
        }
        match phase {
            BroadcastPhase::Ending if stop_waiting => self.end_show_with(true).await.map(drop),
            BroadcastPhase::Preparing | BroadcastPhase::Starting | BroadcastPhase::Countdown | BroadcastPhase::Live => {
                self.end_show_opts(false, Some(true)).await.map(drop)
            }
            BroadcastPhase::Idle | BroadcastPhase::Ending => self.studio_stream(false).await.map(drop),
        }
    }

    /// Starts the show. 409 while one is running or starting; 422 `preflight_failed` when a
    /// check failed, unless `force`.
    pub async fn start_show(&self, force: bool) -> Result<ShowRun, EngineError> {
        self.start_show_with(ShowStartOpts { force, stream: None }).await
    }

    /// Starts the show with per-run options (e.g. `stream: Some(true)` without rewriting saved
    /// `ShowConfig`). Serialized through the locked `BroadcastMachine`.
    pub async fn start_show_with(&self, opts: ShowStartOpts) -> Result<ShowRun, EngineError> {
        let cfg = self.show_config();
        let start_stream = opts.stream.unwrap_or(cfg.start_stream);

        // A show whose stream stopped some other way (the stream dropping, external stop) is
        // over: it no longer blocks a new one (19 Nov, live). A countdown still blocks until
        // cancelled. (Only a show that streamed: one that didn't may run with the stream off.)
        let stale_candidate = {
            let m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if (m.started_stream || cfg.start_stream)
                && matches!(m.phase, BroadcastPhase::Live | BroadcastPhase::Starting)
            {
                Some(m.epoch)
            } else {
                None
            }
        };
        if let Some(old_gen) = stale_candidate
            && let Ok(rig) = self.rig().await
            && matches!(self.rig_streaming(&rig).await, Ok(false))
        {
            let ended = {
                let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
                if m.epoch == old_gen && matches!(m.phase, BroadcastPhase::Live | BroadcastPhase::Starting) {
                    m.phase = BroadcastPhase::Idle;
                    m.epoch += 1;
                    m.countdown = None;
                    m.transitioning_live = None;
                    m.started_stream = false;
                    if let Some(r) = m.run.as_mut() {
                        r.phase = Phase::Ended;
                        r.ended_at = Some(now().0);
                        r.countdown_ends_at = None;
                        Some(r.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(r) = ended {
                self.emit_show(Some(r));
            }
        }

        // Atomically claim `Preparing` before any `.await` so double-clicks or racing starts
        // never overlap, and abort any pending delayed stop from an earlier ended Show.
        let my_gen = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if matches!(
                m.phase,
                BroadcastPhase::Preparing | BroadcastPhase::Starting | BroadcastPhase::Countdown | BroadcastPhase::Live
            ) {
                return Err(EngineError::Conflict(
                    "A show is already running. End it first, or cancel the countdown.".into(),
                ));
            }
            if m.ending_in_flight {
                return Err(EngineError::Conflict("The show is ending. Wait a moment and try again.".into()));
            }
            if let Some(h) = m.stop_task.take() {
                h.abort();
            }
            m.stop_waiting = false;
            m.epoch += 1;
            let g = m.epoch;
            m.phase = BroadcastPhase::Preparing;
            m.countdown = None;
            m.transitioning_live = None;
            m.started_stream = start_stream;
            g
        };
        let mut guard = PreparingGuard { engine: self, epoch: my_gen, committed: false };

        if !opts.force {
            let pf = self.preflight().await;
            if !self.is_show_epoch(my_gen) {
                return Err(EngineError::Conflict("Show start was cancelled.".into()));
            }
            if !pf.ready {
                let failed: Vec<String> = pf
                    .checks
                    .iter()
                    .filter(|c| c.status == CheckStatus::Fail)
                    .map(|c| format!("{}: {}", c.label, c.detail))
                    .collect();
                return Err(EngineError::Refused {
                    status: 422,
                    code: "preflight_failed",
                    message: format!("Not ready: {}", failed.join(" ")),
                    hint: "Fix what the checklist shows, or press Start anyway.".into(),
                    retry_after: None,
                });
            }
        }

        let cfg = self.show_config();
        let start_stream = opts.stream.unwrap_or(cfg.start_stream);
        let at = now().0;
        let mut steps = Vec::new();
        let mut stream_started_by_us = false;

        // Stream info first: it's what viewers see in the directory.
        let mut title = None;
        if let Some(id) = &cfg.preset_id {
            steps.push(match self.preset(id).await {
                Err(_) => Step::new("stream_info", "Stream info", "failed", "The preset doesn't exist any more."),
                Ok(p) => {
                    title = p.info.title.clone();
                    match self.apply_stream_info(p.info, None, None).await {
                        Ok(r) if r.failed == 0 => Step::new(
                            "stream_info",
                            "Stream info",
                            "done",
                            format!("\"{}\" sent to {} platform(s).", p.name, r.ok),
                        ),
                        Ok(r) => {
                            let why: Vec<String> = r
                                .results
                                .iter()
                                .filter(|x| !x.ok)
                                .map(|x| {
                                    format!(
                                        "{}: {}",
                                        x.platform.label(),
                                        x.error.as_ref().map(|e| e.message.as_str()).unwrap_or("failed")
                                    )
                                })
                                .collect();
                            Step::new(
                                "stream_info",
                                "Stream info",
                                if r.ok > 0 { "done" } else { "failed" },
                                why.join(" "),
                            )
                        }
                        Err(e) => Step::new("stream_info", "Stream info", "failed", e.to_string()),
                    }
                }
            });
        }
        if !self.is_show_epoch(my_gen) {
            return Err(EngineError::Conflict("Show start was cancelled.".into()));
        }

        let client = if cfg.starting_scene.is_some()
            || cfg.live_scene.is_some()
            || cfg.countdown_s > 0
            || cfg.replay_buffer
            || start_stream
        {
            Some(self.rig().await)
        } else {
            None
        };
        // Capture the scene on air before switching to Starting Soon: when `live_scene` isn't
        // explicitly configured, the Show returns here instead of guessing a scene by name.
        let on_air_at_start = if cfg.live_scene.is_none()
            && let Some(Ok(r)) = &client
        {
            self.rig_scenes(r).await.ok().and_then(|(cur, _)| cur).filter(|s| !s.is_empty())
        } else {
            None
        };
        let live_scene = cfg.live_scene.clone().or_else(|| {
            on_air_at_start.filter(|s| {
                cfg.starting_scene.as_deref() != Some(s.as_str()) && cfg.ending_scene.as_deref() != Some(s.as_str())
            })
        });

        if !self.is_show_epoch(my_gen) {
            return Err(EngineError::Conflict("Show start was cancelled.".into()));
        }
        if cfg.replay_buffer {
            steps.push(match &client {
                Some(Ok(r)) => match self.rig_start_replay(r).await {
                    Ok(()) => Step::new("replay", "Replay buffer", "done", "Running, so \"clip that\" works."),
                    Err(e) => Step::new("replay", "Replay buffer", "failed", e.to_string()),
                },
                Some(Err(e)) => Step::new("replay", "Replay buffer", "failed", e.to_string()),
                None => Step::new("replay", "Replay buffer", "skipped", ""),
            });
        }
        if !self.is_show_epoch(my_gen) {
            return Err(EngineError::Conflict("Show start was cancelled.".into()));
        }
        if let Some(scene) = &cfg.starting_scene {
            steps.push(self.switch_step(&client, "starting_scene", "Starting scene", scene).await);
        }
        if !self.is_show_epoch(my_gen) {
            return Err(EngineError::Conflict("Show start was cancelled.".into()));
        }
        if start_stream {
            steps.push(match &client {
                Some(Ok(r)) => match self.rig_set_streaming(r, true).await {
                    Ok(()) => {
                        stream_started_by_us = true;
                        Step::new("stream", "Go live", "done", format!("{} is streaming.", r.title()))
                    }
                    Err(e) => Step::new("stream", "Go live", "failed", e.to_string()),
                },
                Some(Err(e)) => Step::new("stream", "Go live", "failed", e.to_string()),
                None => Step::new("stream", "Go live", "skipped", ""),
            });
        }
        if !self.is_show_epoch(my_gen) {
            if stream_started_by_us
                && let Some(Ok(r)) = &client
                && matches!(self.rig_streaming(r).await, Ok(true))
            {
                let _ = self.rig_set_streaming(r, false).await;
            }
            return Err(EngineError::Conflict("Show start was cancelled.".into()));
        }
        if cfg.alerts_on {
            let q = self.queue().await;
            if q.paused {
                self.queue_resume().await;
                steps.push(Step::new("alerts", "Alerts", "done", "Alerts were paused; they're on now."));
            } else {
                steps.push(Step::new("alerts", "Alerts", "done", "On."));
            }
        }
        if cfg.bot_on {
            let mut patch = serde_json::Map::new();
            patch.insert("bot_enabled".into(), serde_json::Value::Bool(true));
            steps.push(match self.patch_settings(&patch).await {
                Ok(_) => Step::new("bot", "Bot and timers", "done", "On."),
                Err(e) => Step::new("bot", "Bot and timers", "failed", e.to_string()),
            });
        }
        if !self.is_show_epoch(my_gen) {
            if stream_started_by_us
                && let Some(Ok(r)) = &client
                && matches!(self.rig_streaming(r).await, Ok(true))
            {
                let _ = self.rig_set_streaming(r, false).await;
            }
            return Err(EngineError::Conflict("Show start was cancelled.".into()));
        }
        if cfg.discord {
            let d = self.discord().await.ok();
            steps.push(match d {
                Some(d) if d.has_webhook && d.enabled => {
                    let primary = Platform::CHAT
                        .iter()
                        .copied()
                        .find(|p| self.connector(*p).is_some())
                        .unwrap_or(Platform::Twitch);
                    self.discord_go_live(primary, title.clone());
                    Step::new("discord", "Discord post", "done", "Posting your go-live message.")
                }
                _ => Step::new("discord", "Discord post", "skipped", "No Discord webhook (Settings, Discord)."),
            });
        }
        let (phase, next_bphase, countdown_timer, ends) = if cfg.countdown_s > 0 {
            let n = cfg.countdown_s;
            steps.push(Step::new(
                "countdown",
                "Countdown",
                "waiting",
                if cfg.at_zero.switch { format!("{n} seconds, then the live scene.") } else { format!("{n} seconds.") },
            ));
            let dur = Duration::from_secs(u64::from(n));
            (
                Phase::Countdown,
                BroadcastPhase::Countdown,
                Some(CountdownTimer::start(dur, tokio::time::Instant::now())),
                Some(now().0 + i64::from(n) * 1000),
            )
        } else if let Some(scene) = &cfg.live_scene {
            if !self.is_show_epoch(my_gen) {
                if stream_started_by_us
                    && let Some(Ok(r)) = &client
                    && matches!(self.rig_streaming(r).await, Ok(true))
                {
                    let _ = self.rig_set_streaming(r, false).await;
                }
                return Err(EngineError::Conflict("Show start was cancelled.".into()));
            }
            steps.push(self.switch_step(&client, "live_scene", "Live scene", scene).await);
            (Phase::Live, BroadcastPhase::Live, None, None)
        } else {
            (Phase::Starting, BroadcastPhase::Starting, None, None)
        };
        let run = ShowRun {
            id: ferrocast_store::new_id(),
            phase,
            started_at: at,
            countdown_ends_at: ends,
            live_scene,
            ended_at: None,
            steps,
        };
        let committed = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if m.epoch == my_gen && m.phase == BroadcastPhase::Preparing {
                m.phase = next_bphase;
                m.countdown = countdown_timer;
                m.run = Some(run.clone());
                true
            } else {
                false
            }
        };
        if !committed {
            if stream_started_by_us
                && let Some(Ok(r)) = &client
                && matches!(self.rig_streaming(r).await, Ok(true))
            {
                let _ = self.rig_set_streaming(r, false).await;
            }
            return Err(EngineError::Conflict("Show start was cancelled.".into()));
        }
        guard.committed = true;
        self.emit_show(Some(run.clone()));
        Ok(run)
    }

    async fn switch_step(&self, rig: &Option<Result<Rig, EngineError>>, id: &str, label: &str, scene: &str) -> Step {
        let own;
        let r = match rig {
            Some(Ok(r)) => r,
            Some(Err(e)) => return Step::new(id, label, "failed", e.to_string()),
            None => match self.rig().await {
                Ok(r) => {
                    own = r;
                    &own
                }
                Err(e) => return Step::new(id, label, "failed", e.to_string()),
            },
        };
        // Never onto a scene with no visible source (empty, all hidden, empty/hidden groups):
        // the stream would go black (19 Nov, live: a guessed empty "Main" blacked out at 5:23).
        if self.rig_scene_is_empty(r, scene).await == Some(true) {
            return Step::new(
                id,
                label,
                "skipped",
                format!("\"{scene}\" is empty, so the on-air scene stays. Pick another in Show settings."),
            );
        }
        match self.rig_switch(r, scene).await {
            Ok(()) => Step::new(id, label, "done", format!("{} is on \"{scene}\".", r.title())),
            Err(e) => Step::new(id, label, "failed", e.to_string()),
        }
    }

    /// Ends the countdown now: switch to the live scene.
    pub async fn skip_countdown(&self) -> Result<ShowRun, EngineError> {
        self.go_live_now(false).await
    }

    /// Stops the countdown; OBS stays where it is. Synchronous and serialized through the
    /// locked `BroadcastMachine`, so an in-flight `go_live_now` cannot resurrect `Live`.
    pub fn cancel_countdown(&self) -> Result<ShowRun, EngineError> {
        let run = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if m.phase != BroadcastPhase::Countdown {
                return Err(NO_COUNTDOWN);
            }
            let Some(mut run) = m.run.clone() else {
                return Err(NO_COUNTDOWN);
            };
            m.epoch += 1;
            m.phase = BroadcastPhase::Starting;
            m.countdown = None;
            m.transitioning_live = None;
            run.phase = Phase::Starting;
            run.countdown_ends_at = None;
            if let Some(s) = run.steps.iter_mut().find(|s| s.id == "countdown") {
                s.status = "skipped".into();
                s.detail = "Cancelled.".into();
            }
            m.run = Some(run.clone());
            run
        };
        self.emit_show(Some(run.clone()));
        Ok(run)
    }

    /// Switch to the live scene now (from Starting or at the end of the countdown).
    pub async fn go_live_scene(&self) -> Result<ShowRun, EngineError> {
        self.go_live_now(true).await
    }

    /// Serialized Countdown/Starting → Live transition. Claims `transitioning_live` under
    /// `BroadcastMachine` so racing callers (countdown expiry + Skip click) never post chat
    /// twice, and checks generation before every step and before committing so a cancelled
    /// countdown or stopped Show can never be resurrected.
    async fn go_live_now(&self, allow_starting: bool) -> Result<ShowRun, EngineError> {
        let (my_gen, mut run) = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            let phase_ok =
                m.phase == BroadcastPhase::Countdown || (allow_starting && m.phase == BroadcastPhase::Starting);
            if !phase_ok || m.transitioning_live.is_some() {
                return Err(NO_COUNTDOWN);
            }
            let Some(run) = m.run.clone() else {
                return Err(NO_COUNTDOWN);
            };
            m.epoch += 1;
            let g = m.epoch;
            m.transitioning_live = Some(g);
            m.countdown = None;
            (g, run)
        };
        let mut guard = LiveTransitionGuard { engine: self, epoch: my_gen, committed: false };

        let cfg = self.show_config();
        let z = &cfg.at_zero;
        run.steps.retain(|s| s.id != "countdown" && !s.id.starts_with("zero_") && s.id != "live_scene");

        if z.switch {
            if !self.is_live_transition(my_gen) {
                return Ok(self.show_run().unwrap_or(run));
            }
            run.steps.push(match &run.live_scene {
                Some(scene) => self.switch_step(&None, "live_scene", "Live scene", scene).await,
                None => Step::new("live_scene", "Live scene", "skipped", "No live scene picked."),
            });
        }
        if !self.is_live_transition(my_gen) {
            return Ok(self.show_run().unwrap_or(run));
        }

        let client = if z.unmute || z.start_stream || z.record { Some(self.rig().await) } else { None };
        if z.unmute {
            if !self.is_live_transition(my_gen) {
                return Ok(self.show_run().unwrap_or(run));
            }
            run.steps.push(match &client {
                Some(Ok(r)) => {
                    let mut bad = Vec::new();
                    for m in &cfg.mic_sources {
                        if self.rig_set_muted(r, m, false).await.is_err() {
                            bad.push(m.clone());
                        }
                    }
                    if cfg.mic_sources.is_empty() {
                        Step::new("zero_unmute", "Mic", "skipped", "No mic is picked in the pre-flight.")
                    } else if bad.is_empty() {
                        Step::new("zero_unmute", "Mic", "done", "On.")
                    } else {
                        Step::new("zero_unmute", "Mic", "failed", format!("{} has no {}.", r.title(), bad.join(", ")))
                    }
                }
                Some(Err(e)) => Step::new("zero_unmute", "Mic", "failed", e.to_string()),
                None => Step::new("zero_unmute", "Mic", "skipped", ""),
            });
        }
        if z.start_stream {
            if !self.is_live_transition(my_gen) {
                return Ok(self.show_run().unwrap_or(run));
            }
            run.steps.push(match &client {
                Some(Ok(r)) => match self.rig_streaming(r).await {
                    Ok(true) => {
                        Step::new("zero_stream", "Go live", "skipped", format!("{} is streaming already.", r.title()))
                    }
                    _ => match self.rig_set_streaming(r, true).await {
                        Ok(()) => Step::new("zero_stream", "Go live", "done", format!("{} is streaming.", r.title())),
                        Err(e) => Step::new("zero_stream", "Go live", "failed", e.to_string()),
                    },
                },
                Some(Err(e)) => Step::new("zero_stream", "Go live", "failed", e.to_string()),
                None => Step::new("zero_stream", "Go live", "skipped", ""),
            });
        }
        if z.record {
            if !self.is_live_transition(my_gen) {
                return Ok(self.show_run().unwrap_or(run));
            }
            run.steps.push(match &client {
                Some(Ok(r)) => match self.rig_start_record(r).await {
                    Ok(()) => Step::new("zero_record", "Recording", "done", format!("{} is recording.", r.title())),
                    Err(e) => Step::new("zero_record", "Recording", "failed", e.to_string()),
                },
                Some(Err(e)) => Step::new("zero_record", "Recording", "failed", e.to_string()),
                None => Step::new("zero_record", "Recording", "skipped", ""),
            });
        }
        if let Some(text) = &z.chat {
            if !self.is_live_transition(my_gen) {
                return Ok(self.show_run().unwrap_or(run));
            }
            let connected: Vec<Platform> =
                Platform::CHAT.iter().copied().filter(|p| self.connector(*p).is_some()).collect();
            run.steps.push(if connected.is_empty() {
                Step::new("zero_chat", "Chat", "skipped", "No chat is connected.")
            } else {
                match self.send_chat(text, &connected, None).await {
                    Ok(o) if !o.ok.is_empty() && o.failed.is_empty() => {
                        Step::new("zero_chat", "Chat", "done", text.clone())
                    }
                    // Said on some platforms: done, with the ones that refused and why.
                    Ok(o) if !o.ok.is_empty() => Step::new(
                        "zero_chat",
                        "Chat",
                        "done",
                        o.failed
                            .iter()
                            .map(|f| format!("{}: {}", f.platform.label(), f.reason))
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    Ok(o) => Step::new(
                        "zero_chat",
                        "Chat",
                        "failed",
                        o.failed
                            .iter()
                            .map(|f| format!("{}: {}", f.platform.label(), f.reason))
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    Err(e) => Step::new("zero_chat", "Chat", "failed", e.to_string()),
                }
            });
        }
        if z.marker && self.is_live_transition(my_gen) {
            let _ = self.add_marker_now(ferrocast_core::moments::MomentKind::Manual, "Live", None, false).await;
        }

        // Only commit `Live` if the transition wasn't cancelled, ended, or stopped meanwhile.
        let committed = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if m.epoch == my_gen && m.transitioning_live == Some(my_gen) {
                m.transitioning_live = None;
                m.phase = BroadcastPhase::Live;
                run.phase = Phase::Live;
                run.countdown_ends_at = None;
                m.run = Some(run.clone());
                true
            } else {
                false
            }
        };
        if !committed {
            return Ok(self.show_run().unwrap_or(run));
        }
        guard.committed = true;
        self.emit_show(Some(run.clone()));
        Ok(run)
    }

    /// End show: the ending scene, then a thank-you in chat with this stream's supporters.
    pub async fn end_show(&self) -> Result<ShowRun, EngineError> {
        self.end_show_with(false).await
    }

    /// End show; with `at_once` (`{ now: true }` in the API) the stream stops at once, as
    /// OBS's Stop Streaming does (also when the show doesn't stop it by itself): no
    /// `stop_delay_s` wait. On a show that is already ending (its stop waiting) `at_once`
    /// only brings the stop forward without repeating the ending scene or recap.
    pub async fn end_show_with(&self, at_once: bool) -> Result<ShowRun, EngineError> {
        self.end_show_opts(at_once, None).await
    }

    pub(crate) async fn end_show_opts(
        &self,
        at_once: bool,
        stop_stream_override: Option<bool>,
    ) -> Result<ShowRun, EngineError> {
        // Fast path: if a delayed stop is already waiting and the caller asks for `at_once`,
        // stop immediately without repeating the ending scene or thank-you chat.
        let bring_forward = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if m.phase == BroadcastPhase::Ending && !m.ending_in_flight && m.stop_waiting && at_once {
                if let Some(h) = m.stop_task.take() {
                    h.abort();
                }
                m.stop_waiting = false;
                m.ending_in_flight = true;
                m.epoch += 1;
                Some(m.epoch)
            } else {
                None
            }
        };
        if let Some(my_gen) = bring_forward {
            let mut guard = EndingGuard { engine: self, epoch: my_gen, committed: false };
            let step = self.stop_stream_step().await;
            let out = {
                let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
                m.ending_in_flight = false;
                if m.epoch == my_gen {
                    m.phase = BroadcastPhase::Idle;
                    if let Some(r) = m.run.as_mut() {
                        match r.steps.iter_mut().find(|s| s.id == "stop_stream") {
                            Some(s) => *s = step,
                            None => r.steps.push(step),
                        }
                        Some(r.clone())
                    } else {
                        None
                    }
                } else {
                    m.run.clone()
                }
            };
            guard.committed = true;
            if let Some(run) = out {
                self.emit_show(Some(run.clone()));
                return Ok(run);
            }
            return Ok(ShowRun {
                id: ferrocast_store::new_id(),
                phase: Phase::Ended,
                started_at: now().0,
                countdown_ends_at: None,
                live_scene: None,
                ended_at: Some(now().0),
                steps: Vec::new(),
            });
        }

        let (my_gen, was_preparing, mut run) = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if m.ending_in_flight {
                return Err(EngineError::Conflict("The show is already ending.".into()));
            }
            if let Some(h) = m.stop_task.take() {
                h.abort();
            }
            let was_preparing = m.phase == BroadcastPhase::Preparing;
            m.stop_waiting = false;
            m.countdown = None;
            m.transitioning_live = None;
            m.epoch += 1;
            let g = m.epoch;
            m.phase = BroadcastPhase::Ending;
            m.ending_in_flight = true;
            let run = m.run.clone().filter(|r| r.phase != Phase::Ended).unwrap_or(ShowRun {
                id: ferrocast_store::new_id(),
                phase: Phase::Live,
                started_at: self.stream_started_at().unwrap_or(now().0),
                countdown_ends_at: None,
                live_scene: None,
                ended_at: None,
                steps: Vec::new(),
            });
            (g, was_preparing, run)
        };
        let mut guard = EndingGuard { engine: self, epoch: my_gen, committed: false };

        let cfg = self.show_config();
        run.steps.clear();
        if !was_preparing {
            if let Some(scene) = &cfg.ending_scene
                && self.is_show_epoch(my_gen)
            {
                run.steps.push(self.switch_step(&None, "ending_scene", "Ending scene", scene).await);
            }
            if let Some(tpl) = &cfg.recap_message
                && self.is_show_epoch(my_gen)
            {
                let recap = self.recap(None).await.ok();
                let (thanks, follows, subs) =
                    recap.map(|r| (r.thanks, r.totals.follows, r.totals.subs)).unwrap_or_default();
                let duration = now().0 - self.stream_started_at().unwrap_or(run.started_at);
                let vars = show::recap_vars(&thanks, follows, subs, duration);
                let text = Template::parse(tpl).map(|t| t.render(&vars)).unwrap_or_default();
                let connected: Vec<Platform> =
                    Platform::CHAT.iter().copied().filter(|p| self.connector(*p).is_some()).collect();
                run.steps.push(if connected.is_empty() {
                    Step::new("recap", "Thank-you message", "skipped", "No chat is connected.")
                } else {
                    match self.send_chat(&text, &connected, None).await {
                        Ok(o) if o.failed.is_empty() => Step::new("recap", "Thank-you message", "done", text),
                        Ok(o) => Step::new(
                            "recap",
                            "Thank-you message",
                            if o.ok.is_empty() { "failed" } else { "done" },
                            o.failed
                                .iter()
                                .map(|f| format!("{}: {}", f.platform.label(), f.reason))
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                        Err(e) => Step::new("recap", "Thank-you message", "failed", e.to_string()),
                    }
                });
            }
            if self.is_show_epoch(my_gen) {
                let _ =
                    self.add_marker_now(ferrocast_core::moments::MomentKind::Manual, "Show ended", None, false).await;
            }
        }

        let want_stop = at_once || stop_stream_override.unwrap_or(cfg.stop_stream);
        let mut scheduled_delay: Option<u32> = None;
        if want_stop && self.is_show_epoch(my_gen) {
            if at_once || cfg.stop_delay_s == 0 || was_preparing {
                run.steps.push(self.stop_stream_step().await);
            } else {
                let delay = cfg.stop_delay_s;
                run.steps.push(Step::new(
                    "stop_stream",
                    "Stop streaming",
                    "waiting",
                    format!("Streaming stops in {delay} seconds."),
                ));
                scheduled_delay = Some(delay);
            }
        }

        run.phase = Phase::Ended;
        run.ended_at = Some(now().0);
        run.countdown_ends_at = None;

        let committed = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            m.ending_in_flight = false;
            if m.epoch == my_gen {
                m.started_stream = false;
                m.run = Some(run.clone());
                if let Some(delay) = scheduled_delay {
                    let stream_gen = m.stream_epoch;
                    let engine = self.clone();
                    let id = run.id.clone();
                    let handle = tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(u64::from(delay))).await;
                        engine.stop_stream_later(my_gen, stream_gen, &id).await;
                    });
                    m.stop_task = Some(handle.abort_handle());
                    m.stop_waiting = true;
                    m.phase = BroadcastPhase::Ending;
                } else {
                    m.stop_waiting = false;
                    m.phase = BroadcastPhase::Idle;
                }
                true
            } else {
                false
            }
        };
        guard.committed = true;
        if !committed {
            return Ok(self.show_run().unwrap_or(run));
        }
        self.emit_show(Some(run.clone()));
        Ok(run)
    }
}

impl Engine {
    async fn stop_stream_step(&self) -> Step {
        let rig = match self.rig().await {
            Ok(r) => r,
            Err(e) => return Step::new("stop_stream", "Stop streaming", "failed", e.to_string()),
        };
        match self.rig_set_streaming(&rig, false).await {
            Ok(()) => Step::new("stop_stream", "Stop streaming", "done", format!("{} stopped streaming.", rig.title())),
            Err(e) => Step::new("stop_stream", "Stop streaming", "failed", e.to_string()),
        }
    }

    async fn stop_stream_step_locked(&self) -> Step {
        let rig = match self.rig().await {
            Ok(r) => r,
            Err(e) => return Step::new("stop_stream", "Stop streaming", "failed", e.to_string()),
        };
        match self.rig_set_streaming_locked(&rig, false).await {
            Ok(()) => Step::new("stop_stream", "Stop streaming", "done", format!("{} stopped streaming.", rig.title())),
            Err(e) => Step::new("stop_stream", "Stop streaming", "failed", e.to_string()),
        }
    }

    /// The delayed Stop Streaming of an ended show. Serialized with `output_lock` and guarded
    /// by BOTH `show_gen`, `stream_gen`, and a still-waiting `stop_stream` step, so it never
    /// fires after "stop now", a new Show, or a plain stream started without a Show.
    async fn stop_stream_later(&self, show_gen: u64, stream_gen: u64, show_id: &str) {
        let _out = self.studio().output_lock.lock().await;
        let still_ours = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            let waiting = |r: &ShowRun| {
                r.id == show_id
                    && r.phase == Phase::Ended
                    && r.steps.iter().any(|s| s.id == "stop_stream" && s.status == "waiting")
            };
            let ok = m.epoch == show_gen
                && m.stream_epoch == stream_gen
                && m.stop_waiting
                && m.run.as_ref().is_some_and(waiting);
            if ok {
                m.stop_task = None;
                m.stop_waiting = false;
            }
            ok
        };
        if !still_ours {
            return;
        }
        let step = self.stop_stream_step_locked().await;
        let emit = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if m.epoch == show_gen && m.phase == BroadcastPhase::Ending {
                m.phase = BroadcastPhase::Idle;
            }
            if let Some(r) = m.run.as_mut().filter(|r| r.id == show_id) {
                match r.steps.iter_mut().find(|s| s.id == "stop_stream") {
                    Some(s) => *s = step,
                    None => r.steps.push(step),
                }
                Some(r.clone())
            } else {
                None
            }
        };
        if let Some(run) = emit {
            self.emit_show(Some(run));
        }
    }

    /// Stops the Studio stream and ends any active Show when every enabled destination has
    /// permanently failed (`PushState::Failed`), without posting a thank-you recap.
    pub(crate) async fn abort_live_on_destination_failure(&self, reason: &str) {
        if let Err(e) = self.studio_stream(false).await {
            tracing::warn!(error = %e, "couldn't stop Studio stream after destination failure");
        }
        let emit = {
            let mut m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = m.stop_task.take() {
                h.abort();
            }
            m.stop_waiting = false;
            m.countdown = None;
            m.transitioning_live = None;
            m.started_stream = false;
            m.ending_in_flight = false;
            if m.phase != BroadcastPhase::Idle {
                m.epoch += 1;
                m.phase = BroadcastPhase::Idle;
            }
            if let Some(r) = m.run.as_mut().filter(|r| r.phase != Phase::Ended) {
                r.phase = Phase::Ended;
                r.ended_at = Some(now().0);
                r.countdown_ends_at = None;
                r.steps.push(Step::new("stream", "Go live", "failed", reason));
                Some(r.clone())
            } else {
                None
            }
        };
        if let Some(run) = emit {
            self.emit_show(Some(run));
        }
    }

    /// Whether OBS is streaming (`GET /api/v1/obs/stream`).
    pub async fn obs_streaming(&self) -> Result<bool, EngineError> {
        let c = self.obs_client().await?;
        ferrocast_obs::control::streaming(&c).await.map_err(super::obs_error)
    }

    /// Presses OBS's Start or Stop Streaming (`POST /api/v1/obs/stream`).
    pub async fn obs_set_streaming(&self, on: bool) -> Result<bool, EngineError> {
        let _out = self.studio().output_lock.lock().await;
        let c = self.obs_client().await?;
        if on {
            ferrocast_obs::control::start_stream(&c).await.map_err(super::obs_error)?;
        } else {
            ferrocast_obs::control::stop_stream(&c).await.map_err(super::obs_error)?;
        }
        // OBS answers before its output has started or stopped: wait for it, so the answer
        // is the new state (a stale one read "stopped" right after Start).
        let deadline = tokio::time::Instant::now() + STREAM_SWITCH_WAIT;
        loop {
            let live = ferrocast_obs::control::streaming(&c).await.map_err(super::obs_error)?;
            if live == on {
                if on {
                    self.on_stream_started();
                }
                return Ok(live);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(EngineError::Refused {
                    status: 422,
                    code: "obs_stream_stuck",
                    message: if on { "OBS didn't start streaming." } else { "OBS didn't stop streaming." }.into(),
                    hint: "Look at OBS: it says why (often the server or the stream key).".into(),
                    retry_after: None,
                });
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// How long Start or Stop Streaming may take before OBS is told to say why.
const STREAM_SWITCH_WAIT: Duration = Duration::from_secs(5);

const NO_COUNTDOWN: EngineError = EngineError::Missing {
    code: "no_countdown",
    message: "There's no countdown running.",
    hint: "Start the show first.",
};

/// Switches to the live scene when a monotonic countdown reaches zero.
/// Ticks at most once per second so sleep/wake clock jumps are detected and clamped rather
/// than blasting straight through the countdown into the live scene.
pub(crate) async fn countdown_loop(engine: Engine) {
    loop {
        let (wait, resync_run) = {
            let mut m = engine.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            if m.phase == BroadcastPhase::Countdown && m.transitioning_live.is_none() {
                if let Some(timer) = m.countdown.as_mut() {
                    let (rem, jumped) = timer.advance(tokio::time::Instant::now());
                    let resync = if jumped && let Some(run) = m.run.as_mut() {
                        run.countdown_ends_at = Some(now().0 + i64::try_from(rem.as_millis()).unwrap_or(0));
                        Some(run.clone())
                    } else {
                        None
                    };
                    (Some(rem), resync)
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        };
        if let Some(run) = resync_run {
            engine.emit_show(Some(run));
        }
        match wait {
            Some(rem) if rem.is_zero() => {
                let _ = engine.go_live_now(false).await;
            }
            Some(rem) => {
                let step = rem.min(Duration::from_secs(1));
                tokio::select! {
                    () = tokio::time::sleep(step) => {}
                    () = engine.studio().show_wake.notified() => {}
                }
            }
            None => engine.studio().show_wake.notified().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_timer_clamps_wake_jumps_and_expires_normally() {
        let t0 = tokio::time::Instant::now();
        let mut timer = CountdownTimer::start(Duration::from_secs(10), t0);
        // Normal 1 s tick reduces remaining by 1 s.
        let (rem, jumped) = timer.advance(t0 + Duration::from_secs(1));
        assert!(!jumped);
        assert_eq!(rem, Duration::from_secs(9));
        // A 5-minute sleep/wake jump is clamped to 1 s instead of firing go_live_now early.
        let (rem, jumped) = timer.advance(t0 + Duration::from_secs(301));
        assert!(jumped);
        assert_eq!(rem, Duration::from_secs(8));
        // Normal ticks finish cleanly to zero.
        let mut t = t0 + Duration::from_secs(301);
        for _ in 0..8 {
            t += Duration::from_secs(1);
            let (_, jumped) = timer.advance(t);
            assert!(!jumped);
        }
        let (rem, _) = timer.advance(t);
        assert!(rem.is_zero());
    }
}
