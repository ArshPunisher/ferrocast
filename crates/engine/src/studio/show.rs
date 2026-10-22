//! "Start show" and "End show" (API.md §4 "Studio: show"): the go-live checklist the
//! streamer sets up once, pre-flight checks with one-click fixes, and the countdown that
//! switches OBS to the live scene at zero. Every step reports what it did; one step failing
//! never stops the others (OBS closed shouldn't stop the stream info going out).

use std::time::Duration;

use ferrocast_core::event::Platform;
use ferrocast_core::show::{self, Check, CheckStatus, ShowConfig, fix};
use ferrocast_core::template::Template;
use serde::Serialize;

use super::{SHOW_KEY, save_setting};
use crate::{Engine, EngineError, now, platform_key};

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
    fn new(id: &str, label: &str, status: &str, detail: impl Into<String>) -> Step {
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

#[derive(Debug, Clone, Serialize)]
pub struct Preflight {
    pub checks: Vec<Check>,
    pub ready: bool,
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
        self.studio().show.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn set_show(&self, run: Option<ShowRun>) {
        *self.studio().show.lock().unwrap_or_else(|p| p.into_inner()) = run.clone();
        self.studio_frame("show", &serde_json::json!({ "show": run }));
        self.studio().show_wake.notify_one();
    }

    /// Everything that could go wrong at go-live, each with how to fix it.
    pub async fn preflight(&self) -> Preflight {
        let cfg = self.show_config();
        let mut checks = Vec::new();
        let client = self.obs_client().await;
        match &client {
            Ok(_) => checks.push(Check::new("obs", "OBS", CheckStatus::Ok, "Connected.", None)),
            Err(e) => checks.push(Check::new(
                "obs",
                "OBS",
                CheckStatus::Fail,
                e.to_string(),
                fix("OBS settings", "#/settings/obs"),
            )),
        }
        if let Ok(c) = &client {
            match ferrocast_obs::control::scene_names(c).await {
                Ok((_, names)) => {
                    for (id, label, scene) in [
                        ("starting_scene", "Starting scene", &cfg.starting_scene),
                        ("live_scene", "Live scene", &cfg.live_scene),
                        ("ending_scene", "Ending scene", &cfg.ending_scene),
                    ] {
                        let Some(s) = scene else { continue };
                        checks.push(if names.iter().any(|n| n == s) {
                            Check::new(id, label, CheckStatus::Ok, format!("\"{s}\" is in OBS."), None)
                        } else {
                            Check::new(
                                id,
                                label,
                                CheckStatus::Fail,
                                format!("OBS has no scene called \"{s}\" (renamed or deleted?)."),
                                fix("Pick the scene again", "#/studio/show"),
                            )
                        });
                    }
                }
                Err(e) => checks.push(Check::new("scenes", "Scenes", CheckStatus::Warn, e.to_string(), None)),
            }
            for m in &cfg.mic_sources {
                checks.push(match ferrocast_obs::control::muted(c, m).await {
                    Ok(false) => Check::new(&format!("mic:{m}"), m, CheckStatus::Ok, "Not muted.", None),
                    Ok(true) => Check::new(
                        &format!("mic:{m}"),
                        m,
                        CheckStatus::Fail,
                        "Muted in OBS: nobody will hear you.",
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
            let scene = cfg.live_scene.clone().or(cfg.starting_scene.clone());
            for cam in &cfg.cam_sources {
                let Some(scene) = &scene else { break };
                checks.push(match ferrocast_obs::control::source_visible(c, scene, cam).await {
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
            if cfg.replay_buffer {
                checks.push(match ferrocast_obs::control::replay_buffer_active(c).await {
                    Ok(true) => Check::new("replay", "Replay buffer", CheckStatus::Ok, "Running.", None),
                    Ok(false) => Check::new("replay", "Replay buffer", CheckStatus::Ok, "Starts with the show.", None),
                    Err(e) => Check::new("replay", "Replay buffer", CheckStatus::Warn, e.to_string(), None),
                });
            }
        }
        let disk = match &client {
            Ok(c) => ferrocast_obs::control::health_sample(c, now().0).await.ok().map(|s| s.disk_mb),
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
                "No platform is connected: chat, alerts and stream info won't work.",
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
                    "No Discord webhook is set (or posts are off), so nothing is posted.",
                    fix("Discord settings", "#/settings/discord"),
                ));
            }
        }
        let ready = show::ready(&checks);
        Preflight { checks, ready }
    }

    /// Runs a pre-flight fix: `unmute:<input>`, `show:<scene>/<source>`, `start_replay_buffer`.
    pub async fn preflight_fix(&self, action: &str) -> Result<Preflight, EngineError> {
        let c = self.obs_client().await?;
        if let Some(input) = action.strip_prefix("unmute:") {
            ferrocast_obs::control::set_muted(&c, input, false).await.map_err(super::obs_error)?;
        } else if let Some(rest) = action.strip_prefix("show:") {
            let (scene, source) = rest
                .split_once('/')
                .ok_or_else(|| EngineError::Invalid("action: use show:<scene>/<source>.".into()))?;
            let id = ferrocast_obs::control::item_id(&c, scene, source)
                .await
                .map_err(super::obs_error)?
                .ok_or_else(|| EngineError::Invalid(format!("action: \"{source}\" isn't in \"{scene}\".")))?;
            ferrocast_obs::control::set_visible(&c, scene, id, true).await.map_err(super::obs_error)?;
        } else if action == "start_replay_buffer" {
            ferrocast_obs::control::start_replay_buffer(&c).await.map_err(super::obs_error)?;
        } else {
            return Err(EngineError::Invalid(
                "action: unmute:<input>, show:<scene>/<source> or start_replay_buffer.".into(),
            ));
        }
        Ok(self.preflight().await)
    }

    /// Starts the show. 409 while one is running; 422 `preflight_failed` when a check
    /// failed, unless `force`.
    pub async fn start_show(&self, force: bool) -> Result<ShowRun, EngineError> {
        if self.show_run().is_some_and(|r| r.phase != Phase::Ended) {
            return Err(EngineError::Conflict(
                "A show is already running. End it first, or cancel the countdown.".into(),
            ));
        }
        if !force {
            let pf = self.preflight().await;
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
        let at = now().0;
        let mut steps = Vec::new();

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
        let client =
            if cfg.starting_scene.is_some() || cfg.live_scene.is_some() || cfg.replay_buffer || cfg.start_stream {
                Some(self.obs_client().await)
            } else {
                None
            };
        if cfg.replay_buffer {
            steps.push(match &client {
                Some(Ok(c)) => match ferrocast_obs::control::start_replay_buffer(c).await {
                    Ok(()) => Step::new("replay", "Replay buffer", "done", "Running, so \"clip that\" works."),
                    Err(e) => Step::new("replay", "Replay buffer", "failed", e.to_string()),
                },
                Some(Err(e)) => Step::new("replay", "Replay buffer", "failed", e.to_string()),
                None => Step::new("replay", "Replay buffer", "skipped", ""),
            });
        }
        if let Some(scene) = &cfg.starting_scene {
            steps.push(self.switch_step(&client, "starting_scene", "Starting scene", scene).await);
        }
        if cfg.start_stream {
            steps.push(match &client {
                Some(Ok(c)) => match ferrocast_obs::control::start_stream(c).await {
                    Ok(()) => Step::new("stream", "Go live in OBS", "done", "OBS is streaming."),
                    Err(e) => Step::new("stream", "Go live in OBS", "failed", e.to_string()),
                },
                Some(Err(e)) => Step::new("stream", "Go live in OBS", "failed", e.to_string()),
                None => Step::new("stream", "Go live in OBS", "skipped", ""),
            });
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
        let (phase, ends) = match (&cfg.live_scene, cfg.countdown_s) {
            (Some(_), n) if n > 0 => {
                steps.push(Step::new(
                    "countdown",
                    "Countdown",
                    "waiting",
                    format!("{n} seconds, then the live scene."),
                ));
                (Phase::Countdown, Some(at + i64::from(n) * 1000))
            }
            (Some(scene), _) => {
                steps.push(self.switch_step(&client, "live_scene", "Live scene", scene).await);
                (Phase::Live, None)
            }
            (None, _) => (Phase::Starting, None),
        };
        let run = ShowRun {
            id: ferrocast_store::new_id(),
            phase,
            started_at: at,
            countdown_ends_at: ends,
            live_scene: cfg.live_scene.clone(),
            ended_at: None,
            steps,
        };
        self.set_show(Some(run.clone()));
        Ok(run)
    }

    async fn switch_step(
        &self,
        client: &Option<Result<ferrocast_obs::Client, EngineError>>,
        id: &str,
        label: &str,
        scene: &str,
    ) -> Step {
        match client {
            Some(Ok(c)) => match ferrocast_obs::control::switch_scene(c, scene).await {
                Ok(()) => Step::new(id, label, "done", format!("OBS is on \"{scene}\".")),
                Err(e) => Step::new(id, label, "failed", e.to_string()),
            },
            Some(Err(e)) => Step::new(id, label, "failed", e.to_string()),
            None => match self.obs_client().await {
                Ok(c) => match ferrocast_obs::control::switch_scene(&c, scene).await {
                    Ok(()) => Step::new(id, label, "done", format!("OBS is on \"{scene}\".")),
                    Err(e) => Step::new(id, label, "failed", e.to_string()),
                },
                Err(e) => Step::new(id, label, "failed", e.to_string()),
            },
        }
    }

    /// Ends the countdown now: switch to the live scene.
    pub async fn skip_countdown(&self) -> Result<ShowRun, EngineError> {
        let run = self.show_run().filter(|r| r.phase == Phase::Countdown).ok_or(NO_COUNTDOWN)?;
        Ok(self.go_live_now(run).await)
    }

    /// Stops the countdown; OBS stays where it is.
    pub fn cancel_countdown(&self) -> Result<ShowRun, EngineError> {
        let mut run = self.show_run().filter(|r| r.phase == Phase::Countdown).ok_or(NO_COUNTDOWN)?;
        run.phase = Phase::Starting;
        run.countdown_ends_at = None;
        if let Some(s) = run.steps.iter_mut().find(|s| s.id == "countdown") {
            s.status = "skipped".into();
            s.detail = "Cancelled.".into();
        }
        self.set_show(Some(run.clone()));
        Ok(run)
    }

    /// Switch to the live scene now (from Starting or at the end of the countdown).
    pub async fn go_live_scene(&self) -> Result<ShowRun, EngineError> {
        let run =
            self.show_run().filter(|r| matches!(r.phase, Phase::Countdown | Phase::Starting)).ok_or(NO_COUNTDOWN)?;
        Ok(self.go_live_now(run).await)
    }

    async fn go_live_now(&self, mut run: ShowRun) -> ShowRun {
        let step = match &run.live_scene {
            Some(scene) => self.switch_step(&None, "live_scene", "Live scene", scene).await,
            None => Step::new("live_scene", "Live scene", "skipped", "No live scene picked."),
        };
        run.steps.retain(|s| s.id != "countdown" && s.id != "live_scene");
        run.steps.push(step);
        run.phase = Phase::Live;
        run.countdown_ends_at = None;
        self.set_show(Some(run.clone()));
        run
    }

    /// End show: the ending scene, then a thank-you in chat with this stream's supporters.
    pub async fn end_show(&self) -> Result<ShowRun, EngineError> {
        let cfg = self.show_config();
        let mut run = self.show_run().filter(|r| r.phase != Phase::Ended).unwrap_or(ShowRun {
            id: ferrocast_store::new_id(),
            phase: Phase::Live,
            started_at: self.stream_started_at().unwrap_or(now().0),
            countdown_ends_at: None,
            live_scene: None,
            ended_at: None,
            steps: Vec::new(),
        });
        run.steps.clear();
        if let Some(scene) = &cfg.ending_scene {
            run.steps.push(self.switch_step(&None, "ending_scene", "Ending scene", scene).await);
        }
        if let Some(tpl) = &cfg.recap_message {
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
        let _ = self.add_marker_now(ferrocast_core::moments::MomentKind::Manual, "Show ended", None, false).await;
        if cfg.stop_stream {
            run.steps.push(if cfg.stop_delay_s == 0 {
                self.stop_stream_step().await
            } else {
                let engine = self.clone();
                let (id, delay) = (run.id.clone(), cfg.stop_delay_s);
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(u64::from(delay))).await;
                    engine.stop_stream_later(&id).await;
                });
                Step::new(
                    "stop_stream",
                    "Stop streaming",
                    "waiting",
                    format!("OBS stops streaming in {delay} seconds."),
                )
            });
        }
        run.phase = Phase::Ended;
        run.ended_at = Some(now().0);
        run.countdown_ends_at = None;
        self.set_show(Some(run.clone()));
        Ok(run)
    }
}

impl Engine {
    async fn stop_stream_step(&self) -> Step {
        let r = match self.obs_client().await {
            Ok(c) => ferrocast_obs::control::stop_stream(&c).await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match r {
            Ok(()) => Step::new("stop_stream", "Stop streaming", "done", "OBS stopped streaming."),
            Err(e) => Step::new("stop_stream", "Stop streaming", "failed", e),
        }
    }

    /// The delayed Stop Streaming of an ended show. Does nothing if another show started
    /// meanwhile (the streamer went live again).
    async fn stop_stream_later(&self, show_id: &str) {
        let current = self.show_run();
        if current.as_ref().is_none_or(|r| r.id != show_id || r.phase != Phase::Ended) {
            return;
        }
        let step = self.stop_stream_step().await;
        if let Some(mut run) = self.show_run().filter(|r| r.id == show_id) {
            match run.steps.iter_mut().find(|s| s.id == "stop_stream") {
                Some(s) => *s = step,
                None => run.steps.push(step),
            }
            self.set_show(Some(run));
        }
    }

    /// Whether OBS is streaming (`GET /api/v1/obs/stream`).
    pub async fn obs_streaming(&self) -> Result<bool, EngineError> {
        let c = self.obs_client().await?;
        ferrocast_obs::control::streaming(&c).await.map_err(super::obs_error)
    }

    /// Presses OBS's Start or Stop Streaming (`POST /api/v1/obs/stream`).
    pub async fn obs_set_streaming(&self, on: bool) -> Result<bool, EngineError> {
        let c = self.obs_client().await?;
        if on {
            ferrocast_obs::control::start_stream(&c).await.map_err(super::obs_error)?;
        } else {
            ferrocast_obs::control::stop_stream(&c).await.map_err(super::obs_error)?;
        }
        ferrocast_obs::control::streaming(&c).await.map_err(super::obs_error)
    }
}

const NO_COUNTDOWN: EngineError = EngineError::Missing {
    code: "no_countdown",
    message: "There's no countdown running.",
    hint: "Start the show first.",
};

/// Switches to the live scene when a countdown reaches zero.
pub(crate) async fn countdown_loop(engine: Engine) {
    loop {
        let wait = engine
            .show_run()
            .filter(|r| r.phase == Phase::Countdown)
            .and_then(|r| r.countdown_ends_at)
            .map(|end| Duration::from_millis(u64::try_from((end - now().0).max(0)).unwrap_or(0)));
        match wait {
            Some(w) if w.is_zero() => {
                if let Some(run) = engine.show_run().filter(|r| r.phase == Phase::Countdown) {
                    engine.go_live_now(run).await;
                }
            }
            Some(w) => {
                tokio::select! {
                    () = tokio::time::sleep(w) => {}
                    () = engine.studio().show_wake.notified() => {}
                }
            }
            None => engine.studio().show_wake.notified().await,
        }
    }
}
