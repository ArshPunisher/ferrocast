//! Auto-replies (API.md §4 "Auto-replies", PRO.md M5): the bot answers chat that mentions
//! something, like "discord" → the invite. Free and local.
//!
//! The pipeline calls [`Engine::answer_autoreply`] for live chat from this channel, after
//! moderation passed it and only when no command handled it. It stays quiet when the bot is
//! off or panic mode is on, for messages from the streamer's own account (the bot speaks as
//! it, R-28), from other bots, and for anything that echoes a reply it just sent (loop
//! guard). Each platform has a send budget (R-19) and the platform's own "slow down" pauses
//! auto-replies there. Matching, cooldowns and the budget are pure (`core::autoreply`).

use std::collections::VecDeque;
use std::sync::Arc;

use ferrocast_connector::ConnectorError;
use ferrocast_core::autoreply::{self, AutoReply, AutoReplyError, MAX_RULES, Message, RuleSet, Skip, fold};
use ferrocast_core::event::{Actor, Event, EventKind, Millis, Platform, Roles};
use ferrocast_core::loyalty::is_known_bot;
use ferrocast_core::template::Vars;
use ferrocast_store::repo;
use serde::Serialize;

use crate::{Engine, EngineError, now, platform_key};

/// How long a sent reply is remembered for the echo guard.
const ECHO_MS: i64 = 120_000;
/// How many sent replies are remembered.
const ECHO_KEEP: usize = 64;

const NOT_FOUND: EngineError = EngineError::Missing {
    code: "autoreply_not_found",
    message: "That auto-reply doesn't exist.",
    hint: "Reload the Bot page; it may have been deleted.",
};

/// Auto-replies' runtime state (one per engine, inside Studio).
#[derive(Debug, Default)]
pub(crate) struct AutoReplies {
    /// The saved rules in order, as the dashboard sees them.
    pub list: std::sync::RwLock<Vec<AutoReply>>,
    /// The same rules compiled once (patterns, templates).
    pub set: std::sync::RwLock<Arc<RuleSet>>,
    pub cooldowns: std::sync::Mutex<autoreply::Cooldowns>,
    pub budget: std::sync::Mutex<autoreply::SendBudget>,
    /// What the bot said lately, folded, per platform: chat that repeats it is the bot's
    /// own message coming back (or another bot copying it), never answered.
    pub sent: std::sync::Mutex<VecDeque<(Platform, String, Millis)>>,
}

/// The dashboard's "Try it" answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TryResult {
    pub matched: bool,
    pub rule_id: Option<String>,
    pub rule_name: Option<String>,
    /// The reply as the bot would send it now (variables filled in).
    pub reply: Option<String>,
}

/// A rule that can't be saved: 413 for a reply over the limit, 422 for a pattern that
/// can't be used, 400 for the rest. The message names the field.
fn rule_error(e: AutoReplyError) -> EngineError {
    match e.status() {
        400 => EngineError::Invalid(e.to_string()),
        status => EngineError::Refused {
            status,
            code: e.code(),
            message: e.to_string(),
            hint: e.hint().into(),
            retry_after: None,
        },
    }
}

impl Engine {
    fn auto(&self) -> &AutoReplies {
        &self.studio().autoreplies
    }

    /// Loads the saved rules (at start). A rule that no longer compiles is kept in the list
    /// (so the dashboard can fix it) but never matches, and is logged.
    pub(crate) async fn load_autoreplies(&self) {
        match self.db(repo::autoreplies::list).await {
            Ok(list) => self.set_autoreplies(list),
            Err(err) => tracing::error!(%err, "couldn't load auto-replies"),
        }
    }

    fn set_autoreplies(&self, list: Vec<AutoReply>) {
        let (set, bad) = RuleSet::compile(&list);
        for (id, err) in bad {
            tracing::warn!(rule = %id, %err, "an auto-reply can't be used until it's fixed");
        }
        *self.auto().set.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(set);
        *self.auto().list.write().unwrap_or_else(|p| p.into_inner()) = list;
    }

    async fn reload_autoreplies(&self) -> Result<Vec<AutoReply>, EngineError> {
        let list = self.db(repo::autoreplies::list).await?;
        self.set_autoreplies(list.clone());
        Ok(list)
    }

    pub fn autoreplies(&self) -> Vec<AutoReply> {
        self.auto().list.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Adds a rule at the end. 409 for a taken id, 422 `too_many_autoreplies` at 100.
    pub async fn create_autoreply(&self, mut rule: AutoReply) -> Result<AutoReply, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        if rule.id.trim().is_empty() {
            rule.id = ferrocast_store::new_id();
        }
        rule.name = rule.name.trim().to_string();
        let list = self.autoreplies();
        if list.iter().any(|r| r.id == rule.id) {
            return Err(EngineError::Conflict("An auto-reply with that id already exists.".into()));
        }
        if list.len() >= MAX_RULES {
            return Err(EngineError::Refused {
                status: 422,
                code: "too_many_autoreplies",
                message: format!("You have {MAX_RULES} auto-replies, the most Ferrocast keeps."),
                hint: "Delete or merge some first: one rule can look for several words with a pattern.".into(),
                retry_after: None,
            });
        }
        rule.validate().map_err(rule_error)?;
        let saved = rule.clone();
        self.db(move |s| repo::autoreplies::save(s, &saved)).await?;
        self.reload_autoreplies().await?;
        Ok(rule)
    }

    /// Replaces a rule, keeping its place. Its cooldowns start fresh.
    pub async fn update_autoreply(&self, id: &str, mut rule: AutoReply) -> Result<AutoReply, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        if !self.autoreplies().iter().any(|r| r.id == id) {
            return Err(NOT_FOUND);
        }
        rule.id = id.to_string();
        rule.name = rule.name.trim().to_string();
        rule.validate().map_err(rule_error)?;
        let saved = rule.clone();
        self.db(move |s| repo::autoreplies::save(s, &saved)).await?;
        self.reload_autoreplies().await?;
        self.auto().cooldowns.lock().unwrap_or_else(|p| p.into_inner()).forget(id);
        Ok(rule)
    }

    pub async fn delete_autoreply(&self, id: &str) -> Result<(), EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let rid = id.to_string();
        match self.db(move |s| repo::autoreplies::delete(s, &rid)).await {
            Err(EngineError::Store(ferrocast_store::StoreError::NotFound(_))) => return Err(NOT_FOUND),
            other => other?,
        }
        self.reload_autoreplies().await?;
        self.auto().cooldowns.lock().unwrap_or_else(|p| p.into_inner()).forget(id);
        Ok(())
    }

    /// Puts the rules in this order (every id once). 409 when the list changed meanwhile.
    pub async fn reorder_autoreplies(&self, ids: Vec<String>) -> Result<Vec<AutoReply>, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        self.db(move |s| repo::autoreplies::reorder(s, &ids)).await?;
        self.reload_autoreplies().await
    }

    /// "Try it": whether `text` would get an answer and what it would be. With `rule`, that
    /// (unsaved) rule alone is tried, so the add/edit dialog can test before saving; a bad
    /// pattern answers the same 422 a save would. Platform, role, live and cooldowns are
    /// ignored: this is about the words.
    pub async fn try_autoreply(
        &self,
        text: &str,
        rule: Option<AutoReply>,
        platform: Option<Platform>,
    ) -> Result<TryResult, EngineError> {
        let mut has_reply = true;
        let set = match rule {
            Some(mut r) => {
                if r.id.is_empty() {
                    r.id = "draft".into();
                }
                // A draft may still be half written: no name or reply yet doesn't stop a try.
                if r.name.trim().is_empty() {
                    r.name = "Draft".into();
                }
                if r.reply.trim().is_empty() {
                    has_reply = false;
                    r.reply = "-".into();
                }
                r.enabled = true;
                autoreply::compile_rule(&r).map_err(rule_error)?;
                Arc::new(RuleSet::compile(&[r]).0)
            }
            None => self.auto().set.read().unwrap_or_else(|p| p.into_inner()).clone(),
        };
        let Some(hit) = set.first_text_match(text) else {
            return Ok(TryResult { matched: false, rule_id: None, rule_name: None, reply: None });
        };
        let p = platform.unwrap_or(Platform::Twitch);
        let viewer = Actor::named(p, "viewer");
        let reply = if has_reply {
            let extra = self.autoreply_vars(p, &hit.rule.reply).await;
            Some(hit.render(p, &viewer, &extra))
        } else {
            None
        };
        Ok(TryResult {
            matched: true,
            rule_id: Some(hit.rule.id.clone()),
            rule_name: Some(hit.rule.name.clone()),
            reply,
        })
    }

    /// The variables a reply can use beyond `{user}`, `{name}` and `{platform}`: `{channel}`
    /// (this platform's account), `{game}` and `{title}` (last set from Ferrocast),
    /// `{uptime}`, `{uptime_text}` and `{count:<name>}`.
    async fn autoreply_vars(&self, platform: Platform, reply: &str) -> Vars {
        let mut extra = Vars::new();
        let (channel, started) = {
            let st = self.state();
            let channel = st
                .connectors
                .get(platform_key(platform))
                .and_then(|c| c.account.as_ref())
                .map(|a| a.name.clone())
                .unwrap_or_default();
            let live = st.live.values().any(|l| *l);
            (channel, st.session.started_at.filter(|_| live))
        };
        extra.insert("channel".into(), channel);
        match started {
            Some(at) => {
                let mins = now().seconds_since(at) / 60;
                let up = format!("{}h {}m", mins / 60, mins % 60);
                extra.insert("uptime_text".into(), format!("We've been live for {up}."));
                extra.insert("uptime".into(), up);
            }
            None => {
                extra.insert("uptime_text".into(), "We're not live right now.".into());
            }
        }
        // Only read the stream info when the reply uses it.
        if reply.contains("{game") || reply.contains("{title") {
            let history = self.stream_info_saved().await.map(|s| s.history).unwrap_or_default();
            let game = history.iter().find_map(|h| h.info.category.as_ref().map(|c| c.name.clone()));
            let title = history.iter().find_map(|h| h.info.title.clone());
            extra.insert("game".into(), game.unwrap_or_else(|| "something fun".into()));
            extra.insert("title".into(), title.unwrap_or_default());
        }
        self.counter_vars(&mut extra);
        extra
    }

    /// Whether this message comes from the bot itself or another bot (loop guard): the
    /// streamer's own account (the bot speaks as it, R-28), the connected account's name,
    /// a bot badge or a well-known bot, or text the bot just said on that platform.
    fn is_bot_message(&self, platform: Platform, actor: &Actor, text: &str, at: Millis) -> bool {
        if actor.roles.contains(Roles::BROADCASTER) || actor.roles.contains(Roles::BOT) {
            return true;
        }
        let login = actor.login.as_deref().unwrap_or(&actor.display_name);
        if is_known_bot(login) || is_known_bot(&actor.display_name) {
            return true;
        }
        let own = self
            .state()
            .connectors
            .get(platform_key(platform))
            .and_then(|c| c.account.as_ref())
            .map(|a| a.name.trim_start_matches('@').to_lowercase());
        if own.is_some_and(|o| !o.is_empty() && (o == login.to_lowercase() || o == actor.display_name.to_lowercase())) {
            return true;
        }
        let folded = fold(text, false);
        let sent = self.auto().sent.lock().unwrap_or_else(|p| p.into_inner());
        sent.iter().any(|(p, t, when)| *p == platform && at.0 - when.0 < ECHO_MS && *t == folded)
    }

    /// The pipeline's hook (see the module docs). Returns whether a reply was sent.
    pub(crate) async fn answer_autoreply(&self, e: &Event) -> bool {
        let EventKind::ChatMessage { actor, content, .. } = &e.kind else { return false };
        let set = self.auto().set.read().unwrap_or_else(|p| p.into_inner()).clone();
        if set.is_empty() {
            return false;
        }
        let (bot_on, panic, live) = {
            let st = self.state();
            (st.settings.bot_enabled, st.panic_on, st.live.get(&e.platform).copied().unwrap_or(false))
        };
        let at = now();
        if !bot_on || panic || self.is_bot_message(e.platform, actor, &content.text, at) {
            return false;
        }
        if self.auto().budget.lock().unwrap_or_else(|p| p.into_inner()).left(e.platform, at) == 0 {
            tracing::debug!(platform = %e.platform, "auto-reply budget used up");
            return false;
        }
        // Variables first (may read the store), then the match and cooldowns in one step.
        let Some(found) = set.first_match(e.platform, actor, &content.text, live) else { return false };
        let extra = self.autoreply_vars(e.platform, &found.rule.reply).await;
        let msg = Message { platform: e.platform, actor, text: &content.text, live };
        let hit = {
            let mut cd = self.auto().cooldowns.lock().unwrap_or_else(|p| p.into_inner());
            autoreply::run(&set, &mut cd, msg, &extra, at)
        };
        let hit = match hit {
            Ok(h) => h,
            Err(Skip::NoMatch) => return false,
            Err(skip) => {
                tracing::debug!(?skip, "auto-reply cooling down");
                return false;
            }
        };
        if !self.auto().budget.lock().unwrap_or_else(|p| p.into_inner()).take(e.platform, at) {
            return false;
        }
        {
            let mut sent = self.auto().sent.lock().unwrap_or_else(|p| p.into_inner());
            sent.push_back((e.platform, fold(&hit.reply, false), at));
            while sent.len() > ECHO_KEEP {
                sent.pop_front();
            }
        }
        let engine = self.clone();
        let platform = e.platform;
        let reply_to = e.source_id.clone();
        let text = hit.reply;
        self.spawn_action(platform, "send an auto-reply", move |c| {
            Box::pin(async move {
                let r = c.send_chat(&text, reply_to.as_deref()).await;
                if let Err(ConnectorError::RateLimited { retry_after, .. }) = &r {
                    engine.auto().budget.lock().unwrap_or_else(|p| p.into_inner()).pause(
                        platform,
                        retry_after.as_secs().max(1),
                        now(),
                    );
                }
                r
            })
        });
        true
    }
}

#[cfg(test)]
mod tests;
