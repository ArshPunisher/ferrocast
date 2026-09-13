//! Counters, quotes and the built-in chat modules (API.md §4 "Studio: chatbot").
//!
//! A custom command with the same name always wins (it's the streamer's own choice), then
//! counters, then modules. Only live chat from this channel gets answers (R-11). Viewers
//! have cooldowns; mods never do.

use std::collections::HashMap;

use ferrocast_core::chatbot::{
    self, BetError, Challenge, CounterDef, CounterOp, Module, ModulesConfig, QuoteCmd, counter_usage,
};
use ferrocast_core::command::{Access, parse};
use ferrocast_core::event::{Actor, Event, EventKind, Platform};
use ferrocast_core::overlay::WidgetKind;
use ferrocast_core::template::{Template, Vars};
use ferrocast_store::repo;
use ferrocast_store::repo::bot::Quote;
use ferrocast_store::repo::loyalty::{Change, Floor};
use serde::Serialize;
use serde_json::{Value, json};

use super::{COUNTERS_KEY, MODULES_KEY, save_setting};
use crate::frames::{Audience, OverlayFrame};
use crate::{Engine, EngineError, now, platform_key};

/// Viewers wait this long between uses of one module or counter command.
const VIEWER_COOLDOWN_S: i64 = 10;
/// One answer per command per this long, whoever asks (a counter shown by ten people).
const GLOBAL_COOLDOWN_S: i64 = 3;

/// A counter as the dashboard sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CounterView {
    pub name: String,
    pub value: i64,
    pub command: Option<String>,
    pub reply: String,
    pub enabled: bool,
}

const COUNTER_NOT_FOUND: EngineError = EngineError::Missing {
    code: "counter_not_found",
    message: "That counter doesn't exist.",
    hint: "Reload the Bot page; it may have been deleted.",
};

const QUOTE_NOT_FOUND: EngineError = EngineError::Missing {
    code: "quote_not_found",
    message: "That quote doesn't exist.",
    hint: "Reload the quotes list; a mod may have removed it.",
};

impl Engine {
    // -------------------------------------------------------------- counters

    pub fn counters(&self) -> Vec<CounterView> {
        let values = self.studio().counters.lock().unwrap_or_else(|p| p.into_inner()).clone();
        self.studio()
            .counter_defs
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|d| CounterView {
                name: d.name.clone(),
                value: values.get(&d.name).copied().unwrap_or(0),
                command: d.command.clone(),
                reply: d.reply.clone(),
                enabled: d.enabled,
            })
            .collect()
    }

    pub fn counter_view(&self, name: &str) -> Result<CounterView, EngineError> {
        self.counters().into_iter().find(|c| c.name == name).ok_or(COUNTER_NOT_FOUND)
    }

    /// Every counter value, for `{count:name}` in templates.
    pub(crate) fn counter_vars(&self, vars: &mut Vars) {
        let values = self.studio().counters.lock().unwrap_or_else(|p| p.into_inner()).clone();
        chatbot::counter_vars(vars, &values);
    }

    /// Creates a counter (and its `!name` command unless `command` is null). 409 when the
    /// name or command is taken by a counter or a custom command.
    pub async fn create_counter(&self, def: CounterDef, value: i64) -> Result<CounterView, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        def.validate().map_err(EngineError::Invalid)?;
        let mut defs = self.studio().counter_defs.read().unwrap_or_else(|p| p.into_inner()).clone();
        if defs.len() >= chatbot::MAX_COUNTERS {
            return Err(EngineError::Conflict(format!("You can have up to {} counters.", chatbot::MAX_COUNTERS)));
        }
        if defs.iter().any(|d| d.name == def.name) {
            return Err(EngineError::Conflict(format!("A counter called \"{}\" already exists.", def.name)));
        }
        self.command_free(&def, &defs)?;
        if value.abs() > chatbot::MAX_COUNTER_VALUE {
            return Err(EngineError::Invalid("value: between -1,000,000,000 and 1,000,000,000.".into()));
        }
        defs.push(def.clone());
        save_setting(self, COUNTERS_KEY, &defs).await?;
        let name = def.name.clone();
        let saved = self.db(move |s| repo::studio::set_counter(s, &name, value)).await?;
        *self.studio().counter_defs.write().unwrap_or_else(|p| p.into_inner()) = defs;
        self.counter_changed(&def.name, saved).await;
        self.counter_view(&def.name)
    }

    /// Changes a counter's command, reply, on/off or value (only what was sent).
    pub async fn update_counter(
        &self,
        name: &str,
        patch: &serde_json::Map<String, Value>,
    ) -> Result<CounterView, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut defs = self.studio().counter_defs.read().unwrap_or_else(|p| p.into_inner()).clone();
        let i = defs.iter().position(|d| d.name == name).ok_or(COUNTER_NOT_FOUND)?;
        let mut def = crate::patch::merge(&defs[i], patch)?;
        def.name = name.to_string(); // the name is the counter's identity
        def.validate().map_err(EngineError::Invalid)?;
        let others: Vec<CounterDef> = defs.iter().filter(|d| d.name != name).cloned().collect();
        self.command_free(&def, &others)?;
        let value = match patch.get("value") {
            None => None,
            Some(v) => Some(
                v.as_i64()
                    .filter(|n| n.abs() <= chatbot::MAX_COUNTER_VALUE)
                    .ok_or_else(|| EngineError::Invalid("value: a whole number up to ±1,000,000,000.".into()))?,
            ),
        };
        defs[i] = def;
        save_setting(self, COUNTERS_KEY, &defs).await?;
        *self.studio().counter_defs.write().unwrap_or_else(|p| p.into_inner()) = defs;
        if let Some(v) = value {
            let n = name.to_string();
            let saved = self.db(move |s| repo::studio::set_counter(s, &n, v)).await?;
            self.counter_changed(name, saved).await;
        }
        self.counter_view(name)
    }

    pub async fn delete_counter(&self, name: &str) -> Result<(), EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut defs = self.studio().counter_defs.read().unwrap_or_else(|p| p.into_inner()).clone();
        let before = defs.len();
        defs.retain(|d| d.name != name);
        if defs.len() == before {
            return Err(COUNTER_NOT_FOUND);
        }
        save_setting(self, COUNTERS_KEY, &defs).await?;
        let n = name.to_string();
        self.db(move |s| repo::studio::delete_counter(s, &n)).await?;
        *self.studio().counter_defs.write().unwrap_or_else(|p| p.into_inner()) = defs;
        self.studio().counters.lock().unwrap_or_else(|p| p.into_inner()).remove(name);
        Ok(())
    }

    /// `+`, `-`, `set`, `reset` from the dashboard or chat. Returns the new value.
    pub async fn change_counter(&self, name: &str, op: CounterOp) -> Result<CounterView, EngineError> {
        if !self.studio().counter_defs.read().unwrap_or_else(|p| p.into_inner()).iter().any(|d| d.name == name) {
            return Err(COUNTER_NOT_FOUND);
        }
        if op.changes() {
            let n = name.to_string();
            let v = self.db(move |s| repo::studio::update_counter(s, &n, |v| chatbot::apply_counter(v, op))).await?;
            self.counter_changed(name, v).await;
        }
        self.counter_view(name)
    }

    /// A counter's command must not clash with another counter or a custom command.
    fn command_free(&self, def: &CounterDef, others: &[CounterDef]) -> Result<(), EngineError> {
        let Some(cmd) = &def.command else { return Ok(()) };
        if others.iter().any(|o| o.command.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(cmd))) {
            return Err(EngineError::Conflict(format!("Another counter already uses {cmd}.")));
        }
        if self.state().commands.iter().any(|c| c.answers_to(cmd)) {
            return Err(EngineError::Conflict(format!("A custom command already uses {cmd}. Rename one of them.")));
        }
        if chatbot::module_for(cmd).is_some()
            || matches!(cmd.to_lowercase().as_str(), "!scene" | "!brb" | "!back" | "!clip")
        {
            return Err(EngineError::Conflict(format!("{cmd} is a built-in command. Pick another name.")));
        }
        Ok(())
    }

    /// Remembers the value, tells dashboards and updates overlay labels that show it.
    async fn counter_changed(&self, name: &str, value: i64) {
        self.studio().counters.lock().unwrap_or_else(|p| p.into_inner()).insert(name.to_string(), value);
        self.studio_frame("counter", &json!({ "name": name, "value": value }));
        let Ok(overlays) = self.db(repo::overlays::list).await else { return };
        let mut vars = Vars::new();
        self.counter_vars(&mut vars);
        for o in overlays {
            let labels: serde_json::Map<String, Value> = o
                .widgets
                .iter()
                .filter(|w| w.kind == WidgetKind::Label)
                .filter_map(|w| {
                    let tpl = w.config.get("template").and_then(Value::as_str)?;
                    if !chatbot::counters_in(tpl).iter().any(|c| c == name) {
                        return None;
                    }
                    let source = w.config.get("source").and_then(Value::as_str).unwrap_or("");
                    let mut v = vars.clone();
                    v.insert("value".into(), self.state().session.label(source));
                    let text = Template::parse(tpl).map(|t| t.render(&v)).unwrap_or_default();
                    Some((w.id.clone(), json!(text)))
                })
                .collect();
            if !labels.is_empty() {
                self.send_overlay(OverlayFrame::new(
                    Audience::Overlay(o.id.clone()),
                    self.next_seq(),
                    "state",
                    &json!({ "labels": labels }),
                ));
            }
        }
    }

    // -------------------------------------------------------------- quotes

    pub async fn quotes(&self) -> Result<Vec<Quote>, EngineError> {
        self.db(repo::studio::quotes).await
    }

    pub async fn add_quote(&self, text: &str, added_by: &str) -> Result<Quote, EngineError> {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        let text = text.trim().to_string();
        if text.is_empty() || text.chars().count() > chatbot::MAX_QUOTE_CHARS {
            return Err(EngineError::Invalid(format!("text: 1 to {} characters.", chatbot::MAX_QUOTE_CHARS)));
        }
        if self.db(repo::studio::quote_count).await? >= chatbot::MAX_QUOTES {
            return Err(EngineError::Conflict(format!(
                "You have {} quotes, the most Ferrocast keeps. Remove some first.",
                chatbot::MAX_QUOTES
            )));
        }
        let by: String = added_by.chars().take(60).collect();
        self.db(move |s| repo::bot::add_quote(s, &text, &by)).await
    }

    pub async fn edit_quote(&self, id: i64, text: &str) -> Result<Quote, EngineError> {
        if text.trim().is_empty() || text.trim().chars().count() > chatbot::MAX_QUOTE_CHARS {
            return Err(EngineError::Invalid(format!("text: 1 to {} characters.", chatbot::MAX_QUOTE_CHARS)));
        }
        let t = text.to_string();
        match self.db(move |s| repo::studio::edit_quote(s, id, &t)).await {
            Err(EngineError::Store(ferrocast_store::StoreError::NotFound(_))) => Err(QUOTE_NOT_FOUND),
            other => other,
        }
    }

    pub async fn delete_quote(&self, id: i64) -> Result<(), EngineError> {
        match self.db(move |s| repo::bot::delete_quote(s, id)).await {
            Err(EngineError::Store(ferrocast_store::StoreError::NotFound(_))) => Err(QUOTE_NOT_FOUND),
            other => other,
        }
    }

    // -------------------------------------------------------------- modules

    pub fn modules(&self) -> ModulesConfig {
        self.studio().modules.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Saves only what was sent, one module at a time (`{ "dice": { "enabled": false } }`).
    pub async fn patch_modules(&self, patch: &serde_json::Map<String, Value>) -> Result<ModulesConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let current = serde_json::to_value(self.modules()).map_err(|e| EngineError::Internal(e.to_string()))?;
        let mut merged = current.clone();
        for (k, v) in patch {
            let Some(slot) = merged.get_mut(k) else { continue };
            let (Some(slot_obj), Some(v_obj)) = (slot.as_object_mut(), v.as_object()) else {
                return Err(EngineError::Invalid(format!("{k}: must be an object of settings.")));
            };
            let base = serde_json::Value::Object(slot_obj.clone());
            for (fk, fv) in v_obj {
                if let Some(cur) = base.get(fk) {
                    let same_type = matches!(
                        (cur, fv),
                        (Value::Bool(_), Value::Bool(_))
                            | (Value::Number(_), Value::Number(_))
                            | (Value::String(_), Value::String(_))
                    );
                    if !same_type {
                        let want = match cur {
                            Value::Bool(_) => "true or false",
                            Value::Number(_) => "a number",
                            _ => "text",
                        };
                        return Err(EngineError::Invalid(format!("{k}.{fk}: must be {want}.")));
                    }
                    slot_obj.insert(fk.clone(), fv.clone());
                }
            }
        }
        let cfg: ModulesConfig = serde_json::from_value(merged)
            .map_err(|e| EngineError::Invalid(format!("A setting has the wrong type: {e}")))?;
        cfg.validate().map_err(EngineError::Invalid)?;
        save_setting(self, MODULES_KEY, &cfg).await?;
        *self.studio().modules.write().unwrap_or_else(|p| p.into_inner()) = cfg.clone();
        Ok(cfg)
    }

    // -------------------------------------------------------------- chat

    /// Counters, modules and the OBS chat commands. Returns whether the message was one of
    /// them (so nothing else answers it).
    pub(crate) async fn answer_studio_command(&self, e: &Event) -> bool {
        let EventKind::ChatMessage { actor, content, .. } = &e.kind else { return false };
        let Some(inv) = parse(&content.text) else { return false };
        let word = inv.word.to_lowercase();
        if self.state().commands.iter().any(|c| c.enabled && c.answers_to(&word)) {
            return false;
        }
        let rest = content.text.trim_start().get(inv.word.len()..).unwrap_or("").trim().to_string();
        let is_mod = Access::Mods.allows(actor.roles);
        let user_key =
            format!("{}:{}", platform_key(e.platform), actor.id.clone().unwrap_or_else(|| actor.display_name.clone()));

        // Counters
        let def = self
            .studio()
            .counter_defs
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|d| d.enabled && d.command.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(&word)))
            .cloned();
        if let Some(def) = def {
            self.counter_command(e, actor, &def, &inv.args, is_mod, &user_key).await;
            return true;
        }

        if self.obs_chat_command(e, actor, &word, &rest, is_mod).await {
            return true;
        }

        let Some(module) = chatbot::module_for(&word) else { return false };
        let cfg = self.modules();
        if !cfg.enabled(module) {
            return false;
        }
        if !is_mod {
            let per_user = format!("{word}:{user_key}");
            let global = format!("{word}:*");
            // Mods-only asks from viewers are ignored without using up their cooldown.
            let mods_only = matches!(module, Module::Shoutout)
                || (module == Module::Quote && chatbot::parse_quote(&rest).mods_only());
            if mods_only {
                return true;
            }
            if !self.studio_cooldown(&global, GLOBAL_COOLDOWN_S) || !self.studio_cooldown(&per_user, VIEWER_COOLDOWN_S)
            {
                // Accept/deny answer a waiting duel: never held back by the cooldown.
                if !matches!(module, Module::Accept | Module::Deny) {
                    return true;
                }
            }
        }
        // Changing the title or game talks to every platform (seconds): never inline.
        let changes_info = matches!(module, Module::Game | Module::Title) && is_mod && !rest.is_empty();
        if changes_info {
            let (engine, ev, actor, cfg, rest) = (self.clone(), e.clone(), actor.clone(), cfg.clone(), rest.clone());
            tokio::spawn(async move {
                if let Some(r) = engine.game_title(&actor, module, &cfg, &rest, true).await {
                    engine.studio_reply(&ev, &r);
                }
            });
            return true;
        }
        let reply = self.module_reply(e, actor, module, &cfg, &inv.args, &rest, is_mod).await;
        if let Some(r) = reply {
            self.studio_reply(e, &r);
        }
        true
    }

    async fn counter_command(
        &self,
        e: &Event,
        actor: &Actor,
        def: &CounterDef,
        args: &[&str],
        is_mod: bool,
        user_key: &str,
    ) {
        let command = def.command.clone().unwrap_or_default();
        let op = match chatbot::parse_counter_args(args) {
            Ok(op) => op,
            Err(chatbot::BadCounterArgs) if is_mod => {
                self.studio_reply(e, &counter_usage(&command));
                return;
            }
            Err(chatbot::BadCounterArgs) => CounterOp::Show,
        };
        // Only mods change a counter; viewers see it (with a cooldown).
        let op = if is_mod { op } else { CounterOp::Show };
        if !is_mod
            && (!self.studio_cooldown(&format!("{command}:*"), GLOBAL_COOLDOWN_S)
                || !self.studio_cooldown(&format!("{command}:{user_key}"), VIEWER_COOLDOWN_S))
        {
            return;
        }
        match self.change_counter(&def.name, op).await {
            Ok(v) => self.studio_reply(e, &def.render(v.value, &actor.display_name)),
            Err(err) => tracing::warn!(%err, "a counter command failed"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn module_reply(
        &self,
        e: &Event,
        actor: &Actor,
        module: Module,
        cfg: &ModulesConfig,
        args: &[&str],
        rest: &str,
        is_mod: bool,
    ) -> Option<String> {
        let name = &actor.display_name;
        let p = e.platform;
        match module {
            Module::EightBall => Some(if rest.is_empty() {
                format!("@{name}, ask me a question: !8ball will we win?")
            } else {
                format!("@{name}, {}", chatbot::eight_ball(random_u64()))
            }),
            Module::Dice => Some(match chatbot::parse_dice(args.first().copied()) {
                Ok((n, m)) => {
                    let rolls: Vec<u64> = (0..n).map(|_| random_u64()).collect();
                    format!("@{name} {}", chatbot::format_roll(n, m, &rolls))
                }
                Err(how) => format!("@{name}, {how}"),
            }),
            Module::Uptime => Some(match self.stream_started_at() {
                Some(start) => format!("We've been live for {}.", chatbot::long_duration(now().0 - start)),
                None => "We're not live right now.".into(),
            }),
            Module::Watchtime => {
                let uid = actor.id.clone()?;
                let mins = self.db(move |s| repo::studio::watch_time(s, p, &uid)).await.ok()?;
                Some(if mins == 0 {
                    format!("@{name}, your watch time starts counting while you chat during a stream.")
                } else {
                    format!("@{name} has watched for {}.", chatbot::long_duration(mins * 60_000))
                })
            }
            Module::Followage => self.followage(e, actor, args).await,
            Module::Quote => self.quote_command(actor, rest, is_mod).await,
            Module::Roulette => self.roulette(e, actor, cfg, args).await,
            Module::Duel => self.duel(e, actor, cfg, args).await,
            Module::Accept | Module::Deny => self.answer_duel(e, actor, module == Module::Accept).await,
            Module::Game | Module::Title => self.game_title(actor, module, cfg, rest, is_mod).await,
            Module::Shoutout => {
                let login = args.first().map(|a| a.trim_start_matches('@').to_string()).filter(|l| !l.is_empty());
                let Some(login) = login else {
                    return Some(format!("@{name}, who? Type !so @name."));
                };
                let mut vars = Vars::new();
                vars.insert("target".into(), login.clone());
                vars.insert("target_game_note".into(), String::new());
                if let Some(c) = self.connector(p)
                    && let Ok(Ok(Some(game))) =
                        tokio::time::timeout(std::time::Duration::from_secs(3), c.last_game(&login)).await
                {
                    vars.insert("target_game_note".into(), format!("They were last playing {game}."));
                }
                let url = match p {
                    Platform::Twitch => format!("https://twitch.tv/{}", login.to_lowercase()),
                    Platform::Kick => format!("https://kick.com/{}", login.to_lowercase()),
                    _ => String::new(),
                };
                vars.insert("url".into(), url);
                let text = Template::parse(&cfg.shoutout.template).map(|t| t.render(&vars)).unwrap_or_default();
                Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
            }
        }
    }

    async fn followage(&self, e: &Event, actor: &Actor, args: &[&str]) -> Option<String> {
        let p = e.platform;
        let (uid, who) = match args.first().map(|a| a.trim_start_matches('@').to_string()).filter(|a| !a.is_empty()) {
            Some(login) => {
                let l = login.clone();
                match self.db(move |s| repo::loyalty::find_login(s, p, &l)).await.ok().flatten() {
                    Some((uid, n)) => (uid, n),
                    None => return Some(format!("I haven't seen @{login} in chat yet.")),
                }
            }
            None => (actor.id.clone()?, actor.display_name.clone()),
        };
        if actor.roles.contains(ferrocast_core::event::Roles::BROADCASTER) && args.is_empty() {
            return Some(format!("@{who}, it's your channel!"));
        }
        let mut since = None;
        if let Some(c) = self.connector(p)
            && let Ok(Ok(at)) = tokio::time::timeout(std::time::Duration::from_secs(3), c.followed_at(&uid)).await
        {
            since = at;
        }
        if since.is_none() {
            let u = uid.clone();
            since = self.db(move |s| repo::events::first_follow(s, p, &u)).await.ok().flatten();
        }
        Some(match since {
            Some(at) => format!("{who} has followed for {}.", chatbot::long_duration(now().0 - at)),
            None => format!("{who} doesn't follow the channel yet."),
        })
    }

    async fn quote_command(&self, actor: &Actor, rest: &str, is_mod: bool) -> Option<String> {
        let cmd = chatbot::parse_quote(rest);
        if cmd.mods_only() && !is_mod {
            return None;
        }
        Some(match cmd {
            QuoteCmd::Usage => chatbot::QUOTE_USAGE.into(),
            QuoteCmd::Add(text) => match self.add_quote(&text, &actor.display_name).await {
                Ok(q) => format!("Added quote #{}.", q.id),
                Err(err) => err.to_string(),
            },
            QuoteCmd::Remove(id) => match self.delete_quote(id).await {
                Ok(()) => format!("Removed quote #{id}."),
                Err(_) => format!("There's no quote #{id}."),
            },
            QuoteCmd::Number(id) => match self.db(move |s| repo::bot::quote(s, Some(id))).await.ok().flatten() {
                Some(q) => chatbot::format_quote(q.id, &q.text, &q.added_by, q.at),
                None => format!("There's no quote #{id}."),
            },
            QuoteCmd::Random => match self.db(|s| repo::bot::quote(s, None)).await.ok().flatten() {
                Some(q) => chatbot::format_quote(q.id, &q.text, &q.added_by, q.at),
                None => "No quotes yet. Mods can add one with !quote add <text>.".into(),
            },
        })
    }

    async fn roulette(&self, e: &Event, actor: &Actor, cfg: &ModulesConfig, args: &[&str]) -> Option<String> {
        let loyalty = self.loyalty();
        let name = &actor.display_name;
        if !loyalty.enabled {
            return Some("Points are switched off on this channel.".into());
        }
        let uid = actor.id.clone()?;
        let p = e.platform;
        let u = uid.clone();
        let have = self.db(move |s| repo::loyalty::balance(s, p, &u)).await.ok().flatten().unwrap_or(0);
        let r = &cfg.roulette;
        let bet = match chatbot::parse_bet(args.first().copied(), have, r.min_bet, r.max_bet) {
            Ok(b) => b,
            Err(err) => return Some(bet_message(name, err, &loyalty.name)),
        };
        let won = chatbot::roulette_wins(r.win_percent, random_u64());
        let delta = if won { bet } else { -bet };
        let change = self.db(move |s| repo::loyalty::change(s, p, &uid, delta, Floor::Strict)).await.ok()?;
        let label = |n| ferrocast_core::loyalty::label(n, &loyalty.name);
        Some(match change {
            Change::Done(n) if won => format!("@{name} won {} and now has {}!", label(bet), label(n)),
            Change::Done(n) => format!("@{name} lost {} and now has {}.", label(bet), label(n)),
            Change::NotEnough { have } => format!("@{name}, you only have {}.", label(have)),
            Change::TooMuch { .. } => format!("@{name}, you can't hold any more {}.", loyalty.name),
        })
    }

    async fn duel(&self, e: &Event, actor: &Actor, cfg: &ModulesConfig, args: &[&str]) -> Option<String> {
        let loyalty = self.loyalty();
        let name = &actor.display_name;
        if !loyalty.enabled {
            return Some("Points are switched off on this channel.".into());
        }
        let uid = actor.id.clone()?;
        let p = e.platform;
        let target = args.first().map(|a| a.trim_start_matches('@').to_string()).filter(|a| !a.is_empty());
        let Some(target) = target else { return Some(format!("@{name}, duel someone: !duel @name 100")) };
        if actor.login.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(&target)) {
            return Some(format!("@{name}, you can't duel yourself."));
        }
        let t = target.clone();
        let Some((_tid, tname)) = self.db(move |s| repo::loyalty::find_login(s, p, &t)).await.ok().flatten() else {
            return Some(format!("I haven't seen @{target} in chat yet."));
        };
        let u = uid.clone();
        let have = self.db(move |s| repo::loyalty::balance(s, p, &u)).await.ok().flatten().unwrap_or(0);
        let bet = match chatbot::parse_bet(args.get(1).copied().or(Some("")), have, 1, cfg.duel.max_bet) {
            Ok(b) => b,
            Err(BetError::Unreadable) => return Some(format!("@{name}, say how much: !duel @{target} 100")),
            Err(err) => return Some(bet_message(name, err, &loyalty.name)),
        };
        let at = now().0;
        let c = Challenge {
            from: (platform_key(p).to_string(), uid),
            from_name: name.clone(),
            to_login: target.to_lowercase(),
            amount: bet,
            expires_at: at + i64::from(cfg.duel.accept_s) * 1000,
        };
        let r = self.studio().duels.lock().unwrap_or_else(|p| p.into_inner()).challenge(c, at);
        Some(match r {
            Ok(()) => format!(
                "@{tname}, {name} challenges you to a duel for {}! Type !accept or !deny within {} seconds.",
                ferrocast_core::loyalty::label(bet, &loyalty.name),
                cfg.duel.accept_s
            ),
            Err(why) => format!("@{name}, {why}"),
        })
    }

    async fn answer_duel(&self, e: &Event, actor: &Actor, accept: bool) -> Option<String> {
        let login = actor.login.clone().unwrap_or_else(|| actor.display_name.to_lowercase());
        let p = e.platform;
        let c =
            self.studio().duels.lock().unwrap_or_else(|p| p.into_inner()).take_for(platform_key(p), &login, now().0)?;
        let name = &actor.display_name;
        if !accept {
            return Some(format!("{name} turned down the duel with {}.", c.from_name));
        }
        let loyalty = self.loyalty();
        let uid = actor.id.clone()?;
        let (from_id, amount) = (c.from.1.clone(), c.amount);
        let (u, f) = (uid.clone(), from_id.clone());
        let balances = self
            .db(move |s| {
                Ok((repo::loyalty::balance(s, p, &u)?.unwrap_or(0), repo::loyalty::balance(s, p, &f)?.unwrap_or(0)))
            })
            .await
            .ok()?;
        let label = |n| ferrocast_core::loyalty::label(n, &loyalty.name);
        if balances.0 < amount {
            return Some(format!("@{name}, you need {} to take this duel.", label(amount)));
        }
        if balances.1 < amount {
            return Some(format!("{} doesn't have {} any more; the duel is off.", c.from_name, label(amount)));
        }
        let challenger_wins = random_u64().is_multiple_of(2);
        let (winner_id, loser_id, winner, loser) = if challenger_wins {
            (from_id, uid, c.from_name.clone(), name.clone())
        } else {
            (uid, from_id, name.clone(), c.from_name.clone())
        };
        let moved = self
            .db(move |s| {
                match repo::loyalty::change(s, p, &loser_id, -amount, Floor::Strict)? {
                    Change::Done(_) => {}
                    _ => return Ok(false),
                }
                repo::loyalty::change(s, p, &winner_id, amount, Floor::Clamp)?;
                Ok(true)
            })
            .await
            .ok()?;
        Some(if moved {
            format!("{winner} wins the duel against {loser} and takes {}!", label(amount))
        } else {
            "The duel is off: someone's points changed meanwhile.".into()
        })
    }

    async fn game_title(
        &self,
        actor: &Actor,
        module: Module,
        cfg: &ModulesConfig,
        rest: &str,
        is_mod: bool,
    ) -> Option<String> {
        let last = self.stream_info_saved().await.ok().and_then(|s| s.history.into_iter().next());
        if rest.is_empty() || !is_mod || !cfg.game_title.mods_can_change {
            return Some(match module {
                Module::Title => match last.as_ref().and_then(|h| h.info.title.clone()) {
                    Some(t) => format!("Title: {t}"),
                    None => "The title hasn't been set from Ferrocast yet.".into(),
                },
                _ => match last.as_ref().and_then(|h| h.info.category.as_ref().map(|c| c.name.clone())) {
                    Some(g) => format!("Category: {g}"),
                    None => "The category hasn't been set from Ferrocast yet.".into(),
                },
            });
        }
        let mut info = ferrocast_core::stream_info::StreamInfo::default();
        if module == Module::Title {
            info.title = Some(rest.to_string());
        } else {
            info.category =
                Some(ferrocast_core::stream_info::Category { name: rest.to_string(), ids: Default::default() });
        }
        let what = if module == Module::Title { "Title" } else { "Category" };
        Some(match self.apply_stream_info(info, None, Some(&actor.display_name)).await {
            Ok(r) => {
                let ok: Vec<&str> = r.results.iter().filter(|x| x.ok).map(|x| x.platform.label()).collect();
                let bad: Vec<String> = r
                    .results
                    .iter()
                    .filter(|x| !x.ok)
                    .map(|x| {
                        format!(
                            "{} ({})",
                            x.platform.label(),
                            x.error.as_ref().map(|e| e.message.as_str()).unwrap_or("failed")
                        )
                    })
                    .collect();
                match (ok.is_empty(), bad.is_empty()) {
                    (false, true) => format!("{what} updated on {}.", ok.join(", ")),
                    (false, false) => format!("{what} updated on {}; not on {}.", ok.join(", "), bad.join(", ")),
                    _ => format!("Couldn't change the {}: {}.", what.to_lowercase(), bad.join(", ")),
                }
            }
            Err(err) => format!("Couldn't change the {}: {err}", what.to_lowercase()),
        })
    }
}

fn bet_message(name: &str, err: BetError, points: &str) -> String {
    let label = |n| ferrocast_core::loyalty::label(n, points);
    match err {
        BetError::Unreadable => format!("@{name}, bet a number, half, all or a percentage like 10%."),
        BetError::TooSmall(min) => format!("@{name}, the smallest bet is {}.", label(min)),
        BetError::TooBig(max) => format!("@{name}, the biggest bet is {}.", label(max)),
        BetError::NotEnough(have) => format!("@{name}, you only have {}.", label(have)),
    }
}

/// A random number from the operating system (games of chance never use a guessable seed).
pub(crate) fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        // No entropy at all: still never the same answer twice in a row.
        return u64::try_from(now().0).unwrap_or(0).wrapping_mul(6_364_136_223_846_793_005);
    }
    u64::from_le_bytes(b)
}

/// Counter definitions from a dashboard body: `{ name, command?, reply?, enabled?, value? }`.
pub fn counter_from_body(body: &serde_json::Map<String, Value>) -> Result<(CounterDef, i64), EngineError> {
    let label = body.get("name").and_then(Value::as_str).unwrap_or("");
    let name = chatbot::counter_name_from(label);
    chatbot::valid_counter_name(&name).map_err(EngineError::Invalid)?;
    let mut def = CounterDef::new(&name);
    match body.get("command") {
        None => {}
        Some(Value::Null) => def.command = None,
        Some(Value::String(c)) => def.command = Some(c.trim().to_lowercase()),
        Some(_) => return Err(EngineError::Invalid("command: text like !deaths, or null for none.".into())),
    }
    if let Some(r) = body.get("reply") {
        def.reply = r.as_str().ok_or_else(|| EngineError::Invalid("reply: must be text.".into()))?.to_string();
    }
    if let Some(on) = body.get("enabled") {
        def.enabled = on.as_bool().ok_or_else(|| EngineError::Invalid("enabled: must be true or false.".into()))?;
    }
    let value = match body.get("value") {
        None => 0,
        Some(v) => v.as_i64().ok_or_else(|| EngineError::Invalid("value: must be a whole number.".into()))?,
    };
    Ok((def, value))
}

/// Who added what, for exports and the quotes list.
pub fn quote_views(list: Vec<Quote>) -> Vec<HashMap<&'static str, Value>> {
    list.into_iter()
        .map(|q| {
            HashMap::from([
                ("id", json!(q.id)),
                ("text", json!(q.text)),
                ("added_by", json!(q.added_by)),
                ("at", json!(q.at)),
            ])
        })
        .collect()
}
