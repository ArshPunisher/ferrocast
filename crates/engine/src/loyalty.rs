//! Loyalty points in the engine: paying chatters while live, bonuses for support, the
//! `!points` / `!addpoints` / `!removepoints` commands, and the dashboard's view.

use std::collections::HashMap;
use std::time::Duration;

use ferrocast_core::command::{Access, safe_reply};
use ferrocast_core::event::{Event, EventKind, Millis, Origin, Platform};
use ferrocast_core::loyalty::{self, LoyaltyConfig, MAX_ADJUST, PointsCommand};
use ferrocast_store::repo;
use ferrocast_store::repo::loyalty::{Change, Floor, Leader};

use crate::{Engine, EngineError, now};

/// How often the watch-time clock is checked. The interval itself is in the settings.
const TICK: Duration = Duration::from_secs(30);
/// One `!points` answer per person per this long (mods are never held back).
const BALANCE_COOLDOWN_S: i64 = 15;

pub(crate) const VIEWER_UNKNOWN: EngineError = EngineError::Missing {
    code: "viewer_not_found",
    message: "Ferrocast hasn't seen this person yet.",
    hint: "Points and viewer cards fill in once someone chats or follows while Ferrocast is running.",
};

/// Per-person cooldowns for `!points`.
pub(crate) type PointsCooldowns = HashMap<(Platform, String), Millis>;

pub(crate) async fn run(engine: Engine) {
    loop {
        tokio::time::sleep(TICK).await;
        engine.pay_watch_time(now()).await;
    }
}

impl Engine {
    pub fn loyalty(&self) -> LoyaltyConfig {
        self.state().loyalty.clone()
    }

    /// Saves the fields that were sent; errors name the field.
    pub async fn patch_loyalty(
        &self,
        patch: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<LoyaltyConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut cfg = crate::patch::merge(&self.loyalty(), patch)?;
        cfg.name = cfg.name.trim().to_string();
        cfg.validate().map_err(EngineError::Invalid)?;
        let saved = cfg.clone();
        self.db(move |s| repo::settings::set(s, "loyalty", &saved)).await?;
        self.state_mut().loyalty = cfg.clone();
        Ok(cfg)
    }

    /// Someone's balance (404 `viewer_not_found` for someone never seen).
    pub async fn points(&self, platform: Platform, user_id: &str) -> Result<i64, EngineError> {
        let uid = user_id.to_string();
        self.db(move |s| repo::loyalty::balance(s, platform, &uid)).await?.ok_or(VIEWER_UNKNOWN)
    }

    /// Adds or takes away points from the dashboard. Never below zero, never past the cap;
    /// a bad amount is a 400 that names `delta`.
    pub async fn adjust_points(&self, platform: Platform, user_id: &str, delta: i64) -> Result<i64, EngineError> {
        if delta == 0 || delta.unsigned_abs() > MAX_ADJUST.unsigned_abs() {
            return Err(EngineError::Invalid(
                "delta: must be a whole number from -1,000,000,000 to 1,000,000,000, and not 0.".into(),
            ));
        }
        let uid = user_id.to_string();
        let changed = self.db(move |s| repo::loyalty::change(s, platform, &uid, delta, Floor::Strict)).await;
        let name = self.loyalty().name;
        match changed {
            Ok(Change::Done(n)) => Ok(n),
            Ok(Change::NotEnough { have }) => Err(EngineError::Invalid(format!(
                "delta: they have {}, so you can take away at most {}.",
                loyalty::label(have, &name),
                loyalty::group(have)
            ))),
            Ok(Change::TooMuch { .. }) => Err(EngineError::Invalid(format!(
                "delta: that would put them over the limit of {}.",
                loyalty::label(loyalty::MAX_BALANCE, &name)
            ))),
            Err(EngineError::Store(ferrocast_store::StoreError::NotFound(_))) => Err(VIEWER_UNKNOWN),
            Err(e) => Err(e),
        }
    }

    pub async fn leaderboard(&self, limit: u32) -> Result<Vec<Leader>, EngineError> {
        self.db(move |s| repo::loyalty::leaderboard(s, limit)).await
    }

    /// Notes a live chat message for watch-time points.
    pub(crate) fn loyalty_saw(&self, e: &Event) {
        let EventKind::ChatMessage { actor, .. } = &e.kind else { return };
        if e.origin != Origin::Live || e.channel.is_some() {
            return;
        }
        let (on, live) = {
            let st = self.state();
            (st.loyalty.enabled, st.live.get(&e.platform).copied().unwrap_or(false))
        };
        if on && live {
            self.inner.activity.lock().unwrap_or_else(|p| p.into_inner()).saw(actor);
        }
    }

    /// Pays everyone who chatted during the interval, if one has passed. Returns how many
    /// people were paid (tests call this directly with their own clock).
    pub(crate) async fn pay_watch_time(&self, at: Millis) -> usize {
        let (cfg, live) = {
            let st = self.state();
            (st.loyalty.clone(), st.live.values().any(|l| *l))
        };
        let due = self.inner.activity.lock().unwrap_or_else(|p| p.into_inner()).due(&cfg, live, at);
        let who: Vec<(Platform, String)> = due.into_iter().filter_map(|a| a.id.map(|id| (a.platform, id))).collect();
        if who.is_empty() {
            return 0;
        }
        let amount = cfg.per_interval;
        match self.db(move |s| repo::loyalty::award(s, &who, amount)).await {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(%err, "couldn't pay watch-time points");
                0
            }
        }
    }

    /// The bonus for a live support event, paid once however often it's delivered.
    pub(crate) async fn loyalty_bonus(&self, e: &Event) {
        let cfg = self.loyalty();
        let Some((actor, amount)) = loyalty::bonus(&cfg, e) else { return };
        let Some(uid) = actor.id.clone() else { return };
        let a = actor.clone();
        let source = e.source_id.clone().unwrap_or_else(|| e.id.clone());
        let p = e.platform;
        let r = self
            .db(move |s| {
                repo::viewers::touch(s, &a, false)?;
                repo::loyalty::award_once(s, (p, &source), (a.platform, &uid), amount)
            })
            .await;
        if let Err(err) = r {
            tracing::warn!(%err, "couldn't pay a loyalty bonus");
        }
    }

    /// Answers `!points`, `!addpoints` and `!removepoints`. Returns whether the message was
    /// a points command (so nothing else answers it). A custom command with the same name
    /// wins: it's the streamer's own choice.
    pub(crate) async fn answer_points_command(&self, e: &Event) -> bool {
        let EventKind::ChatMessage { actor, content, .. } = &e.kind else { return false };
        let (cfg, custom) = {
            let st = self.state();
            let word = ferrocast_core::command::parse(&content.text).map(|i| i.word.to_string()).unwrap_or_default();
            (st.loyalty.clone(), st.commands.iter().any(|c| c.enabled && c.answers_to(&word)))
        };
        if !cfg.enabled || custom {
            return false;
        }
        let Some(cmd) = loyalty::parse_command(&cfg, &content.text) else { return false };
        let Some(sender_id) = actor.id.clone() else { return true };
        let is_mod = Access::Mods.allows(actor.roles);
        let p = e.platform;
        let add = matches!(cmd, PointsCommand::Add { .. });
        let reply = match cmd {
            PointsCommand::Balance => {
                if !is_mod && !self.points_cooldown_ok(p, &sender_id) {
                    return true;
                }
                let uid = sender_id.clone();
                let have = self.db(move |s| repo::loyalty::balance(s, p, &uid)).await.ok().flatten().unwrap_or(0);
                format!("@{}, you have {}.", actor.display_name, loyalty::label(have, &cfg.name))
            }
            _ if !is_mod => return true,
            PointsCommand::Usage(how) => how.to_string(),
            PointsCommand::Add { target, amount } | PointsCommand::Remove { target, amount } => {
                let t = target.clone();
                let found = self.db(move |s| repo::loyalty::find_login(s, p, &t)).await.ok().flatten();
                match found {
                    None => format!("I haven't seen @{target} in chat yet."),
                    Some((uid, name)) => {
                        let delta = if add { amount } else { -amount };
                        let r = self.db(move |s| repo::loyalty::change(s, p, &uid, delta, Floor::Clamp)).await;
                        let given = loyalty::label(amount, &cfg.name);
                        match r {
                            Ok(Change::Done(n)) if add => {
                                format!("Gave {given} to {name}. They have {} now.", loyalty::label(n, &cfg.name))
                            }
                            Ok(Change::Done(n)) => {
                                format!("Took {given} from {name}. They have {} now.", loyalty::label(n, &cfg.name))
                            }
                            Ok(Change::TooMuch { .. }) => format!("{name} can't hold any more {}.", cfg.name),
                            Ok(Change::NotEnough { .. }) | Err(_) => "Couldn't change their points just now.".into(),
                        }
                    }
                }
            }
        };
        let reply = safe_reply(&reply);
        let reply_to = e.source_id.clone();
        self.spawn_action(p, "answer a points command", move |c| {
            Box::pin(async move { c.send_chat(&reply, reply_to.as_deref()).await })
        });
        true
    }

    fn points_cooldown_ok(&self, p: Platform, user_id: &str) -> bool {
        let t = now();
        let mut cd = self.inner.points_cooldown.lock().unwrap_or_else(|p| p.into_inner());
        let key = (p, user_id.to_string());
        if cd.get(&key).is_some_and(|last| t.seconds_since(*last) < BALANCE_COOLDOWN_S) {
            return false;
        }
        if cd.len() > 10_000 {
            cd.retain(|_, last| t.seconds_since(*last) < BALANCE_COOLDOWN_S);
        }
        cd.insert(key, t);
        true
    }
}
