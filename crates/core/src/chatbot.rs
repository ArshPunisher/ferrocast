//! Chatbot parity with the hosted bots: named counters (`!deaths +`), quotes (`!quote add`)
//! and the built-in modules (8ball, roulette, duel, dice, followage, watchtime, uptime,
//! game/title, shoutout). Everything here is parsing, validation and outcomes; the engine
//! stores counters and quotes, moves points and talks to the platforms.

use serde::{Deserialize, Serialize};

use crate::template::{Template, Vars};

/// At most this many counters (each one is a chat command too).
pub const MAX_COUNTERS: usize = 100;
/// Counters stay within ±this, so a typo like `!deaths set 9999999999999` can't overflow.
pub const MAX_COUNTER_VALUE: i64 = 1_000_000_000;
/// Longest quote kept.
pub const MAX_QUOTE_CHARS: usize = 400;
/// At most this many quotes.
pub const MAX_QUOTES: i64 = 5_000;

/// A named counter and the chat command that shows and changes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterDef {
    /// `deaths`: lowercase letters, digits and `_`, 1–30. Used as `{count:deaths}`.
    pub name: String,
    /// `!deaths`, or `None` for a counter changed only from the dashboard.
    #[serde(default)]
    pub command: Option<String>,
    /// What the bot says: `{count}`, `{counter}` (the name), `{user}`.
    #[serde(default = "default_reply")]
    pub reply: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

fn default_reply() -> String {
    "{counter}: {count}".into()
}

impl CounterDef {
    pub fn new(name: &str) -> Self {
        CounterDef { name: name.into(), command: Some(format!("!{name}")), reply: default_reply(), enabled: true }
    }

    /// Checks the name, command and reply. Messages name the field.
    pub fn validate(&self) -> Result<(), String> {
        valid_counter_name(&self.name)?;
        if let Some(c) = &self.command {
            let rest = c.strip_prefix('!').unwrap_or("");
            if rest.is_empty()
                || rest.len() > 32
                || !rest.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            {
                return Err("command: start with ! and use letters, digits, - or _ (like !deaths).".into());
            }
        }
        if self.reply.trim().is_empty() || self.reply.chars().count() > 300 {
            return Err("reply: write 1 to 300 characters.".into());
        }
        Template::parse(&self.reply).map_err(|e| format!("reply: {e}"))?;
        Ok(())
    }

    /// The bot's reply for `value`.
    pub fn render(&self, value: i64, user: &str) -> String {
        let mut v = Vars::new();
        v.insert("count".into(), value.to_string());
        v.insert("counter".into(), title_case(&self.name.replace('_', " ")));
        v.insert("user".into(), user.into());
        v.insert(format!("count:{}", self.name), value.to_string());
        Template::parse(&self.reply).map(|t| t.render(&v)).unwrap_or_default()
    }
}

/// Lowercase `a-z 0-9 _`, 1–30 characters.
pub fn valid_counter_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 30
        || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("name: use 1 to 30 lowercase letters, digits or _ (like deaths).".into());
    }
    Ok(())
}

/// Turns what the streamer typed ("Boss Deaths") into a counter name ("boss_deaths").
pub fn counter_name_from(label: &str) -> String {
    let mut out = String::new();
    for c in label.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if (c == ' ' || c == '_' || c == '-') && !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.trim_end_matches('_').chars().take(30).collect()
}

/// What a counter command asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterOp {
    Show,
    Add(i64),
    Set(i64),
    Reset,
}

impl CounterOp {
    pub fn changes(self) -> bool {
        !matches!(self, CounterOp::Show)
    }
}

/// How to use a counter command, for a mod who typed it wrong.
pub fn counter_usage(command: &str) -> String {
    format!("Use {command} to see it, {command} + or {command} - to change it, {command} set 10, or {command} reset.")
}

/// `[]` shows; `+`, `+3`, `+ 3`, `add 3`, `-`, `-2`, `remove 2`, `set 10`, `=10`, `reset`.
/// Anything else (a viewer chatting `!deaths lol`) just shows. `Err` only when an op word
/// comes with a number that isn't one.
/// A counter command with an op word but no usable number (`!deaths set lots`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadCounterArgs;

pub fn parse_counter_args(args: &[&str]) -> Result<CounterOp, BadCounterArgs> {
    let Some(first) = args.first().map(|a| a.to_lowercase()) else { return Ok(CounterOp::Show) };
    let num = |s: Option<&&str>, default: i64| -> Result<i64, BadCounterArgs> {
        match s {
            None => Ok(default),
            Some(n) => n
                .trim()
                .parse::<i64>()
                .map_err(|_| BadCounterArgs)
                .and_then(|v| if v.abs() <= MAX_COUNTER_VALUE { Ok(v) } else { Err(BadCounterArgs) }),
        }
    };
    let first = first.as_str();
    match first {
        "+" | "add" | "inc" | "up" => num(args.get(1), 1).map(|n| CounterOp::Add(n.abs())),
        "-" | "remove" | "sub" | "dec" | "down" => num(args.get(1), 1).map(|n| CounterOp::Add(-n.abs())),
        "set" | "=" => match args.get(1) {
            Some(_) => num(args.get(1), 0).map(CounterOp::Set),
            None => Err(BadCounterArgs),
        },
        "reset" | "clear" => Ok(CounterOp::Reset),
        _ if first.starts_with('+') || first.starts_with('-') => {
            let n: i64 = first.parse().map_err(|_| BadCounterArgs)?;
            if n.abs() > MAX_COUNTER_VALUE { Err(BadCounterArgs) } else { Ok(CounterOp::Add(n)) }
        }
        _ if first.starts_with('=') => num(Some(&&first[1..]), 0).map(CounterOp::Set),
        _ => Ok(CounterOp::Show),
    }
}

/// Applies an op, clamped to ±[`MAX_COUNTER_VALUE`].
pub fn apply_counter(value: i64, op: CounterOp) -> i64 {
    let next = match op {
        CounterOp::Show => value,
        CounterOp::Add(d) => value.saturating_add(d),
        CounterOp::Set(v) => v,
        CounterOp::Reset => 0,
    };
    next.clamp(-MAX_COUNTER_VALUE, MAX_COUNTER_VALUE)
}

/// Counter names a template reads (`{count:deaths}` → `deaths`).
pub fn counters_in(template: &str) -> Vec<String> {
    Template::parse(template)
        .map(|t| t.variables().iter().filter_map(|v| v.strip_prefix("count:")).map(str::to_string).collect::<Vec<_>>())
        .unwrap_or_default()
}

/// Adds `count:<name>` for each counter, for templates.
pub fn counter_vars<'a>(vars: &mut Vars, values: impl IntoIterator<Item = (&'a String, &'a i64)>) {
    for (name, value) in values {
        vars.insert(format!("count:{name}"), value.to_string());
    }
}

// ------------------------------------------------------------------ quotes

/// What `!quote …` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuoteCmd {
    Random,
    Number(i64),
    /// Mods only.
    Add(String),
    /// Mods only.
    Remove(i64),
    Usage,
}

impl QuoteCmd {
    pub fn mods_only(&self) -> bool {
        matches!(self, QuoteCmd::Add(_) | QuoteCmd::Remove(_))
    }
}

pub const QUOTE_USAGE: &str =
    "Use !quote for a random one, !quote 12 for number 12, and (mods) !quote add <text> or !quote remove 12.";

/// Parses what follows `!quote`, keeping the quote's own spacing.
pub fn parse_quote(rest: &str) -> QuoteCmd {
    let rest = rest.trim();
    if rest.is_empty() {
        return QuoteCmd::Random;
    }
    let (word, tail) = match rest.split_once(char::is_whitespace) {
        Some((w, t)) => (w, t.trim()),
        None => (rest, ""),
    };
    let n = |s: &str| s.trim_start_matches('#').parse::<i64>().ok().filter(|n| *n > 0);
    match word.to_lowercase().as_str() {
        "add" | "new" => {
            let text: String = tail.chars().filter(|c| !c.is_control()).collect();
            let text = text.trim().trim_matches('"').trim().to_string();
            if text.is_empty() || text.chars().count() > MAX_QUOTE_CHARS {
                QuoteCmd::Usage
            } else {
                QuoteCmd::Add(text)
            }
        }
        "remove" | "del" | "delete" | "rm" => n(tail).map_or(QuoteCmd::Usage, QuoteCmd::Remove),
        "random" => QuoteCmd::Random,
        w => n(w).map_or(QuoteCmd::Random, QuoteCmd::Number),
    }
}

/// `Quote #12: "gg no re" (Asha, 2026-10-18)`.
pub fn format_quote(id: i64, text: &str, added_by: &str, at_ms: i64) -> String {
    let (y, m, d) = ymd(at_ms);
    let by = if added_by.trim().is_empty() { String::new() } else { format!("{}, ", added_by.trim()) };
    format!("Quote #{id}: \"{text}\" ({by}{y:04}-{m:02}-{d:02})")
}

/// Civil date (UTC) from Unix milliseconds.
pub fn ymd(ms: i64) -> (i64, u32, u32) {
    let days = ms.div_euclid(86_400_000);
    // Howard Hinnant's days-from-civil, inverted.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

// ------------------------------------------------------------------ modules

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Toggle {
    pub enabled: bool,
}

impl Default for Toggle {
    fn default() -> Self {
        Toggle { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouletteConfig {
    pub enabled: bool,
    pub min_bet: i64,
    pub max_bet: i64,
    /// Chance to win, 1–99 %.
    pub win_percent: u8,
}

impl Default for RouletteConfig {
    fn default() -> Self {
        RouletteConfig { enabled: false, min_bet: 10, max_bet: 10_000, win_percent: 45 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DuelConfig {
    pub enabled: bool,
    pub max_bet: i64,
    /// How long the other person has to `!accept`, 10–300 s.
    pub accept_s: u32,
}

impl Default for DuelConfig {
    fn default() -> Self {
        DuelConfig { enabled: false, max_bet: 10_000, accept_s: 60 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameTitleConfig {
    pub enabled: bool,
    /// Mods may change them with `!title <text>` and `!game <name>`.
    pub mods_can_change: bool,
}

impl Default for GameTitleConfig {
    fn default() -> Self {
        GameTitleConfig { enabled: true, mods_can_change: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShoutoutConfig {
    pub enabled: bool,
    /// `{target}`, `{target_game_note}`, `{url}`.
    pub template: String,
}

impl Default for ShoutoutConfig {
    fn default() -> Self {
        ShoutoutConfig { enabled: true, template: "Go show some love to {target}! {target_game_note} {url}".into() }
    }
}

/// The built-in chat modules. A custom command with the same name always wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModulesConfig {
    pub eight_ball: Toggle,
    pub roulette: RouletteConfig,
    pub duel: DuelConfig,
    pub dice: Toggle,
    pub followage: Toggle,
    pub watchtime: Toggle,
    pub uptime: Toggle,
    pub game_title: GameTitleConfig,
    pub shoutout: ShoutoutConfig,
    pub quotes: Toggle,
}

impl ModulesConfig {
    pub fn validate(&self) -> Result<(), String> {
        let r = &self.roulette;
        if !(1..=99).contains(&r.win_percent) {
            return Err("roulette.win_percent: between 1 and 99.".into());
        }
        if r.min_bet < 1 || r.max_bet < r.min_bet || r.max_bet > 1_000_000_000 {
            return Err(
                "roulette: the smallest bet is at least 1 and no more than the biggest (up to 1,000,000,000).".into()
            );
        }
        if self.duel.max_bet < 1 || self.duel.max_bet > 1_000_000_000 {
            return Err("duel.max_bet: between 1 and 1,000,000,000.".into());
        }
        if !(10..=300).contains(&self.duel.accept_s) {
            return Err("duel.accept_s: between 10 and 300 seconds.".into());
        }
        let t = self.shoutout.template.trim();
        if t.is_empty() || t.chars().count() > 300 {
            return Err("shoutout.template: write 1 to 300 characters.".into());
        }
        Template::parse(t).map_err(|e| format!("shoutout.template: {e}"))?;
        Ok(())
    }
}

/// The module a chat word belongs to, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Module {
    EightBall,
    Roulette,
    Duel,
    Accept,
    Deny,
    Dice,
    Followage,
    Watchtime,
    Uptime,
    Game,
    Title,
    Shoutout,
    Quote,
}

pub fn module_for(word: &str) -> Option<Module> {
    Some(match word.to_lowercase().as_str() {
        "!8ball" | "!eightball" => Module::EightBall,
        "!roulette" | "!gamble" => Module::Roulette,
        "!duel" => Module::Duel,
        "!accept" => Module::Accept,
        "!deny" | "!decline" => Module::Deny,
        "!dice" | "!roll" => Module::Dice,
        "!followage" | "!followed" => Module::Followage,
        "!watchtime" => Module::Watchtime,
        "!uptime" => Module::Uptime,
        "!game" | "!category" => Module::Game,
        "!title" => Module::Title,
        "!so" | "!shoutout" => Module::Shoutout,
        "!quote" | "!quotes" => Module::Quote,
        _ => return None,
    })
}

impl ModulesConfig {
    pub fn enabled(&self, m: Module) -> bool {
        match m {
            Module::EightBall => self.eight_ball.enabled,
            Module::Roulette => self.roulette.enabled,
            Module::Duel | Module::Accept | Module::Deny => self.duel.enabled,
            Module::Dice => self.dice.enabled,
            Module::Followage => self.followage.enabled,
            Module::Watchtime => self.watchtime.enabled,
            Module::Uptime => self.uptime.enabled,
            Module::Game | Module::Title => self.game_title.enabled,
            Module::Shoutout => self.shoutout.enabled,
            Module::Quote => self.quotes.enabled,
        }
    }
}

const EIGHT_BALL: [&str; 20] = [
    "It is certain.",
    "Without a doubt.",
    "You may rely on it.",
    "Yes, definitely.",
    "As I see it, yes.",
    "Most likely.",
    "Outlook good.",
    "Yes.",
    "Signs point to yes.",
    "It is decidedly so.",
    "Reply hazy, try again.",
    "Ask again later.",
    "Better not tell you now.",
    "Cannot predict now.",
    "Concentrate and ask again.",
    "Don't count on it.",
    "My reply is no.",
    "My sources say no.",
    "Outlook not so good.",
    "Very doubtful.",
];

/// An answer for `!8ball`, picked by `roll`.
pub fn eight_ball(roll: u64) -> &'static str {
    let i = usize::try_from(roll % EIGHT_BALL.len() as u64).unwrap_or(0);
    EIGHT_BALL.get(i).copied().unwrap_or("Ask again later.")
}

/// `NdM`: 1–10 dice of 2–1000 sides. Default one six-sided die.
pub fn parse_dice(spec: Option<&str>) -> Result<(u32, u32), String> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else { return Ok((1, 6)) };
    let lower = spec.to_lowercase();
    let (n, m) = match lower.split_once('d') {
        Some(("", m)) => ("1", m),
        Some((n, m)) => (n, m),
        None => ("1", lower.as_str()),
    };
    let n: u32 = n.parse().map_err(|_| "Roll like !roll 2d6 (1 to 10 dice, 2 to 1000 sides).".to_string())?;
    let m: u32 = m.parse().map_err(|_| "Roll like !roll 2d6 (1 to 10 dice, 2 to 1000 sides).".to_string())?;
    if !(1..=10).contains(&n) || !(2..=1000).contains(&m) {
        return Err("Roll like !roll 2d6 (1 to 10 dice, 2 to 1000 sides).".into());
    }
    Ok((n, m))
}

/// "rolled 2d6: 3 + 5 = 8" from the dice and one random number per die.
pub fn format_roll(n: u32, m: u32, rolls: &[u64]) -> String {
    let faces: Vec<u64> = rolls.iter().take(n as usize).map(|r| r % u64::from(m) + 1).collect();
    let total: u64 = faces.iter().sum();
    if faces.len() == 1 {
        format!("rolled a d{m}: {total}")
    } else {
        let parts: Vec<String> = faces.iter().map(u64::to_string).collect();
        format!("rolled {n}d{m}: {} = {total}", parts.join(" + "))
    }
}

/// How a bet was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BetError {
    /// Not a number, "all", "half" or "N%".
    Unreadable,
    TooSmall(i64),
    TooBig(i64),
    NotEnough(i64),
}

/// "100", "all", "half", "25%" against a balance and the module's limits.
pub fn parse_bet(arg: Option<&str>, balance: i64, min: i64, max: i64) -> Result<i64, BetError> {
    let arg = arg.map(|a| a.trim().to_lowercase()).ok_or(BetError::Unreadable)?;
    let amount = match arg.as_str() {
        "all" | "allin" | "all-in" => balance.min(max),
        "half" => balance / 2,
        a if a.ends_with('%') => {
            let p: i64 = a.trim_end_matches('%').parse().map_err(|_| BetError::Unreadable)?;
            if !(1..=100).contains(&p) {
                return Err(BetError::Unreadable);
            }
            balance.saturating_mul(p) / 100
        }
        a => a.replace(',', "").parse::<i64>().map_err(|_| BetError::Unreadable)?,
    };
    if amount < min {
        return Err(BetError::TooSmall(min));
    }
    if amount > max {
        return Err(BetError::TooBig(max));
    }
    if amount > balance {
        return Err(BetError::NotEnough(balance));
    }
    Ok(amount)
}

/// Whether a roulette spin wins: `roll` is any random number.
pub fn roulette_wins(win_percent: u8, roll: u64) -> bool {
    roll % 100 < u64::from(win_percent.clamp(1, 99))
}

/// "2 years, 3 months", "4 days, 2 hours", "35 minutes", "less than a minute".
pub fn long_duration(ms: i64) -> String {
    let mins = ms.max(0) / 60_000;
    if mins < 1 {
        return "less than a minute".into();
    }
    let hours = mins / 60;
    let days = hours / 24;
    let unit = |n: i64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if days >= 365 {
        let years = days / 365;
        let months = (days % 365) / 30;
        return if months > 0 {
            format!("{}, {}", unit(years, "year", "years"), unit(months, "month", "months"))
        } else {
            unit(years, "year", "years")
        };
    }
    if days >= 30 {
        let months = days / 30;
        let rest = days % 30;
        return if rest > 0 {
            format!("{}, {}", unit(months, "month", "months"), unit(rest, "day", "days"))
        } else {
            unit(months, "month", "months")
        };
    }
    if days >= 1 {
        let h = hours % 24;
        return if h > 0 {
            format!("{}, {}", unit(days, "day", "days"), unit(h, "hour", "hours"))
        } else {
            unit(days, "day", "days")
        };
    }
    if hours >= 1 {
        let m = mins % 60;
        return if m > 0 {
            format!("{}, {}", unit(hours, "hour", "hours"), unit(m, "minute", "minutes"))
        } else {
            unit(hours, "hour", "hours")
        };
    }
    unit(mins, "minute", "minutes")
}

/// A duel waiting for `!accept`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    /// (platform key, user id) of who challenged and who was challenged.
    pub from: (String, String),
    pub from_name: String,
    pub to_login: String,
    pub amount: i64,
    pub expires_at: i64,
}

/// Open duels, one per challenged person.
#[derive(Debug, Default)]
pub struct Duels {
    pub open: Vec<Challenge>,
}

impl Duels {
    /// Adds a challenge, unless the challenger or the target is already in one.
    pub fn challenge(&mut self, c: Challenge, now: i64) -> Result<(), &'static str> {
        self.expire(now);
        if self.open.len() >= 200 {
            return Err("Too many duels at once; try again in a minute.");
        }
        if self.open.iter().any(|o| o.from == c.from) {
            return Err("You already have a duel waiting.");
        }
        if self.open.iter().any(|o| o.to_login.eq_ignore_ascii_case(&c.to_login)) {
            return Err("They already have a duel waiting.");
        }
        self.open.push(c);
        Ok(())
    }

    /// The challenge waiting for `login` (removes it).
    pub fn take_for(&mut self, platform: &str, login: &str, now: i64) -> Option<Challenge> {
        self.expire(now);
        let i = self.open.iter().position(|o| o.from.0 == platform && o.to_login.eq_ignore_ascii_case(login))?;
        Some(self.open.remove(i))
    }

    pub fn expire(&mut self, now: i64) {
        self.open.retain(|o| o.expires_at > now);
    }
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start = true;
    for c in s.chars() {
        if start {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
        start = c == ' ';
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_names_and_defs() {
        assert!(valid_counter_name("deaths").is_ok());
        assert!(valid_counter_name("boss_2").is_ok());
        for bad in ["", "Deaths", "dea ths", "x".repeat(31).as_str(), "d€aths", "count:x"] {
            assert!(valid_counter_name(bad).is_err(), "{bad}");
        }
        assert_eq!(counter_name_from("  Boss Deaths!! "), "boss_deaths");
        assert_eq!(counter_name_from("--"), "");
        assert_eq!(counter_name_from(&"Long ".repeat(20)).len(), 30);
        let d = CounterDef::new("deaths");
        assert_eq!(d.command.as_deref(), Some("!deaths"));
        assert!(d.validate().is_ok());
        assert_eq!(d.render(7, "Asha"), "Deaths: 7");
        let bad_cmd = CounterDef { command: Some("deaths".into()), ..d.clone() };
        assert!(bad_cmd.validate().unwrap_err().starts_with("command:"));
        let bad_reply = CounterDef { reply: "{oops".into(), ..d.clone() };
        assert!(bad_reply.validate().unwrap_err().starts_with("reply:"));
        let empty = CounterDef { reply: "  ".into(), ..d.clone() };
        assert!(empty.validate().is_err());
        let custom = CounterDef { reply: "{user}, we died {count:deaths} times".into(), ..d };
        assert_eq!(custom.render(3, "Mo"), "Mo, we died 3 times");
    }

    #[test]
    fn counter_args() {
        use CounterOp::*;
        let p = |s: &str| parse_counter_args(&s.split_whitespace().collect::<Vec<_>>());
        assert_eq!(p(""), Ok(Show));
        assert_eq!(p("lol nice"), Ok(Show), "viewers chatting just see it");
        assert_eq!(p("+"), Ok(Add(1)));
        assert_eq!(p("+5"), Ok(Add(5)));
        assert_eq!(p("+ 5"), Ok(Add(5)));
        assert_eq!(p("add 5"), Ok(Add(5)));
        assert_eq!(p("-"), Ok(Add(-1)));
        assert_eq!(p("-3"), Ok(Add(-3)));
        assert_eq!(p("remove 2"), Ok(Add(-2)));
        assert_eq!(p("set 10"), Ok(Set(10)));
        assert_eq!(p("=10"), Ok(Set(10)));
        assert_eq!(p("SET -4"), Ok(Set(-4)));
        assert_eq!(p("reset"), Ok(Reset));
        assert_eq!(p("set"), Err(BadCounterArgs));
        assert_eq!(p("set abc"), Err(BadCounterArgs));
        assert_eq!(p("+ 99999999999999"), Err(BadCounterArgs));
        assert_eq!(p("+1e9"), Err(BadCounterArgs));
        assert_eq!(apply_counter(5, Add(-10)), -5);
        assert_eq!(apply_counter(MAX_COUNTER_VALUE, Add(5)), MAX_COUNTER_VALUE);
        assert_eq!(apply_counter(i64::MIN + 1, Add(-5)), -MAX_COUNTER_VALUE);
        assert_eq!(apply_counter(9, Reset), 0);
        assert!(Reset.changes() && !Show.changes());
        assert!(counter_usage("!deaths").contains("!deaths set 10"));
    }

    #[test]
    fn counter_variables_in_templates() {
        assert_eq!(counters_in("Died {count:deaths} times, {count:wins} wins {user}"), vec!["deaths", "wins"]);
        assert!(counters_in("{broken").is_empty());
        assert!(Template::parse("{name:x}").is_err(), "only count: takes a colon");
        assert!(Template::parse("{count:}").is_err());
        assert!(Template::parse("{count:a b}").is_err());
        let mut v = Vars::new();
        let values: std::collections::BTreeMap<String, i64> = [("deaths".to_string(), 4)].into();
        counter_vars(&mut v, &values);
        let t = Template::parse("RIP x{count:deaths}").unwrap();
        assert_eq!(t.render(&v), "RIP x4");
    }

    #[test]
    fn quotes() {
        assert_eq!(parse_quote(""), QuoteCmd::Random);
        assert_eq!(parse_quote("random"), QuoteCmd::Random);
        assert_eq!(parse_quote("12"), QuoteCmd::Number(12));
        assert_eq!(parse_quote("#12"), QuoteCmd::Number(12));
        assert_eq!(parse_quote("0"), QuoteCmd::Random);
        assert_eq!(parse_quote("hello there"), QuoteCmd::Random);
        assert_eq!(parse_quote("add   \"gg  no re\" "), QuoteCmd::Add("gg  no re".into()));
        assert_eq!(parse_quote("add"), QuoteCmd::Usage);
        assert_eq!(parse_quote(&format!("add {}", "x".repeat(MAX_QUOTE_CHARS + 1))), QuoteCmd::Usage);
        assert_eq!(parse_quote("remove 3"), QuoteCmd::Remove(3));
        assert_eq!(parse_quote("del #3"), QuoteCmd::Remove(3));
        assert_eq!(parse_quote("remove x"), QuoteCmd::Usage);
        assert!(QuoteCmd::Add("x".into()).mods_only() && !QuoteCmd::Random.mods_only());
        assert_eq!(parse_quote("add line\u{0}two"), QuoteCmd::Add("linetwo".into()));
        // 2026-10-18 00:00 UTC
        assert_eq!(format_quote(7, "gg", "Asha", 1_792_281_600_000), "Quote #7: \"gg\" (Asha, 2026-10-18)");
        assert_eq!(format_quote(7, "gg", " ", 0), "Quote #7: \"gg\" (1970-01-01)");
        assert_eq!(ymd(951_782_400_000), (2000, 2, 29));
    }

    #[test]
    fn modules_config_and_words() {
        let c = ModulesConfig::default();
        assert!(c.validate().is_ok());
        assert!(c.eight_ball.enabled && !c.roulette.enabled && !c.duel.enabled, "points games are opt-in");
        let mut bad = c.clone();
        bad.roulette.win_percent = 0;
        assert!(bad.validate().unwrap_err().starts_with("roulette.win_percent"));
        let mut bad = c.clone();
        bad.roulette.min_bet = 50;
        bad.roulette.max_bet = 10;
        assert!(bad.validate().is_err());
        let mut bad = c.clone();
        bad.duel.accept_s = 5;
        assert!(bad.validate().unwrap_err().starts_with("duel.accept_s"));
        let mut bad = c.clone();
        bad.shoutout.template = "{x".into();
        assert!(bad.validate().unwrap_err().starts_with("shoutout.template"));
        assert_eq!(module_for("!8BALL"), Some(Module::EightBall));
        assert_eq!(module_for("!roll"), Some(Module::Dice));
        assert_eq!(module_for("!so"), Some(Module::Shoutout));
        assert_eq!(module_for("!nope"), None);
        assert!(!c.enabled(Module::Accept));
        let partial: ModulesConfig = serde_json::from_str(r#"{"dice":{"enabled":false}}"#).unwrap();
        assert!(!partial.dice.enabled && partial.eight_ball.enabled, "missing fields keep defaults");
    }

    #[test]
    fn games() {
        assert_eq!(eight_ball(0), "It is certain.");
        assert_eq!(eight_ball(u64::MAX), EIGHT_BALL[(u64::MAX % 20) as usize]);
        assert_eq!(parse_dice(None), Ok((1, 6)));
        assert_eq!(parse_dice(Some("2d20")), Ok((2, 20)));
        assert_eq!(parse_dice(Some("d8")), Ok((1, 8)));
        assert_eq!(parse_dice(Some("100")), Ok((1, 100)));
        for bad in ["0d6", "11d6", "2d1", "2d1001", "xd6", "2dx", "-1d6"] {
            assert!(parse_dice(Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(format_roll(1, 6, &[5]), "rolled a d6: 6");
        assert_eq!(format_roll(2, 6, &[0, 7]), "rolled 2d6: 1 + 2 = 3");
        assert_eq!(parse_bet(Some("100"), 500, 10, 1000), Ok(100));
        assert_eq!(parse_bet(Some("1,000"), 5000, 10, 1000), Ok(1000));
        assert_eq!(parse_bet(Some("all"), 500, 10, 1000), Ok(500));
        assert_eq!(parse_bet(Some("all"), 5000, 10, 1000), Ok(1000), "all is capped by the limit");
        assert_eq!(parse_bet(Some("half"), 500, 10, 1000), Ok(250));
        assert_eq!(parse_bet(Some("10%"), 500, 10, 1000), Ok(50));
        assert_eq!(parse_bet(Some("0%"), 500, 10, 1000), Err(BetError::Unreadable));
        assert_eq!(parse_bet(Some("lots"), 500, 10, 1000), Err(BetError::Unreadable));
        assert_eq!(parse_bet(None, 500, 10, 1000), Err(BetError::Unreadable));
        assert_eq!(parse_bet(Some("5"), 500, 10, 1000), Err(BetError::TooSmall(10)));
        assert_eq!(parse_bet(Some("5000"), 9000, 10, 1000), Err(BetError::TooBig(1000)));
        assert_eq!(parse_bet(Some("600"), 500, 10, 1000), Err(BetError::NotEnough(500)));
        assert_eq!(parse_bet(Some("-50"), 500, 10, 1000), Err(BetError::TooSmall(10)));
        assert_eq!(parse_bet(Some("all"), 0, 10, 1000), Err(BetError::TooSmall(10)));
        assert!(roulette_wins(45, 44) && !roulette_wins(45, 45));
        assert!(!roulette_wins(0, 1) && roulette_wins(0, 0), "never a 0 % game: at least 1 %");
        assert!(roulette_wins(100, 98) && !roulette_wins(100, 99));
    }

    #[test]
    fn durations() {
        let min = 60_000;
        assert_eq!(long_duration(-5), "less than a minute");
        assert_eq!(long_duration(30_000), "less than a minute");
        assert_eq!(long_duration(min), "1 minute");
        assert_eq!(long_duration(35 * min), "35 minutes");
        assert_eq!(long_duration(60 * min), "1 hour");
        assert_eq!(long_duration(125 * min), "2 hours, 5 minutes");
        assert_eq!(long_duration(26 * 60 * min), "1 day, 2 hours");
        assert_eq!(long_duration(45 * 24 * 60 * min), "1 month, 15 days");
        assert_eq!(long_duration(400 * 24 * 60 * min), "1 year, 1 month");
        assert_eq!(long_duration(730 * 24 * 60 * min), "2 years");
        assert!(!long_duration(i64::MAX).is_empty());
    }

    #[test]
    fn duels() {
        let mut d = Duels::default();
        let c = |from: &str, to: &str, exp: i64| Challenge {
            from: ("twitch".into(), from.into()),
            from_name: from.into(),
            to_login: to.into(),
            amount: 50,
            expires_at: exp,
        };
        assert!(d.challenge(c("a", "b", 100), 0).is_ok());
        assert_eq!(d.challenge(c("a", "c", 100), 0), Err("You already have a duel waiting."));
        assert_eq!(d.challenge(c("x", "B", 100), 0), Err("They already have a duel waiting."));
        assert!(d.take_for("youtube", "b", 10).is_none(), "another platform's b");
        assert_eq!(d.take_for("twitch", "B", 10).map(|c| c.amount), Some(50));
        assert!(d.take_for("twitch", "b", 10).is_none(), "taken once");
        assert!(d.challenge(c("a", "b", 100), 0).is_ok());
        assert!(d.take_for("twitch", "b", 100).is_none(), "expired");
        assert!(d.open.is_empty());
    }
}
