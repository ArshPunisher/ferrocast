//! Chat commands: `!discord`, `!so @friend`, with roles, cooldowns and template replies.
//!
//! Matching and cooldown bookkeeping are pure; the engine supplies time and the extra
//! variables it knows (uptime, counters) and sends the reply.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::event::{Actor, Millis, Platform, Roles};
use crate::template::{Template, TemplateError, Vars};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Everyone,
    Subs,
    Vips,
    Mods,
}

impl Access {
    pub fn allows(self, roles: Roles) -> bool {
        let is = |r: Roles| roles.contains(r);
        let mod_or_up = is(Roles::BROADCASTER) || is(Roles::MODERATOR);
        match self {
            Access::Everyone => true,
            Access::Subs => {
                mod_or_up || is(Roles::VIP) || is(Roles::SUBSCRIBER) || is(Roles::MEMBER)
            }
            Access::Vips => mod_or_up || is(Roles::VIP),
            Access::Mods => mod_or_up,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub id: String,
    /// With the prefix, e.g. "!discord". Matching ignores case.
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub response: String,
    pub access: Access,
    pub cooldown_s: u32,
    pub user_cooldown_s: u32,
    /// Empty = every platform.
    #[serde(default)]
    pub platforms: Vec<Platform>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("command names start with ! and have no spaces, like !discord")]
    BadName,
    #[error("the reply can't be empty")]
    EmptyResponse,
    #[error("reply: {0}")]
    Template(#[from] TemplateError),
    #[error("'{0}' is already used by another command")]
    Taken(String),
}

/// Longest reply we send. Twitch and Kick cap chat at 500 characters; YouTube at 200
/// (adapters split further).
pub const MAX_REPLY_CHARS: usize = 500;

impl Command {
    pub fn validate(&self, others: &[Command]) -> Result<(), CommandError> {
        for n in std::iter::once(&self.name).chain(&self.aliases) {
            if !valid_name(n) {
                return Err(CommandError::BadName);
            }
        }
        if self.response.trim().is_empty() {
            return Err(CommandError::EmptyResponse);
        }
        Template::parse(&self.response)?;
        for other in others.iter().filter(|o| o.id != self.id) {
            for mine in std::iter::once(&self.name).chain(&self.aliases) {
                if std::iter::once(&other.name)
                    .chain(&other.aliases)
                    .any(|t| t.eq_ignore_ascii_case(mine))
                {
                    return Err(CommandError::Taken(mine.clone()));
                }
            }
        }
        Ok(())
    }

    fn answers_to(&self, word: &str) -> bool {
        self.name.eq_ignore_ascii_case(word)
            || self.aliases.iter().any(|a| a.eq_ignore_ascii_case(word))
    }
}

fn valid_name(n: &str) -> bool {
    let rest = n.strip_prefix('!').unwrap_or("");
    !rest.is_empty()
        && rest.len() <= 32
        && rest
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// A parsed `!command args` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation<'a> {
    pub word: &'a str,
    pub args: Vec<&'a str>,
}

/// Splits a chat message into a command word and arguments, if it is one.
pub fn parse(text: &str) -> Option<Invocation<'_>> {
    let text = text.trim_start();
    if !text.starts_with('!') {
        return None;
    }
    let mut parts = text.split_whitespace();
    let word = parts.next()?;
    if word.len() < 2 {
        return None;
    }
    Some(Invocation {
        word,
        args: parts.take(20).collect(),
    })
}

/// Remembers when commands last ran.
#[derive(Debug, Default)]
pub struct Cooldowns {
    global: HashMap<String, Millis>,
    per_user: HashMap<(String, String), Millis>,
}

/// Why a command didn't run (shown to mods in the log, never in chat).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    NotACommand,
    Unknown,
    Disabled,
    WrongPlatform,
    NotAllowed,
    Cooldown { seconds_left: i64 },
}

/// Finds the command for `text` and checks access and cooldowns. On success, records the use
/// and returns the rendered reply.
pub fn run(
    commands: &[Command],
    cooldowns: &mut Cooldowns,
    platform: Platform,
    actor: &Actor,
    text: &str,
    extra: &Vars,
    now: Millis,
) -> Result<String, Skip> {
    let inv = parse(text).ok_or(Skip::NotACommand)?;
    let cmd = commands
        .iter()
        .find(|c| c.answers_to(inv.word))
        .ok_or(Skip::Unknown)?;
    if !cmd.enabled {
        return Err(Skip::Disabled);
    }
    if !cmd.platforms.is_empty() && !cmd.platforms.contains(&platform) {
        return Err(Skip::WrongPlatform);
    }
    if !cmd.access.allows(actor.roles) {
        return Err(Skip::NotAllowed);
    }
    // Mods skip cooldowns: they use commands to help, not to spam.
    let is_mod = Access::Mods.allows(actor.roles);
    let user_key = (
        cmd.id.clone(),
        actor
            .id
            .clone()
            .unwrap_or_else(|| actor.display_name.clone()),
    );
    if !is_mod {
        if let Some(&last) = cooldowns.global.get(&cmd.id) {
            let left = i64::from(cmd.cooldown_s) - now.seconds_since(last);
            if left > 0 {
                return Err(Skip::Cooldown { seconds_left: left });
            }
        }
        if let Some(&last) = cooldowns.per_user.get(&user_key) {
            let left = i64::from(cmd.user_cooldown_s) - now.seconds_since(last);
            if left > 0 {
                return Err(Skip::Cooldown { seconds_left: left });
            }
        }
    }
    cooldowns.global.insert(cmd.id.clone(), now);
    cooldowns.per_user.insert(user_key, now);

    let mut vars = extra.clone();
    vars.insert("user".into(), actor.display_name.clone());
    vars.insert("name".into(), actor.display_name.clone());
    vars.insert("args".into(), inv.args.join(" "));
    vars.insert("platform".into(), platform.label().into());
    for (i, a) in inv.args.iter().enumerate().take(9) {
        vars.insert(format!("arg{}", i + 1), (*a).to_string());
    }
    let target = inv
        .args
        .first()
        .map(|a| a.trim_start_matches('@'))
        .unwrap_or(&actor.display_name);
    vars.insert("target".into(), target.to_string());
    let reply = Template::parse(&cmd.response)
        .map(|t| t.render(&vars))
        .unwrap_or_default();
    Ok(safe_reply(&reply))
}

/// Makes a reply safe to send: one line, capped, and never starting with `/` or `.`, which
/// some chats treat as a moderation command. A viewer passing "/ban mod" as an argument
/// must not get the bot to run it.
pub fn safe_reply(text: &str) -> String {
    let one_line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut trimmed = one_line.trim().to_string();
    while trimmed.starts_with('/') || trimmed.starts_with('.') {
        trimmed.remove(0);
        trimmed = trimmed.trim_start().to_string();
    }
    trimmed.chars().take(MAX_REPLY_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, response: &str) -> Command {
        Command {
            id: name.trim_start_matches('!').into(),
            name: name.into(),
            aliases: vec![],
            response: response.into(),
            access: Access::Everyone,
            cooldown_s: 10,
            user_cooldown_s: 30,
            platforms: vec![],
            enabled: true,
        }
    }

    fn viewer(name: &str) -> Actor {
        let mut a = Actor::named(Platform::Twitch, name);
        a.id = Some(name.into());
        a
    }

    #[test]
    fn runs_with_args_and_target() {
        let cmds = vec![cmd("!so", "Go follow {target} on {platform}! ({args})")];
        let mut cd = Cooldowns::default();
        let out = run(
            &cmds,
            &mut cd,
            Platform::Twitch,
            &viewer("a"),
            "!SO @Friend extra",
            &Vars::new(),
            Millis(0),
        );
        assert_eq!(out.unwrap(), "Go follow Friend on Twitch! (@Friend extra)");
    }

    #[test]
    fn cooldowns_global_then_per_user_and_mods_skip_them() {
        let cmds = vec![cmd("!d", "x")];
        let mut cd = Cooldowns::default();
        let p = Platform::Twitch;
        assert!(
            run(
                &cmds,
                &mut cd,
                p,
                &viewer("a"),
                "!d",
                &Vars::new(),
                Millis(0)
            )
            .is_ok()
        );
        assert_eq!(
            run(
                &cmds,
                &mut cd,
                p,
                &viewer("b"),
                "!d",
                &Vars::new(),
                Millis(4_000)
            ),
            Err(Skip::Cooldown { seconds_left: 6 })
        );
        assert!(
            run(
                &cmds,
                &mut cd,
                p,
                &viewer("b"),
                "!d",
                &Vars::new(),
                Millis(11_000)
            )
            .is_ok()
        );
        assert!(matches!(
            run(
                &cmds,
                &mut cd,
                p,
                &viewer("a"),
                "!d",
                &Vars::new(),
                Millis(22_000)
            ),
            Err(Skip::Cooldown { .. })
        ));
        let mut m = viewer("m");
        m.roles = Roles::MODERATOR;
        assert!(run(&cmds, &mut cd, p, &m, "!d", &Vars::new(), Millis(22_001)).is_ok());
    }

    #[test]
    fn access_platform_disabled_and_unknown() {
        let mut c = cmd("!mods", "ok");
        c.access = Access::Mods;
        c.platforms = vec![Platform::Kick];
        let mut off = cmd("!off", "x");
        off.enabled = false;
        let cmds = vec![c, off];
        let mut cd = Cooldowns::default();
        assert_eq!(
            run(
                &cmds,
                &mut cd,
                Platform::Twitch,
                &viewer("a"),
                "!mods",
                &Vars::new(),
                Millis(0)
            ),
            Err(Skip::WrongPlatform)
        );
        assert_eq!(
            run(
                &cmds,
                &mut cd,
                Platform::Kick,
                &viewer("a"),
                "!mods",
                &Vars::new(),
                Millis(0)
            ),
            Err(Skip::NotAllowed)
        );
        assert_eq!(
            run(
                &cmds,
                &mut cd,
                Platform::Kick,
                &viewer("a"),
                "!off",
                &Vars::new(),
                Millis(0)
            ),
            Err(Skip::Disabled)
        );
        assert_eq!(
            run(
                &cmds,
                &mut cd,
                Platform::Kick,
                &viewer("a"),
                "!nope",
                &Vars::new(),
                Millis(0)
            ),
            Err(Skip::Unknown)
        );
        assert_eq!(
            run(
                &cmds,
                &mut cd,
                Platform::Kick,
                &viewer("a"),
                "hello !mods",
                &Vars::new(),
                Millis(0)
            ),
            Err(Skip::NotACommand)
        );
        assert_eq!(
            run(
                &cmds,
                &mut cd,
                Platform::Kick,
                &viewer("a"),
                "!",
                &Vars::new(),
                Millis(0)
            ),
            Err(Skip::NotACommand)
        );
    }

    #[test]
    fn viewers_cannot_inject_chat_commands() {
        let cmds = vec![cmd("!echo", "{args}")];
        let mut cd = Cooldowns::default();
        let out = run(
            &cmds,
            &mut cd,
            Platform::Twitch,
            &viewer("a"),
            "!echo /ban streamer",
            &Vars::new(),
            Millis(0),
        );
        assert_eq!(out.unwrap(), "ban streamer");
        assert_eq!(
            safe_reply(". /timeout x\nsecond line"),
            "timeout x second line"
        );
        assert_eq!(
            safe_reply(&"x".repeat(900)).chars().count(),
            MAX_REPLY_CHARS
        );
    }

    #[test]
    fn validation() {
        let a = cmd("!discord", "join {user}");
        assert!(a.validate(&[]).is_ok());
        assert_eq!(
            cmd("discord", "x").validate(&[]),
            Err(CommandError::BadName)
        );
        assert_eq!(
            cmd("!dis cord", "x").validate(&[]),
            Err(CommandError::BadName)
        );
        assert_eq!(cmd("!", "x").validate(&[]), Err(CommandError::BadName));
        assert_eq!(
            cmd("!x", "  ").validate(&[]),
            Err(CommandError::EmptyResponse)
        );
        assert!(matches!(
            cmd("!x", "{oops").validate(&[]),
            Err(CommandError::Template(_))
        ));
        let mut b = cmd("!dc", "x");
        b.aliases = vec!["!Discord".into()];
        assert_eq!(
            b.validate(&[a]),
            Err(CommandError::Taken("!Discord".into()))
        );
    }

    #[test]
    fn access_levels() {
        assert!(Access::Subs.allows(Roles::MEMBER));
        assert!(Access::Subs.allows(Roles::VIP));
        assert!(!Access::Subs.allows(Roles::empty()));
        assert!(Access::Vips.allows(Roles::BROADCASTER));
        assert!(!Access::Mods.allows(Roles::VIP));
    }
}
