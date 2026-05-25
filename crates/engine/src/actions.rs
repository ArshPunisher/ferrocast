//! What the dashboard can ask the engine to do: tests, chat, moderation, panic mode, and
//! saving rules, goals, commands and settings. Each save validates, persists, then updates
//! the in-memory state the pipeline reads.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ferrocast_connector::{Connector, ConnectorError, max_chat_chars, split_chat};
use ferrocast_core::alert::AlertRule;
use ferrocast_core::command::{Command, Timer};
use ferrocast_core::event::{Event, Millis, Origin, Platform};
use ferrocast_core::filter::FilterConfig;
use ferrocast_core::goal::Goal;
use ferrocast_core::moderation::{BanEntry, PanicConfig, PanicPlan, plan_panic};
use ferrocast_core::sample;
use ferrocast_core::tts::TtsPolicy;
use ferrocast_store::repo;
use serde::Serialize;
use serde_json::json;

use crate::frames::{LiveFrame, Notice, NoticeLevel};
use crate::state::Why;
use crate::{Engine, EngineError, now};

type ActionFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ConnectorError>> + Send + 'a>>;

/// Result of an action across platforms.
#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
pub struct Outcome {
    pub ok: Vec<Platform>,
    pub failed: Vec<Failure>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Failure {
    pub platform: Platform,
    pub reason: String,
}

impl Engine {
    /// Runs a platform action in the background and reports failures as a notice. Used by
    /// the pipeline, which must never wait on the network.
    pub(crate) fn spawn_action<F>(&self, platform: Platform, what: &'static str, f: F)
    where
        F: for<'a> FnOnce(&'a Arc<dyn Connector>) -> ActionFuture<'a> + Send + 'static,
    {
        let Some(c) = self.connector(platform) else {
            return;
        };
        let engine = self.clone();
        tokio::spawn(async move {
            if let Err(err) = f(&c).await {
                tracing::warn!(%err, what, "platform action failed");
                engine.send_live(LiveFrame::new(
                    "notice",
                    &Notice {
                        level: NoticeLevel::Warn,
                        message: format!("Couldn't {what} on {platform}: {err}"),
                        action: None,
                    },
                ));
            }
        });
    }

    /// Fires a test event of `kind` through the real pipeline (API `POST /test`).
    pub async fn test(&self, kind: &str, platform: Option<Platform>) -> Result<(), EngineError> {
        let at = now();
        let event = match platform {
            Some(p) => sample::sample(kind, p, at),
            None => sample::any_sample(kind, at),
        }
        .ok_or_else(|| EngineError::Invalid(format!("There's no test for \"{kind}\" on that platform.")))?;
        self.submit(event).await
    }

    /// Replays a logged event as a new alert.
    pub async fn replay(&self, event_id: &str) -> Result<(), EngineError> {
        let id = event_id.to_string();
        let mut e = self.db(move |s| repo::events::get(s, &id)).await?.ok_or(EngineError::NotFound("event"))?;
        // Only alerts can be replayed: replaying "stream started" or a ban would re-run
        // their side effects.
        if !e.kind.is_alertable() {
            return Err(EngineError::Invalid("Only events that showed an alert can be replayed.".into()));
        }
        e.origin = Origin::Replay;
        // A replay is a new event; keeping the platform id would make it a duplicate.
        e.source_id = None;
        e.occurred_at = Millis(0);
        self.submit(e).await
    }

    pub fn why(&self, event_id: &str) -> Option<Why> {
        self.state().why(event_id).cloned()
    }

    /// Sends a chat message to some or all connected platforms.
    pub async fn send_chat(
        &self,
        text: &str,
        platforms: &[Platform],
        reply_to: Option<(Platform, String)>,
    ) -> Result<Outcome, EngineError> {
        let text = ferrocast_core::command::safe_reply(text);
        if text.is_empty() {
            return Err(EngineError::Invalid("The message is empty.".into()));
        }
        let targets: Vec<Platform> = if platforms.is_empty() { Platform::CHAT.to_vec() } else { platforms.to_vec() };
        let mut out = Outcome::default();
        for p in targets {
            let Some(c) = self.connector(p) else {
                out.failed.push(Failure { platform: p, reason: format!("{p} isn't connected") });
                continue;
            };
            let reply = reply_to.as_ref().filter(|(rp, _)| *rp == p).map(|(_, id)| id.as_str());
            let mut result = Ok(());
            for chunk in split_chat(&text, max_chat_chars(p)) {
                result = c.send_chat(&chunk, reply).await;
                if result.is_err() {
                    break;
                }
            }
            match result {
                Ok(()) => out.ok.push(p),
                Err(e) => out.failed.push(Failure { platform: p, reason: e.to_string() }),
            }
        }
        Ok(out)
    }

    pub async fn delete_message(&self, platform: Platform, message_id: &str) -> Result<(), EngineError> {
        let c = self.connector(platform).ok_or(ConnectorError::NotConnected(platform))?;
        c.delete_message(message_id).await?;
        self.hide_message(platform, message_id);
        Ok(())
    }

    pub async fn timeout(
        &self,
        platform: Platform,
        user_id: &str,
        seconds: u32,
        reason: Option<&str>,
    ) -> Result<(), EngineError> {
        if !(1..=1_209_600).contains(&seconds) {
            return Err(EngineError::Invalid("Timeouts are 1 second to 14 days.".into()));
        }
        let c = self.connector(platform).ok_or(ConnectorError::NotConnected(platform))?;
        Ok(c.timeout(user_id, seconds, reason).await?)
    }

    /// Bans on the source platform, adds to the shared list, and with `everywhere` bans
    /// the linked accounts on other connected platforms too.
    pub async fn ban(
        &self,
        platform: Platform,
        user_id: &str,
        login: &str,
        reason: Option<String>,
        everywhere: bool,
    ) -> Result<Outcome, EngineError> {
        let mut out = Outcome::default();
        let mut entry = BanEntry {
            platform,
            user_id: Some(user_id.to_string()),
            login: login.to_string(),
            reason: reason.clone(),
            at: now(),
            linked: Vec::new(),
        };
        if everywhere {
            let (p, uid) = (platform, user_id.to_string());
            if let Ok(card) = self.db(move |s| repo::viewers::card(s, p, &uid)).await {
                for id in card.identities {
                    let ip: Platform = serde_json::from_value(json!(id.platform)).unwrap_or(platform);
                    if ip != platform {
                        entry.linked.push((ip, id.user_id));
                    }
                }
            }
        }
        let mut targets = vec![(platform, user_id.to_string())];
        targets.extend(entry.linked.iter().cloned());
        for (p, uid) in targets {
            match self.connector(p) {
                Some(c) => match c.ban(&uid, reason.as_deref()).await {
                    Ok(()) => out.ok.push(p),
                    Err(e) => out.failed.push(Failure { platform: p, reason: e.to_string() }),
                },
                None => out.failed.push(Failure { platform: p, reason: format!("{p} isn't connected") }),
            }
        }
        let _one_at_a_time = self.config_lock().await;
        let mut list = self.state().bans.clone();
        list.add(entry);
        let saved = list.clone();
        self.db(move |s| repo::bot::save_ban_list(s, &saved)).await?;
        self.state_mut().bans = list;
        Ok(out)
    }

    pub async fn unban(&self, platform: Platform, user_id: &str) -> Result<(), EngineError> {
        {
            let _one_at_a_time = self.config_lock().await;
            let mut list = self.state().bans.clone();
            list.remove(platform, user_id);
            let saved = list.clone();
            self.db(move |s| repo::bot::save_ban_list(s, &saved)).await?;
            self.state_mut().bans = list;
        }
        if let Some(c) = self.connector(platform) {
            c.unban(user_id).await?;
        }
        Ok(())
    }

    pub fn bans(&self) -> ferrocast_core::moderation::BanList {
        self.state().bans.clone()
    }

    /// Turns panic mode on or off on every connected platform (R-50).
    pub async fn panic(&self, on: bool) -> Result<PanicPlan, EngineError> {
        let (cfg, connected) = {
            let st = self.state();
            let connected: Vec<_> = self.connectors().iter().map(|c| (c.platform(), c.capabilities())).collect();
            (st.panic.clone(), connected)
        };
        let plan = plan_panic(&cfg, &connected, on);
        for pp in &plan.platforms {
            if let Some(c) = self.connector(pp.platform) {
                for change in &pp.changes {
                    if let Err(err) = c.set_mode(*change).await {
                        self.send_live(LiveFrame::new(
                            "notice",
                            &Notice {
                                level: NoticeLevel::Error,
                                message: format!("Panic mode on {}: {err}", pp.platform),
                                action: None,
                            },
                        ));
                    }
                }
            }
        }
        {
            let mut st = self.state_mut();
            st.panic_on = on;
            st.tts_paused = plan.pause_tts;
        }
        if on && plan.pause_alerts {
            self.queue_pause().await;
        } else if !on && cfg.pause_alerts {
            self.queue_resume().await;
        }
        self.send_live(LiveFrame::new("panic", &json!({ "on": on, "plan": plan })));
        Ok(plan)
    }

    pub fn panic_state(&self) -> (bool, PanicConfig) {
        let st = self.state();
        (st.panic_on, st.panic.clone())
    }

    pub async fn save_panic(&self, cfg: PanicConfig) -> Result<PanicConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let saved = cfg.clone();
        self.db(move |s| repo::settings::set(s, "panic", &saved)).await?;
        self.state_mut().panic = cfg.clone();
        Ok(cfg)
    }

    pub fn filter(&self) -> FilterConfig {
        self.state().filter.clone()
    }

    pub async fn save_filter(&self, cfg: FilterConfig) -> Result<FilterConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        if cfg.terms.len() > 2_000 || cfg.terms.iter().any(|t| t.term.chars().count() > 100) {
            return Err(EngineError::Invalid("Up to 2,000 blocked terms of up to 100 characters each.".into()));
        }
        let saved = cfg.clone();
        self.db(move |s| repo::settings::set(s, "filter", &saved)).await?;
        self.state_mut().filter = cfg.clone();
        Ok(cfg)
    }

    pub fn tts_policy(&self) -> TtsPolicy {
        self.state().tts.clone()
    }

    pub async fn save_tts(&self, p: TtsPolicy) -> Result<TtsPolicy, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        if p.max_chars == 0 || p.max_chars > 500 {
            return Err(EngineError::Invalid("TTS can read 1 to 500 characters.".into()));
        }
        let saved = p.clone();
        self.db(move |s| repo::settings::set(s, "tts", &saved)).await?;
        self.state_mut().tts = p.clone();
        Ok(p)
    }

    /// Approves (reads aloud via the alert overlays) or rejects a held TTS message.
    pub async fn decide_tts(&self, id: &str, approve: bool) -> Result<(), EngineError> {
        let pid = id.to_string();
        let item = self
            .db(move |s| repo::events::take_pending_tts(s, &pid))
            .await?
            .ok_or(EngineError::NotFound("TTS message"))?;
        if approve {
            let audio = self.speak_to_media(&item.text).await;
            self.send_overlay(crate::frames::OverlayFrame::new(
                crate::frames::Audience::Alerts,
                self.next_seq(),
                "tts",
                &json!({ "id": item.id, "text": item.text, "audio": audio }),
            ));
        }
        self.push_tts_pending().await;
        Ok(())
    }

    pub fn rules(&self) -> Vec<AlertRule> {
        self.state().rules.clone()
    }

    pub async fn save_rule(&self, rule: AlertRule) -> Result<AlertRule, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        rule.validate().map_err(|e| EngineError::Invalid(e.to_string()))?;
        let saved = rule.clone();
        self.db(move |s| repo::rules::save(s, &saved)).await?;
        let rules = self.db(repo::rules::list).await?;
        self.state_mut().rules = rules;
        Ok(rule)
    }

    pub async fn delete_rule(&self, id: &str) -> Result<(), EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let rid = id.to_string();
        self.db(move |s| repo::rules::delete(s, &rid)).await?;
        let rules = self.db(repo::rules::list).await?;
        self.state_mut().rules = rules;
        Ok(())
    }

    pub fn goals(&self) -> Vec<Goal> {
        self.state().goals.clone()
    }

    pub async fn save_goal(&self, goal: Goal) -> Result<Goal, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let saved = goal.clone();
        self.db(move |s| repo::goals::save(s, &saved)).await?;
        let goals = self.db(repo::goals::list).await?;
        self.state_mut().goals = goals;
        self.broadcast_goals(self.current_seq());
        Ok(goal)
    }

    pub async fn delete_goal(&self, id: &str) -> Result<(), EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let gid = id.to_string();
        self.db(move |s| repo::goals::delete(s, &gid)).await?;
        let goals = self.db(repo::goals::list).await?;
        self.state_mut().goals = goals;
        self.broadcast_goals(self.current_seq());
        Ok(())
    }

    pub async fn adjust_goal(&self, id: &str, delta: i64) -> Result<Goal, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let gid = id.to_string();
        let g = self.db(move |s| repo::goals::adjust(s, &gid, delta)).await?;
        {
            let mut st = self.state_mut();
            if let Some(slot) = st.goals.iter_mut().find(|x| x.id == g.id) {
                *slot = g.clone();
            }
        }
        self.broadcast_goals(self.current_seq());
        Ok(g)
    }

    pub fn commands(&self) -> Vec<Command> {
        self.state().commands.clone()
    }

    pub async fn save_command(&self, cmd: Command) -> Result<Command, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let saved = cmd.clone();
        self.db(move |s| repo::bot::save_command(s, &saved)).await?;
        let all = self.db(repo::bot::commands).await?;
        self.state_mut().commands = all;
        Ok(cmd)
    }

    pub async fn delete_command(&self, id: &str) -> Result<(), EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let cid = id.to_string();
        self.db(move |s| repo::bot::delete_command(s, &cid)).await?;
        let all = self.db(repo::bot::commands).await?;
        self.state_mut().commands = all;
        Ok(())
    }

    pub fn timers(&self) -> Vec<Timer> {
        self.state().timers.clone()
    }

    pub async fn save_timer(&self, t: Timer) -> Result<Timer, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let saved = t.clone();
        self.db(move |s| repo::bot::save_timer(s, &saved)).await?;
        let all = self.db(repo::bot::timers).await?;
        self.state_mut().timers = all;
        Ok(t)
    }

    pub async fn delete_timer(&self, id: &str) -> Result<(), EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let tid = id.to_string();
        self.db(move |s| repo::bot::delete_timer(s, &tid)).await?;
        let all = self.db(repo::bot::timers).await?;
        self.state_mut().timers = all;
        Ok(())
    }

    /// Status for `GET /status`.
    pub fn status(&self) -> serde_json::Value {
        let st = self.state();
        json!({
            "live": { "twitch": st.live.get(&Platform::Twitch).copied().unwrap_or(false),
                      "youtube": st.live.get(&Platform::Youtube).copied().unwrap_or(false),
                      "kick": st.live.get(&Platform::Kick).copied().unwrap_or(false) },
            "connectors": st.connectors,
            "panic": st.panic_on,
            "session": st.session,
        })
    }

    /// Recent logged events as the dashboard sees them.
    pub async fn events(&self, before: Option<i64>, limit: u32, kinds: Vec<String>) -> Result<Vec<Event>, EngineError> {
        Ok(self
            .db(move |s| repo::events::recent(s, before, limit, &kinds))
            .await?
            .into_iter()
            .map(|(_, e)| e)
            .collect())
    }
}
