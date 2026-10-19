//! The cloud bot's rules (Pro F1, API.md §9.7): the config a PC pushes, and how one chat
//! line is answered while the PC is off.
//!
//! Nothing here is new behaviour: it puts the desktop bot's own pieces together in the
//! desktop's order, so a viewer can't tell which one answered. Custom commands
//! ([`command::run`]) win over counters ([`chatbot::counter_op_for`], the studio cooldowns),
//! built-in module words stay with the PC, and auto-replies ([`autoreply::run`] with the
//! same send budget) only answer chat no command took. Timers use [`Timer::due`] and
//! [`timer_text`], which the desktop's timer loop uses too.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::autoreply::{self, AutoReply, RuleSet};
use crate::chatbot::{self, BadCounterArgs, CounterDef, CounterOp, KeyedCooldowns};
use crate::command::{self, Access, Command, Skip, Timer};
use crate::event::{Actor, Millis, Platform};
use crate::template::{Template, Vars};

/// Most custom commands a pushed config may have.
pub const MAX_COMMANDS: usize = 500;
/// Most timers a pushed config may have.
pub const MAX_TIMERS: usize = 50;
/// The largest pushed config (the relay's body limit).
pub const MAX_CONFIG_BYTES: usize = 256 * 1024;

/// A counter as the PC pushes it: its definition and its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudCounter {
    pub name: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default = "default_reply")]
    pub reply: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub value: i64,
}

fn yes() -> bool {
    true
}

fn default_reply() -> String {
    "{counter}: {count}".into()
}

impl CloudCounter {
    pub fn def(&self) -> CounterDef {
        CounterDef {
            name: self.name.clone(),
            command: self.command.clone(),
            reply: self.reply.clone(),
            enabled: self.enabled,
        }
    }
}

/// What a PC pushes (`PUT /v1/pro/bot/config` `config`): the desktop's own shapes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BotConfig {
    /// The desktop's bot switch. Off: the cloud is quiet too.
    pub bot_on: bool,
    pub commands: Vec<Command>,
    pub timers: Vec<Timer>,
    pub auto_replies: Vec<AutoReply>,
    pub counters: Vec<CloudCounter>,
}

/// Why a pushed config is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("Up to {MAX_COMMANDS} commands can run in the cloud.")]
    TooManyCommands,
    #[error("Up to {MAX_TIMERS} timers can run in the cloud.")]
    TooManyTimers,
    /// The message starts with the item: `commands[3] (!discord): …`.
    #[error("{0}")]
    Invalid(String),
}

impl BotConfig {
    /// The same checks the desktop makes when each item is saved, plus the cloud's limits.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.commands.len() > MAX_COMMANDS {
            return Err(ConfigError::TooManyCommands);
        }
        if self.timers.len() > MAX_TIMERS {
            return Err(ConfigError::TooManyTimers);
        }
        if self.counters.len() > chatbot::MAX_COUNTERS {
            return Err(ConfigError::Invalid(format!("counters: up to {} counters.", chatbot::MAX_COUNTERS)));
        }
        if self.auto_replies.len() > autoreply::MAX_RULES {
            return Err(ConfigError::Invalid(format!("auto_replies: up to {} rules.", autoreply::MAX_RULES)));
        }
        for (i, c) in self.commands.iter().enumerate() {
            let mut others: Vec<Command> = self.commands[..i].to_vec();
            // Blank ids would all look like one command to `validate`.
            for (j, o) in others.iter_mut().enumerate() {
                if o.id.is_empty() || o.id == c.id {
                    o.id = format!("#{j}");
                }
            }
            c.validate(&others).map_err(|e| ConfigError::Invalid(format!("commands[{i}] ({}): {e}", c.name)))?;
        }
        for (i, t) in self.timers.iter().enumerate() {
            t.validate().map_err(|e| ConfigError::Invalid(format!("timers[{i}] ({}): {e}", t.name)))?;
        }
        for (i, c) in self.counters.iter().enumerate() {
            c.def().validate().map_err(|e| ConfigError::Invalid(format!("counters[{i}] ({}): {e}", c.name)))?;
            if c.value.abs() > chatbot::MAX_COUNTER_VALUE {
                return Err(ConfigError::Invalid(format!(
                    "counters[{i}] ({}): value: between -1,000,000,000 and 1,000,000,000.",
                    c.name
                )));
            }
            if self.counters[..i].iter().any(|o| o.name == c.name) {
                return Err(ConfigError::Invalid(format!("counters[{i}] ({}): name: used twice.", c.name)));
            }
        }
        for (i, r) in self.auto_replies.iter().enumerate() {
            autoreply::compile_rule(r)
                .map_err(|e| ConfigError::Invalid(format!("auto_replies[{i}] ({}): {e}", r.name)))?;
        }
        Ok(())
    }

    /// `{count:<name>}` for every counter.
    pub fn counter_vars(&self, vars: &mut Vars) {
        for c in &self.counters {
            vars.insert(format!("count:{}", c.name), c.value.to_string());
        }
    }
}

/// `{uptime}` and `{uptime_text}`, as the desktop fills them for command replies.
pub fn uptime_vars(vars: &mut Vars, live_since: Option<Millis>, now: Millis) {
    match live_since {
        Some(started) => {
            let mins = now.seconds_since(started) / 60;
            let up = format!("{}h {}m", mins / 60, mins % 60);
            vars.insert("uptime_text".into(), format!("We've been live for {up}."));
            vars.insert("uptime".into(), up);
        }
        None => {
            vars.insert("uptime_text".into(), "We're not live right now.".into());
        }
    }
}

/// A timer's message for one platform (`{platform}` and whatever `vars` has), made safe to
/// send. Empty means nothing to post.
pub fn timer_text(timer: &Timer, vars: &Vars, platform: Platform) -> String {
    let mut v = vars.clone();
    v.insert("platform".into(), platform.label().into());
    let text = Template::parse(&timer.message).map(|tpl| tpl.render(&v)).unwrap_or_default();
    command::safe_reply(&text)
}

/// A chat line as the cloud bot sees it. The caller already dropped the bot's own lines,
/// Shared Chat lines from a partner channel and repeats of a message id.
#[derive(Debug, Clone, Copy)]
pub struct Chat<'a> {
    pub platform: Platform,
    pub actor: &'a Actor,
    pub text: &'a str,
    /// That channel is live now.
    pub live: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerKind {
    Command,
    Counter,
    AutoReply,
}

/// A counter the cloud changed: its new `value` and how (`Add(delta)` for `+`/`-`, `Set` for
/// `set`/`reset`). A PC applies the op, not the value, so a change it made meanwhile isn't
/// overwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterChange {
    pub name: String,
    pub value: i64,
    pub op: CounterOp,
}

/// What to say, and the counter it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub text: String,
    pub kind: AnswerKind,
    pub counter: Option<CounterChange>,
}

/// One channel's memory: cooldowns, the auto-reply budget, when each timer last posted and
/// how many lines chat had then.
#[derive(Debug, Default)]
pub struct Brain {
    commands: command::Cooldowns,
    studio: KeyedCooldowns,
    auto: autoreply::Cooldowns,
    budget: autoreply::SendBudget,
    rules: RuleSet,
    /// The auto-replies `rules` was compiled from.
    rules_from: Vec<AutoReply>,
    timers: HashMap<String, (Millis, u32)>,
    lines: u32,
    /// The takeover the timers were last seeded for.
    seeded_for: Option<Millis>,
}

fn user_key(platform: Platform, actor: &Actor) -> String {
    let p = match platform {
        Platform::Twitch => "twitch",
        Platform::Kick => "kick",
        Platform::Youtube => "youtube",
        _ => "other",
    };
    format!("{p}:{}", actor.id.clone().unwrap_or_else(|| actor.display_name.clone()))
}

impl Brain {
    fn compile(&mut self, cfg: &BotConfig) {
        if self.rules_from != cfg.auto_replies {
            let (set, _bad) = RuleSet::compile(&cfg.auto_replies);
            for gone in self.rules_from.iter().filter(|r| !cfg.auto_replies.contains(r)) {
                self.auto.forget(&gone.id);
            }
            self.rules = set;
            self.rules_from = cfg.auto_replies.clone();
        }
    }

    /// Chat lines seen (for timers' `min_chat_lines`).
    pub fn lines(&self) -> u32 {
        self.lines
    }

    /// Answers one chat line, or not. Counts it for timers either way. `extra` holds the
    /// caller's variables (`{uptime}`, `{channel}`); counters are added here. A counter a
    /// mod changed is changed in `cfg` and named in the answer.
    pub fn on_chat(&mut self, cfg: &mut BotConfig, msg: Chat<'_>, extra: &Vars, now: Millis) -> Option<Answer> {
        self.lines = self.lines.saturating_add(1);
        if !cfg.bot_on {
            return None;
        }
        self.compile(cfg);
        let Chat { platform, actor, text, live } = msg;
        let mut vars = extra.clone();
        cfg.counter_vars(&mut vars);
        if let Some(inv) = command::parse(text) {
            let word = inv.word.to_lowercase();
            let custom = cfg.commands.iter().any(|c| c.enabled && c.answers_to(&word));
            if !custom {
                // Counters (the desktop's studio commands).
                let is_mod = Access::Mods.allows(actor.roles);
                if let Some(i) = cfg
                    .counters
                    .iter()
                    .position(|c| c.enabled && c.command.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(&word)))
                {
                    let def = cfg.counters[i].def();
                    let command = def.command.clone().unwrap_or_default();
                    let op = match chatbot::counter_op_for(&inv.args, is_mod) {
                        Ok(op) => op,
                        Err(BadCounterArgs) => {
                            return Some(Answer {
                                text: command::safe_reply(&chatbot::counter_usage(&command)),
                                kind: AnswerKind::Counter,
                                counter: None,
                            });
                        }
                    };
                    let user = user_key(platform, actor);
                    if !is_mod
                        && (!self.studio.take(&format!("{command}:*"), chatbot::GLOBAL_COOLDOWN_S, now.0)
                            || !self.studio.take(&format!("{command}:{user}"), chatbot::VIEWER_COOLDOWN_S, now.0))
                    {
                        return None;
                    }
                    let mut changed = None;
                    if op.changes() {
                        let prev = cfg.counters[i].value;
                        let next = chatbot::apply_counter(prev, op);
                        cfg.counters[i].value = next;
                        // The clamped step, so a PC adding it lands where the cloud did.
                        let op = match op {
                            CounterOp::Add(_) => CounterOp::Add(next.saturating_sub(prev)),
                            _ => CounterOp::Set(next),
                        };
                        changed = Some(CounterChange { name: def.name.clone(), value: next, op });
                    }
                    let text = command::safe_reply(&def.render(cfg.counters[i].value, &actor.display_name));
                    return (!text.is_empty()).then_some(Answer { text, kind: AnswerKind::Counter, counter: changed });
                }
                // Built-in modules and OBS commands need the PC's data: the PC answers them.
                if chatbot::module_for(&word).is_some()
                    || matches!(word.as_str(), "!scene" | "!brb" | "!back" | "!clip")
                {
                    return None;
                }
            }
            // Any command by that name (even a switched-off one) means it isn't an auto-reply.
            if cfg.commands.iter().any(|c| c.answers_to(inv.word)) {
                return match command::run(&cfg.commands, &mut self.commands, platform, actor, text, &vars, now) {
                    Ok(reply) if !reply.is_empty() => {
                        Some(Answer { text: reply, kind: AnswerKind::Command, counter: None })
                    }
                    Ok(_) | Err(Skip::NotACommand | Skip::Unknown) => None,
                    Err(_) => None,
                };
            }
        }
        if self.rules.is_empty() || self.budget.left(platform, now) == 0 {
            return None;
        }
        let hit =
            autoreply::run(&self.rules, &mut self.auto, autoreply::Message { platform, actor, text, live }, &vars, now)
                .ok()?;
        if !self.budget.take(platform, now) {
            return None;
        }
        Some(Answer { text: hit.reply, kind: AnswerKind::AutoReply, counter: None })
    }

    /// The cloud took over at `since`: each timer counts as posted then, so a timer the PC
    /// just posted isn't posted again at once (the PC's own timer loop counts the same way).
    /// Once per takeover; a timer that posted after `since` keeps its time.
    pub fn takeover(&mut self, cfg: &BotConfig, since: Millis) {
        if self.seeded_for == Some(since) {
            return;
        }
        self.seeded_for = Some(since);
        for t in &cfg.timers {
            let e = self.timers.entry(t.id.clone()).or_insert((since, self.lines));
            if e.0 < since {
                *e = (since, self.lines);
            }
        }
    }

    /// The platform said "slow down": no auto-replies there for `seconds`.
    pub fn rate_limited(&mut self, platform: Platform, seconds: u64, now: Millis) {
        self.budget.pause(platform, seconds, now);
    }

    /// Timer messages due now on this channel (and records them as posted).
    pub fn due_timers(&mut self, cfg: &BotConfig, platform: Platform, live: bool, now: Millis) -> Vec<String> {
        let mut out = Vec::new();
        if !cfg.bot_on {
            return out;
        }
        let mut vars = Vars::new();
        cfg.counter_vars(&mut vars);
        for timer in &cfg.timers {
            if !timer.platforms.is_empty() && !timer.platforms.contains(&platform) {
                continue;
            }
            let prev = self.timers.get(&timer.id).copied();
            let since = self.lines.saturating_sub(prev.map_or(0, |(_, l)| l));
            if !timer.due(prev.map(|(at, _)| at), since, live, now) {
                continue;
            }
            let text = timer_text(timer, &vars, platform);
            if text.is_empty() {
                continue;
            }
            self.timers.insert(timer.id.clone(), (now, self.lines));
            out.push(text);
        }
        // Deleted timers are forgotten.
        let ids: Vec<&String> = cfg.timers.iter().map(|t| &t.id).collect();
        self.timers.retain(|id, _| ids.contains(&id));
        out
    }
}

/// Counter values by name (for overlaying newer cloud changes on a pushed config).
pub fn counter_values(cfg: &BotConfig) -> BTreeMap<String, i64> {
    cfg.counters.iter().map(|c| (c.name.clone(), c.value)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoreply::MatchKind;
    use crate::event::Roles;

    fn cmd(name: &str, response: &str) -> Command {
        Command {
            id: name.trim_start_matches('!').into(),
            name: name.into(),
            aliases: vec![],
            response: response.into(),
            access: Access::Everyone,
            cooldown_s: 0,
            user_cooldown_s: 0,
            platforms: vec![],
            enabled: true,
        }
    }

    fn viewer(name: &str) -> Actor {
        let mut a = Actor::named(Platform::Twitch, name);
        a.id = Some(name.into());
        a
    }

    fn moderator(name: &str) -> Actor {
        let mut a = viewer(name);
        a.roles = Roles::MODERATOR;
        a
    }

    fn config() -> BotConfig {
        let mut auto = AutoReply::new("Discord", MatchKind::Word, "discord", "Join: discord.gg/x");
        auto.id = "ar1".into();
        BotConfig {
            bot_on: true,
            commands: vec![cmd("!lurk", "Enjoy the lurk, {user}!"), cmd("!up", "{uptime_text} {count:deaths}")],
            timers: vec![Timer {
                id: "t1".into(),
                name: "socials".into(),
                message: "Follow on {platform} ({count:deaths})".into(),
                every_min: 10,
                min_chat_lines: 2,
                platforms: vec![],
                enabled: true,
            }],
            auto_replies: vec![auto],
            counters: vec![CloudCounter {
                name: "deaths".into(),
                command: Some("!deaths".into()),
                reply: "{counter}: {count}".into(),
                enabled: true,
                value: 4,
            }],
        }
    }

    fn chat<'a>(actor: &'a Actor, text: &'a str) -> Chat<'a> {
        Chat { platform: Platform::Twitch, actor, text, live: true }
    }

    #[test]
    fn commands_counters_and_auto_replies_answer_like_the_desktop() {
        let mut cfg = config();
        let mut b = Brain::default();
        let a = viewer("asha");
        let mut extra = Vars::new();
        uptime_vars(&mut extra, Some(Millis(0)), Millis(65 * 60_000));
        let r = b.on_chat(&mut cfg, chat(&a, "!lurk"), &extra, Millis(1)).unwrap();
        assert_eq!((r.text.as_str(), r.kind), ("Enjoy the lurk, asha!", AnswerKind::Command));
        let r = b.on_chat(&mut cfg, chat(&a, "!up"), &extra, Millis(2)).unwrap();
        assert_eq!(r.text, "We've been live for 1h 5m. 4");
        // A viewer sees the counter but can't change it.
        let r = b.on_chat(&mut cfg, chat(&a, "!deaths +"), &extra, Millis(3)).unwrap();
        assert_eq!((r.text.as_str(), r.counter.clone()), ("Deaths: 4", None));
        assert!(b.on_chat(&mut cfg, chat(&a, "!deaths"), &extra, Millis(1_000)).is_none(), "viewer cooldown");
        // A mod changes it, with no cooldown, and the change is named.
        let m = moderator("meera");
        let r = b.on_chat(&mut cfg, chat(&m, "!deaths +2"), &extra, Millis(1_500)).unwrap();
        let change = CounterChange { name: "deaths".into(), value: 6, op: CounterOp::Add(2) };
        assert_eq!((r.text.as_str(), r.counter.clone()), ("Deaths: 6", Some(change)));
        let r = b.on_chat(&mut cfg, chat(&m, "!deaths set 9"), &extra, Millis(1_550)).unwrap();
        assert_eq!(r.counter.map(|c| c.op), Some(CounterOp::Set(9)));
        let r = b.on_chat(&mut cfg, chat(&m, "!deaths reset"), &extra, Millis(1_560)).unwrap();
        assert_eq!(r.counter.map(|c| c.op), Some(CounterOp::Set(0)));
        cfg.counters[0].value = chatbot::MAX_COUNTER_VALUE - 1;
        let r = b.on_chat(&mut cfg, chat(&m, "!deaths +5"), &extra, Millis(1_570)).unwrap();
        assert_eq!(r.counter.map(|c| c.op), Some(CounterOp::Add(1)), "the clamped step");
        cfg.counters[0].value = 6;
        assert_eq!(cfg.counters[0].value, 6);
        let r = b.on_chat(&mut cfg, chat(&m, "!deaths set lots"), &extra, Millis(1_600)).unwrap();
        assert!(r.text.starts_with("Use !deaths to see it"), "{}", r.text);
        // Auto-replies answer chat no command took.
        let r = b.on_chat(&mut cfg, chat(&a, "what's the discord?"), &extra, Millis(2_000)).unwrap();
        assert_eq!(r.kind, AnswerKind::AutoReply);
        assert!(b.on_chat(&mut cfg, chat(&a, "discord pls"), &extra, Millis(3_000)).is_none(), "its cooldown");
        // Module words stay with the PC; unknown commands get nothing.
        assert!(b.on_chat(&mut cfg, chat(&a, "!8ball will I win"), &extra, Millis(60_000)).is_none());
        let other = viewer("ravi");
        assert!(
            b.on_chat(&mut cfg, chat(&other, "!nothing discord"), &extra, Millis(60_000)).is_some(),
            "not a command"
        );
    }

    #[test]
    fn a_custom_command_wins_over_a_counter_and_off_means_quiet() {
        let mut cfg = config();
        cfg.commands.push(cmd("!deaths", "custom wins"));
        let mut b = Brain::default();
        let a = viewer("asha");
        let r = b.on_chat(&mut cfg, chat(&a, "!deaths"), &Vars::new(), Millis(0)).unwrap();
        assert_eq!(r.text, "custom wins");
        // A switched-off command by that name: no auto-reply either.
        cfg.commands.last_mut().unwrap().enabled = false;
        cfg.commands.push({
            let mut c = cmd("!discord", "x");
            c.enabled = false;
            c
        });
        assert!(b.on_chat(&mut cfg, chat(&a, "!discord"), &Vars::new(), Millis(100_000)).is_none());
        cfg.bot_on = false;
        assert!(b.on_chat(&mut cfg, chat(&a, "!lurk"), &Vars::new(), Millis(200_000)).is_none());
        assert!(b.due_timers(&cfg, Platform::Twitch, true, Millis(10_000_000)).is_empty());
    }

    #[test]
    fn timers_need_live_chat_and_their_interval() {
        let mut cfg = config();
        let mut b = Brain::default();
        let a = viewer("asha");
        assert!(b.due_timers(&cfg, Platform::Twitch, true, Millis(0)).is_empty(), "no chat yet");
        b.on_chat(&mut cfg, chat(&a, "hi"), &Vars::new(), Millis(0));
        b.on_chat(&mut cfg, chat(&a, "hello"), &Vars::new(), Millis(0));
        assert!(b.due_timers(&cfg, Platform::Twitch, false, Millis(0)).is_empty(), "offline");
        assert_eq!(b.due_timers(&cfg, Platform::Twitch, true, Millis(0)), vec!["Follow on Twitch (4)".to_string()]);
        b.on_chat(&mut cfg, chat(&a, "a"), &Vars::new(), Millis(0));
        b.on_chat(&mut cfg, chat(&a, "b"), &Vars::new(), Millis(0));
        assert!(b.due_timers(&cfg, Platform::Twitch, true, Millis(599_000)).is_empty(), "every 10 minutes");
        assert_eq!(b.due_timers(&cfg, Platform::Twitch, true, Millis(600_000)).len(), 1);
        cfg.timers[0].platforms = vec![Platform::Kick];
        b.on_chat(&mut cfg, chat(&a, "c"), &Vars::new(), Millis(0));
        b.on_chat(&mut cfg, chat(&a, "d"), &Vars::new(), Millis(0));
        assert!(b.due_timers(&cfg, Platform::Twitch, true, Millis(2_000_000)).is_empty(), "another platform");
    }

    #[test]
    fn a_takeover_counts_as_a_post_so_timers_never_double_up() {
        let mut cfg = config();
        cfg.timers[0].min_chat_lines = 0;
        let mut b = Brain::default();
        // The cloud takes over at 100 s: nothing at once (the PC may have just posted).
        b.takeover(&cfg, Millis(100_000));
        assert!(b.due_timers(&cfg, Platform::Twitch, true, Millis(100_000)).is_empty(), "no post at handover");
        assert!(b.due_timers(&cfg, Platform::Twitch, true, Millis(699_999)).is_empty());
        assert_eq!(b.due_timers(&cfg, Platform::Twitch, true, Millis(700_000)).len(), 1, "10 min after takeover");
        // The same takeover again changes nothing; a later one seeds again.
        b.takeover(&cfg, Millis(100_000));
        assert!(b.due_timers(&cfg, Platform::Twitch, true, Millis(1_300_000)).len() == 1);
        b.takeover(&cfg, Millis(1_400_000));
        assert!(b.due_timers(&cfg, Platform::Twitch, true, Millis(1_900_000)).is_empty());
        assert_eq!(b.due_timers(&cfg, Platform::Twitch, true, Millis(2_000_000)).len(), 1);
    }

    #[test]
    fn validation_names_the_item_and_the_limits() {
        assert_eq!(config().validate(), Ok(()));
        let mut c = config();
        c.commands.push(cmd("!lurk", "again"));
        let e = c.validate().unwrap_err().to_string();
        assert!(e.starts_with("commands[2] (!lurk): "), "{e}");
        let mut c = config();
        c.commands = (0..=MAX_COMMANDS).map(|i| cmd(&format!("!c{i}"), "x")).collect();
        assert_eq!(c.validate(), Err(ConfigError::TooManyCommands));
        let mut c = config();
        c.timers = (0..=MAX_TIMERS).map(|_| c.timers[0].clone()).collect();
        assert_eq!(c.validate(), Err(ConfigError::TooManyTimers));
        let mut c = config();
        c.counters[0].value = chatbot::MAX_COUNTER_VALUE + 1;
        assert!(c.validate().unwrap_err().to_string().starts_with("counters[0] (deaths): value"));
        let mut c = config();
        c.auto_replies[0].reply = String::new();
        assert!(c.validate().unwrap_err().to_string().starts_with("auto_replies[0] (Discord): "));
        // Blank ids (never saved on a PC) don't count as the same command.
        let mut c = config();
        for x in &mut c.commands {
            x.id.clear();
        }
        assert_eq!(c.validate(), Ok(()));
        // The shape is the desktop's: an empty object is a quiet, valid config.
        let empty: BotConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.validate(), Ok(()));
        assert!(!empty.bot_on);
    }

    #[test]
    fn the_send_budget_and_a_platform_slowdown_hold_auto_replies() {
        let mut cfg = config();
        cfg.auto_replies[0].cooldown_s = 5;
        cfg.auto_replies[0].user_cooldown_s = 0;
        let mut b = Brain::default();
        let mut sent = 0;
        for i in 0..20 {
            let a = viewer(&format!("v{i}"));
            if b.on_chat(&mut cfg, chat(&a, "discord"), &Vars::new(), Millis(i64::from(i) * 5_000)).is_some() {
                sent += 1;
            }
        }
        assert!(sent > 0 && sent <= 20);
        b.rate_limited(Platform::Twitch, 60, Millis(100_000));
        let a = viewer("late");
        assert!(b.on_chat(&mut cfg, chat(&a, "discord"), &Vars::new(), Millis(110_000)).is_none());
    }

    #[test]
    fn the_counter_op_is_shared() {
        use chatbot::{CounterOp, counter_op_for};
        assert_eq!(counter_op_for(&["+"], true), Ok(CounterOp::Add(1)));
        assert_eq!(counter_op_for(&["+"], false), Ok(CounterOp::Show));
        assert_eq!(counter_op_for(&["set", "x"], true), Err(BadCounterArgs));
        assert_eq!(counter_op_for(&["set", "x"], false), Ok(CounterOp::Show));
        let mut k = KeyedCooldowns::default();
        assert!(k.take("a", 3, 0));
        assert!(!k.take("a", 3, 2_999));
        assert!(k.take("a", 3, 3_000));
    }
}
