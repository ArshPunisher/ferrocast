//! Auto-replies: the bot answers chat that mentions something ("discord" → the invite link),
//! without anyone typing a `!command`. Free and local (PRO.md M5); the Pro cloud bot runs the
//! same rules later.
//!
//! Everything here is pure: validation, compiling the patterns once, matching, cooldowns and
//! the send budget. The engine decides *whether* a message may get an answer at all (live
//! chat from this channel, after moderation, never from a bot, never a command) and sends it.
//!
//! Matching works on a folded copy of both sides: Unicode NFKC (so `ｄｉｓｃｏｒｄ` and a
//! ligature match the plain letters), whitespace runs collapsed to one space, and lowercase
//! unless the rule is case sensitive. Regex rules use the `regex` crate, which runs in linear
//! time (no catastrophic backtracking), with a cap on the compiled size.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::command::{Access, safe_reply};
use crate::event::{Actor, Millis, Platform};
use crate::template::{Template, TemplateError, Vars};

/// At most this many auto-replies.
pub const MAX_RULES: usize = 100;
/// Longest reply (Twitch and Kick cap chat at 500 characters).
pub const MAX_REPLY_CHARS: usize = 500;
/// Longest text or pattern to look for.
pub const MAX_MATCH_CHARS: usize = 200;
/// Longest name.
pub const MAX_NAME_CHARS: usize = 60;
/// Longest cooldown, a day.
pub const MAX_COOLDOWN_S: u32 = 86_400;
/// However the rule is set, one rule answers at most once per this many seconds per chat.
pub const MIN_GLOBAL_COOLDOWN_S: u32 = 5;
/// Only this much of a message is looked at (chat caps messages at 500 characters anyway).
pub const MAX_INPUT_CHARS: usize = 2_000;
/// Compiled regex program cap (bytes). Big enough for any sensible pattern.
pub const REGEX_SIZE_LIMIT: usize = 256 * 1024;
/// Lazy DFA cache cap (bytes).
pub const REGEX_DFA_LIMIT: usize = 512 * 1024;
/// Deepest nesting of groups and repetitions in a pattern.
pub const REGEX_NEST_LIMIT: u32 = 24;

fn yes() -> bool {
    true
}
fn default_cooldown() -> u32 {
    30
}
fn default_user_cooldown() -> u32 {
    120
}

/// How the text is looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// Anywhere in the message: "discord" matches "is there a discord?".
    #[default]
    Contains,
    /// As a whole word or phrase: "game" matches "what game is this" but not "gameplay".
    Word,
    /// The message starts with it: "hi" matches "hi chat" (and "hippo"; use `word` for words).
    StartsWith,
    /// A regular expression (Rust `regex` syntax).
    Regex,
}

/// What a rule looks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    #[serde(default)]
    pub kind: MatchKind,
    pub text: String,
    #[serde(default)]
    pub case_sensitive: bool,
}

/// One auto-reply rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoReply {
    /// Blank when creating; the engine picks one.
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(rename = "match")]
    pub matcher: Match,
    /// A template with the same variables as command replies (`{user}`, `{channel}`, …).
    pub reply: String,
    /// Empty = every platform.
    #[serde(default)]
    pub platforms: Vec<Platform>,
    /// Per rule, per platform chat: answers at most once per this many seconds (at least
    /// [`MIN_GLOBAL_COOLDOWN_S`]).
    #[serde(default = "default_cooldown")]
    pub cooldown_s: u32,
    /// Per viewer: the same person gets this rule's answer at most once per this long.
    #[serde(default = "default_user_cooldown")]
    pub user_cooldown_s: u32,
    /// Who it answers ("everyone", "subs", "vips", "mods"; the same roles as commands).
    #[serde(default = "everyone")]
    pub min_role: Access,
    /// Answer only while that platform is live.
    #[serde(default = "yes")]
    pub only_when_live: bool,
}

fn everyone() -> Access {
    Access::Everyone
}

impl AutoReply {
    /// A rule with the defaults (on, 30 s / 120 s cooldowns, everyone, only while live).
    pub fn new(name: &str, kind: MatchKind, text: &str, reply: &str) -> Self {
        AutoReply {
            id: String::new(),
            name: name.into(),
            enabled: true,
            matcher: Match { kind, text: text.into(), case_sensitive: false },
            reply: reply.into(),
            platforms: vec![],
            cooldown_s: default_cooldown(),
            user_cooldown_s: default_user_cooldown(),
            min_role: Access::Everyone,
            only_when_live: true,
        }
    }
}

/// Why a rule can't be saved. Each message starts with the field it's about.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AutoReplyError {
    #[error("name: write 1 to {MAX_NAME_CHARS} characters.")]
    Name,
    #[error("match.text: write what to look for, 1 to {MAX_MATCH_CHARS} characters.")]
    Text,
    #[error("match.text: the pattern has a problem at character {at}: {problem}.")]
    BadPattern { at: usize, problem: String },
    #[error("match.text: this pattern is too complex. Make it shorter or simpler.")]
    PatternTooBig,
    #[error("match.text: this pattern matches every message. Make it more specific.")]
    MatchesEverything,
    #[error("reply: write what the bot says.")]
    EmptyReply,
    #[error("reply: {len} characters is too long; replies can be up to {MAX_REPLY_CHARS}.")]
    ReplyTooLong { len: usize },
    #[error("reply: {0}")]
    Template(#[from] TemplateError),
    #[error("{0}: use 0 to 86400 seconds.")]
    Cooldown(&'static str),
}

impl AutoReplyError {
    /// The HTTP status the API answers with: 413 for a reply over the limit, 422 for a
    /// pattern that can't be used, 400 for the rest.
    pub fn status(&self) -> u16 {
        match self {
            AutoReplyError::ReplyTooLong { .. } => 413,
            AutoReplyError::BadPattern { .. } | AutoReplyError::PatternTooBig | AutoReplyError::MatchesEverything => {
                422
            }
            _ => 400,
        }
    }

    /// The API error code.
    pub fn code(&self) -> &'static str {
        match self {
            AutoReplyError::ReplyTooLong { .. } => "reply_too_long",
            AutoReplyError::BadPattern { .. } => "bad_pattern",
            AutoReplyError::PatternTooBig => "pattern_too_complex",
            AutoReplyError::MatchesEverything => "pattern_too_broad",
            _ => "invalid",
        }
    }

    /// What to do about it.
    pub fn hint(&self) -> &'static str {
        match self {
            AutoReplyError::BadPattern { .. } => {
                "Check brackets and put a \\ before special characters like ? . ( or [. Or use Contains instead."
            }
            AutoReplyError::PatternTooBig | AutoReplyError::MatchesEverything => {
                "Look for a word or phrase instead, like discord."
            }
            AutoReplyError::ReplyTooLong { .. } => "Shorten the reply; chat cuts longer messages.",
            _ => "Fix the field and save again.",
        }
    }
}

impl AutoReply {
    /// Checks every field and that the pattern compiles.
    pub fn validate(&self) -> Result<(), AutoReplyError> {
        compile_rule(self).map(|_| ())
    }
}

// ------------------------------------------------------------------ compiling

/// A rule ready to match: the folded needle or the compiled regex, and the parsed reply.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub rule: AutoReply,
    matcher: Matcher,
    reply: Template,
}

#[derive(Debug, Clone)]
enum Matcher {
    Contains(String),
    Word(String),
    StartsWith(String),
    Regex(regex::Regex),
}

/// Checks a rule and compiles it once.
pub fn compile_rule(rule: &AutoReply) -> Result<Compiled, AutoReplyError> {
    let name = rule.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(AutoReplyError::Name);
    }
    let text = &rule.matcher.text;
    if text.trim().is_empty() || text.chars().count() > MAX_MATCH_CHARS {
        return Err(AutoReplyError::Text);
    }
    if rule.reply.trim().is_empty() {
        return Err(AutoReplyError::EmptyReply);
    }
    let len = rule.reply.chars().count();
    if len > MAX_REPLY_CHARS {
        return Err(AutoReplyError::ReplyTooLong { len });
    }
    let reply = Template::parse(&rule.reply)?;
    if rule.cooldown_s > MAX_COOLDOWN_S {
        return Err(AutoReplyError::Cooldown("cooldown_s"));
    }
    if rule.user_cooldown_s > MAX_COOLDOWN_S {
        return Err(AutoReplyError::Cooldown("user_cooldown_s"));
    }
    let cs = rule.matcher.case_sensitive;
    let matcher = match rule.matcher.kind {
        MatchKind::Contains => Matcher::Contains(fold(text, cs)),
        MatchKind::Word => Matcher::Word(fold(text, cs)),
        MatchKind::StartsWith => Matcher::StartsWith(fold(text, cs)),
        MatchKind::Regex => Matcher::Regex(compile_regex(text, cs)?),
    };
    Ok(Compiled { rule: rule.clone(), matcher, reply })
}

fn compile_regex(pattern: &str, case_sensitive: bool) -> Result<regex::Regex, AutoReplyError> {
    // Parse first for a message that points at the problem ("at character 4: unclosed group").
    let parsed = regex_syntax::ParserBuilder::new()
        .nest_limit(REGEX_NEST_LIMIT)
        .case_insensitive(!case_sensitive)
        .build()
        .parse(pattern);
    if let Err(e) = parsed {
        let (offset, problem) = match &e {
            regex_syntax::Error::Parse(p) => (p.span().start.offset, p.kind().to_string()),
            regex_syntax::Error::Translate(t) => (t.span().start.offset, t.kind().to_string()),
            other => (0, other.to_string()),
        };
        let at = pattern.get(..offset).map(|s| s.chars().count()).unwrap_or(0) + 1;
        return Err(AutoReplyError::BadPattern { at, problem });
    }
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_DFA_LIMIT)
        .nest_limit(REGEX_NEST_LIMIT)
        .build()
        .map_err(|e| match e {
            regex::Error::CompiledTooBig(_) => AutoReplyError::PatternTooBig,
            other => AutoReplyError::BadPattern { at: 1, problem: other.to_string() },
        })?;
    // `a*` or `.?` would answer every single message.
    if re.is_match("") {
        return Err(AutoReplyError::MatchesEverything);
    }
    Ok(re)
}

/// The folded form both sides are compared in (see the module docs).
pub fn fold(text: &str, case_sensitive: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars().take(MAX_INPUT_CHARS).nfkc() {
        if c.is_whitespace() || c.is_control() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        if case_sensitive {
            out.push(c);
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `needle` appears in `hay` as a whole word or phrase. An edge of the needle that
/// is punctuation (like `?` in "discord?") needs no boundary on that side.
fn has_word(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let first_is_word = needle.chars().next().is_some_and(is_word_char);
    let last_is_word = needle.chars().next_back().is_some_and(is_word_char);
    let mut from = 0;
    while let Some(pos) = hay.get(from..).and_then(|h| h.find(needle)) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = !first_is_word || hay[..start].chars().next_back().is_none_or(|c| !is_word_char(c));
        let after_ok = !last_is_word || hay[end..].chars().next().is_none_or(|c| !is_word_char(c));
        if before_ok && after_ok {
            return true;
        }
        // Step one character on (never into the middle of a UTF-8 sequence).
        from = start + hay[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

impl Compiled {
    /// Whether the message text matches, ignoring every other condition.
    pub fn matches_text(&self, text: &str) -> bool {
        let cs = self.rule.matcher.case_sensitive;
        match &self.matcher {
            Matcher::Contains(n) => !n.is_empty() && fold(text, cs).contains(n.as_str()),
            Matcher::Word(n) => has_word(&fold(text, cs), n),
            Matcher::StartsWith(n) => !n.is_empty() && fold(text, cs).starts_with(n.as_str()),
            Matcher::Regex(re) => {
                let hay: String = text.chars().take(MAX_INPUT_CHARS).nfkc().collect();
                re.is_match(&hay)
            }
        }
    }

    /// Whether this rule applies to this message at all (on, platform, role, live).
    pub fn applies(&self, platform: Platform, actor: &Actor, live: bool) -> bool {
        let r = &self.rule;
        r.enabled
            && (r.platforms.is_empty() || r.platforms.contains(&platform))
            && r.min_role.allows(actor.roles)
            && (live || !r.only_when_live)
    }

    /// The reply for this message: the template with `extra` plus `{user}`, `{name}` and
    /// `{platform}`, made safe to send (one line, capped, no `/` commands).
    pub fn render(&self, platform: Platform, actor: &Actor, extra: &Vars) -> String {
        let mut vars = extra.clone();
        vars.insert("user".into(), actor.display_name.clone());
        vars.insert("name".into(), actor.display_name.clone());
        vars.insert("platform".into(), platform.label().into());
        safe_reply(&self.reply.render(&vars))
    }
}

/// The rules in order, compiled. Rules that no longer compile (a saved pattern from an
/// older build) are left out and reported, never fatal.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    rules: Vec<Compiled>,
}

impl RuleSet {
    pub fn compile(rules: &[AutoReply]) -> (RuleSet, Vec<(String, AutoReplyError)>) {
        let mut ok = Vec::with_capacity(rules.len());
        let mut bad = Vec::new();
        for r in rules.iter().take(MAX_RULES) {
            match compile_rule(r) {
                Ok(c) => ok.push(c),
                Err(e) => bad.push((r.id.clone(), e)),
            }
        }
        (RuleSet { rules: ok }, bad)
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// The first rule that applies and whose text matches. First match wins: a later rule
    /// never answers a message an earlier one matched, even while that one cools down.
    pub fn first_match(&self, platform: Platform, actor: &Actor, text: &str, live: bool) -> Option<&Compiled> {
        self.rules.iter().find(|c| c.applies(platform, actor, live) && c.matches_text(text))
    }

    /// For the dashboard's "Try it": the first enabled rule whose text matches, ignoring
    /// platform, role, live and cooldowns.
    pub fn first_text_match(&self, text: &str) -> Option<&Compiled> {
        self.rules.iter().find(|c| c.rule.enabled && c.matches_text(text))
    }
}

// ------------------------------------------------------------------ cooldowns

/// When each rule last answered, per platform chat and per viewer.
#[derive(Debug, Default)]
pub struct Cooldowns {
    global: HashMap<(String, Platform), Millis>,
    per_user: HashMap<(String, Platform, String), Millis>,
}

/// Why a matching message got no answer (logged, never said in chat).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    NoMatch,
    Cooldown { rule_id: String, seconds_left: i64 },
}

/// A rule answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub rule_id: String,
    pub reply: String,
}

impl Cooldowns {
    /// Seconds until `rule` may answer `user` on `platform` again (0 = now). Everyone waits,
    /// mods too: an auto-reply is never something a mod needs.
    pub fn wait(&self, rule: &AutoReply, platform: Platform, user: &str, now: Millis) -> i64 {
        let key = (rule.id.clone(), platform);
        let global = i64::from(rule.cooldown_s.max(MIN_GLOBAL_COOLDOWN_S));
        let g = self.global.get(&key).map_or(0, |last| global - now.seconds_since(*last));
        let u = self
            .per_user
            .get(&(rule.id.clone(), platform, user.to_string()))
            .map_or(0, |last| i64::from(rule.user_cooldown_s) - now.seconds_since(*last));
        g.max(u).max(0)
    }

    pub fn record(&mut self, rule: &AutoReply, platform: Platform, user: &str, now: Millis) {
        if self.per_user.len() > 20_000 {
            // A day is the longest cooldown, so older entries no longer matter.
            self.per_user.retain(|_, last| now.seconds_since(*last) < i64::from(MAX_COOLDOWN_S));
        }
        self.global.insert((rule.id.clone(), platform), now);
        self.per_user.insert((rule.id.clone(), platform, user.to_string()), now);
    }

    /// Forgets a rule (deleted or edited, so its new settings start fresh).
    pub fn forget(&mut self, rule_id: &str) {
        self.global.retain(|(id, _), _| id != rule_id);
        self.per_user.retain(|(id, _, _), _| id != rule_id);
    }
}

/// Who a viewer is for cooldowns: their platform id, else their name.
pub fn user_key(actor: &Actor) -> String {
    actor.id.clone().unwrap_or_else(|| actor.display_name.to_lowercase())
}

/// A chat message as auto-replies see it.
#[derive(Debug, Clone, Copy)]
pub struct Message<'a> {
    pub platform: Platform,
    pub actor: &'a Actor,
    pub text: &'a str,
    /// Whether that platform is live now.
    pub live: bool,
}

/// Finds the rule for a message, checks its cooldowns, records the use and renders the
/// reply. The caller has already decided the message may be answered at all.
pub fn run(set: &RuleSet, cooldowns: &mut Cooldowns, msg: Message<'_>, extra: &Vars, now: Millis) -> Result<Hit, Skip> {
    let Message { platform, actor, text, live } = msg;
    let c = set.first_match(platform, actor, text, live).ok_or(Skip::NoMatch)?;
    let user = user_key(actor);
    let left = cooldowns.wait(&c.rule, platform, &user, now);
    if left > 0 {
        return Err(Skip::Cooldown { rule_id: c.rule.id.clone(), seconds_left: left });
    }
    let reply = c.render(platform, actor, extra);
    if reply.is_empty() {
        return Err(Skip::NoMatch);
    }
    cooldowns.record(&c.rule, platform, &user, now);
    Ok(Hit { rule_id: c.rule.id.clone(), reply })
}

// ------------------------------------------------------------------ send budget (R-19)

/// How many auto-replies a platform's chat gets per window. Well under each platform's own
/// limit (Twitch: 20 messages per 30 s for a non-mod account), so commands and timers
/// always have room; YouTube is slow because every message costs API quota.
pub fn budget_for(platform: Platform) -> (usize, i64) {
    match platform {
        Platform::Youtube => (2, 60),
        _ => (6, 30),
    }
}

/// A client-side rate budget per platform (R-19): a sliding window of sends, plus a pause
/// when the platform itself said "slow down".
#[derive(Debug, Default)]
pub struct SendBudget {
    sent: HashMap<Platform, VecDeque<Millis>>,
    paused_until: HashMap<Platform, Millis>,
}

impl SendBudget {
    /// Whether one more auto-reply may go to `platform` now; records it when it may.
    pub fn take(&mut self, platform: Platform, now: Millis) -> bool {
        if self.paused_until.get(&platform).is_some_and(|until| now.0 < until.0) {
            return false;
        }
        let (max, window_s) = budget_for(platform);
        let q = self.sent.entry(platform).or_default();
        while q.front().is_some_and(|t| now.seconds_since(*t) >= window_s) {
            q.pop_front();
        }
        if q.len() >= max {
            return false;
        }
        q.push_back(now);
        true
    }

    /// How many sends are left in the window right now (for diagnostics).
    pub fn left(&self, platform: Platform, now: Millis) -> usize {
        let (max, window_s) = budget_for(platform);
        if self.paused_until.get(&platform).is_some_and(|until| now.0 < until.0) {
            return 0;
        }
        let used =
            self.sent.get(&platform).map_or(0, |q| q.iter().filter(|t| now.seconds_since(**t) < window_s).count());
        max.saturating_sub(used)
    }

    /// The platform answered "rate limited": no auto-replies there for `seconds`.
    pub fn pause(&mut self, platform: Platform, seconds: u64, now: Millis) {
        let ms = i64::try_from(seconds.saturating_mul(1000)).unwrap_or(i64::MAX);
        self.paused_until.insert(platform, Millis(now.0.saturating_add(ms)));
    }
}

// ------------------------------------------------------------------ starters

/// The examples the dashboard offers on an empty list.
pub fn starters() -> Vec<AutoReply> {
    vec![
        AutoReply::new("Discord link", MatchKind::Word, "discord", "Join the Discord: discord.gg/yourcode"),
        AutoReply::new("What game", MatchKind::Contains, "what game", "@{user} we're playing {game} right now."),
        AutoReply::new("Greeting", MatchKind::StartsWith, "hello", "Welcome in, {user}!"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Roles;

    fn rule(kind: MatchKind, text: &str) -> AutoReply {
        let mut r = AutoReply::new("r", kind, text, "hi {user} on {platform}");
        r.id = format!("{kind:?}:{text}");
        r
    }

    fn compiled(kind: MatchKind, text: &str) -> Compiled {
        compile_rule(&rule(kind, text)).unwrap()
    }

    fn viewer(name: &str) -> Actor {
        let mut a = Actor::named(Platform::Twitch, name);
        a.id = Some(name.into());
        a
    }

    fn msg<'a>(platform: Platform, actor: &'a Actor, text: &'a str) -> Message<'a> {
        Message { platform, actor, text, live: true }
    }

    #[test]
    fn contains_ignores_case_and_spacing() {
        let c = compiled(MatchKind::Contains, "what   Game");
        assert!(c.matches_text("hey WHAT game is this"));
        assert!(c.matches_text("what\tgame"));
        assert!(!c.matches_text("whatgame"));
        assert!(!c.matches_text("what is the game"));
    }

    #[test]
    fn case_sensitive_when_asked() {
        let mut r = rule(MatchKind::Contains, "VOD");
        r.matcher.case_sensitive = true;
        let c = compile_rule(&r).unwrap();
        assert!(c.matches_text("where is the VOD"));
        assert!(!c.matches_text("where is the vod"));
    }

    #[test]
    fn unicode_case_and_width_fold() {
        let c = compiled(MatchKind::Contains, "discord");
        assert!(c.matches_text("ＤＩＳＣＯＲＤ?"), "fullwidth letters");
        let c = compiled(MatchKind::Word, "straße");
        assert!(c.matches_text("Welche STRASSE? nein, die Straße"));
        let c = compiled(MatchKind::Word, "café");
        // Composed and decomposed é are the same letter.
        assert!(c.matches_text("le cafe\u{301} est ouvert"));
        let c = compiled(MatchKind::Contains, "ΣΟΦΊΑ");
        assert!(c.matches_text("η σοφία"));
        let c = compiled(MatchKind::Contains, "🎮");
        assert!(c.matches_text("what 🎮 tonight"));
        let c = compiled(MatchKind::Contains, "ゲーム");
        assert!(c.matches_text("何のゲームですか"));
    }

    #[test]
    fn word_boundaries() {
        let c = compiled(MatchKind::Word, "game");
        assert!(c.matches_text("what game is this"));
        assert!(c.matches_text("game?"));
        assert!(c.matches_text("GAME"));
        assert!(c.matches_text("new-game"), "hyphen is a boundary");
        assert!(!c.matches_text("gameplay"));
        assert!(!c.matches_text("endgame"));
        assert!(!c.matches_text("game_over"), "underscore joins words");
        assert!(!c.matches_text("gameé"), "letters beyond ASCII join words too");
        // A later occurrence can still match after an earlier non-word one.
        assert!(c.matches_text("gameplay or game"));
        let c = compiled(MatchKind::Word, "what game");
        assert!(c.matches_text("so, what  game?"));
        assert!(!c.matches_text("somewhat game"));
        // Punctuation at the needle's edge needs no boundary there.
        let c = compiled(MatchKind::Word, "!discord");
        assert!(c.matches_text("type!discord"));
        let c = compiled(MatchKind::Word, "über");
        assert!(c.matches_text("Über alles"));
        assert!(!c.matches_text("Überall"));
    }

    #[test]
    fn starts_with() {
        let c = compiled(MatchKind::StartsWith, "hello");
        assert!(c.matches_text("   Hello chat"));
        assert!(c.matches_text("hellooo"));
        assert!(!c.matches_text("oh hello"));
    }

    #[test]
    fn regex_matching_and_case() {
        let c = compiled(MatchKind::Regex, r"\b(what|which) (game|category)\b");
        assert!(c.matches_text("Which GAME is this"));
        assert!(!c.matches_text("whatever game"));
        let mut r = rule(MatchKind::Regex, "^VOD$");
        r.matcher.case_sensitive = true;
        let c = compile_rule(&r).unwrap();
        assert!(c.matches_text("VOD"));
        assert!(!c.matches_text("vod"));
        // Unicode classes work.
        let c = compiled(MatchKind::Regex, r"^\p{Greek}+$");
        assert!(c.matches_text("σοφία"));
    }

    #[test]
    fn regex_errors_point_at_the_problem() {
        let err = rule(MatchKind::Regex, "(discord").validate().unwrap_err();
        assert!(matches!(err, AutoReplyError::BadPattern { at: 1, .. }), "{err:?}");
        assert_eq!(err.status(), 422);
        assert_eq!(err.code(), "bad_pattern");
        assert!(err.to_string().starts_with("match.text: the pattern has a problem at character 1"));
        let err = rule(MatchKind::Regex, "ab[c").validate().unwrap_err();
        assert!(matches!(err, AutoReplyError::BadPattern { at: 3, .. }), "{err:?}");
        let err = rule(MatchKind::Regex, "dé(?P<x").validate().unwrap_err();
        assert!(matches!(err, AutoReplyError::BadPattern { .. }));
        // Look-around isn't supported by the regex crate: a clear error, not a crash.
        let err = rule(MatchKind::Regex, "(?=x)y").validate().unwrap_err();
        assert!(matches!(err, AutoReplyError::BadPattern { .. }), "{err:?}");
        let err = rule(MatchKind::Regex, "a*").validate().unwrap_err();
        assert_eq!(err, AutoReplyError::MatchesEverything);
        let err = rule(MatchKind::Regex, "x|").validate().unwrap_err();
        assert_eq!(err, AutoReplyError::MatchesEverything);
    }

    #[test]
    fn regex_size_and_nesting_limits() {
        // Repetition that blows up when compiled.
        let err = rule(MatchKind::Regex, r"\w{50}\w{50}\w{50}").validate().unwrap_err();
        assert_eq!(err, AutoReplyError::PatternTooBig, "{err:?}");
        let deep = format!("{}a{}", "(".repeat(30), ")".repeat(30));
        let err = rule(MatchKind::Regex, &deep).validate().unwrap_err();
        assert!(matches!(err, AutoReplyError::BadPattern { .. }), "{err:?}");
    }

    #[test]
    fn hostile_input_is_quick_and_bounded() {
        // The classic catastrophic-backtracking shape stays linear here.
        let c = compiled(MatchKind::Regex, "(a+)+b");
        let started = std::time::Instant::now();
        assert!(!c.matches_text(&"a".repeat(100_000)));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let c = compiled(MatchKind::Word, "aa");
        assert!(!c.matches_text(&"a".repeat(100_000)));
    }

    #[test]
    fn validation_fields_and_statuses() {
        let ok = rule(MatchKind::Contains, "discord");
        assert!(ok.validate().is_ok());
        assert_eq!(AutoReply { name: " ".into(), ..ok.clone() }.validate(), Err(AutoReplyError::Name));
        let mut r = ok.clone();
        r.matcher.text = "  ".into();
        assert_eq!(r.validate(), Err(AutoReplyError::Text));
        r.matcher.text = "x".repeat(201);
        assert_eq!(r.validate(), Err(AutoReplyError::Text));
        assert_eq!(AutoReply { reply: " ".into(), ..ok.clone() }.validate(), Err(AutoReplyError::EmptyReply));
        let long = AutoReply { reply: "é".repeat(501), ..ok.clone() }.validate().unwrap_err();
        assert_eq!(long, AutoReplyError::ReplyTooLong { len: 501 });
        assert_eq!(long.status(), 413);
        assert!(AutoReply { reply: "é".repeat(500), ..ok.clone() }.validate().is_ok());
        let bad_tpl = AutoReply { reply: "{oops".into(), ..ok.clone() }.validate().unwrap_err();
        assert!(matches!(bad_tpl, AutoReplyError::Template(_)));
        assert_eq!(bad_tpl.status(), 400);
        assert_eq!(
            AutoReply { cooldown_s: 86_401, ..ok.clone() }.validate(),
            Err(AutoReplyError::Cooldown("cooldown_s"))
        );
        assert_eq!(
            AutoReply { user_cooldown_s: 90_000, ..ok }.validate(),
            Err(AutoReplyError::Cooldown("user_cooldown_s"))
        );
    }

    #[test]
    fn json_shape_and_defaults() {
        let r: AutoReply = serde_json::from_str(
            r#"{"name":"d","match":{"kind":"word","text":"discord"},"reply":"discord.gg/x","min_role":"subs"}"#,
        )
        .unwrap();
        assert!(r.enabled && r.only_when_live && !r.matcher.case_sensitive);
        assert_eq!((r.cooldown_s, r.user_cooldown_s), (30, 120));
        assert_eq!(r.min_role, Access::Subs);
        assert!(r.platforms.is_empty());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["match"]["kind"], "word");
        assert_eq!(v["min_role"], "subs");
        let roles: AutoReply = serde_json::from_str(
            r#"{"name":"d","match":{"kind":"starts_with","text":"x"},"reply":"y","min_role":"mod"}"#,
        )
        .unwrap();
        assert_eq!(roles.min_role, Access::Mods, "the singular names work too");
    }

    #[test]
    fn first_match_wins_and_conditions_apply() {
        let mut a = rule(MatchKind::Contains, "discord");
        a.id = "a".into();
        a.reply = "A".into();
        let mut b = rule(MatchKind::Contains, "dis");
        b.id = "b".into();
        b.reply = "B".into();
        let (set, bad) = RuleSet::compile(&[a.clone(), b.clone()]);
        assert!(bad.is_empty());
        let v = viewer("v");
        assert_eq!(set.first_match(Platform::Twitch, &v, "discord?", true).unwrap().rule.id, "a");
        assert_eq!(set.first_match(Platform::Twitch, &v, "disco", true).unwrap().rule.id, "b");
        // Off, wrong platform, too low a role, offline: the next rule gets its turn.
        for change in [
            |r: &mut AutoReply| r.enabled = false,
            |r: &mut AutoReply| r.platforms = vec![Platform::Kick],
            |r: &mut AutoReply| r.min_role = Access::Subs,
        ] {
            let mut a2 = a.clone();
            change(&mut a2);
            let (set, _) = RuleSet::compile(&[a2, b.clone()]);
            assert_eq!(set.first_match(Platform::Twitch, &v, "discord", true).unwrap().rule.id, "b");
        }
        assert!(set.first_match(Platform::Twitch, &v, "discord", false).is_none(), "only while live");
        let mut always = a.clone();
        always.only_when_live = false;
        let (set2, _) = RuleSet::compile(&[always]);
        assert!(set2.first_match(Platform::Twitch, &v, "discord", false).is_some());
        let mut sub = v.clone();
        sub.roles = Roles::SUBSCRIBER;
        let mut subs_only = a;
        subs_only.min_role = Access::Subs;
        let (set3, _) = RuleSet::compile(&[subs_only]);
        assert!(set3.first_match(Platform::Twitch, &sub, "discord", true).is_some());
    }

    #[test]
    fn broken_saved_rules_are_left_out() {
        let good = rule(MatchKind::Contains, "x");
        let mut bad = rule(MatchKind::Regex, "(");
        bad.id = "bad".into();
        let (set, errs) = RuleSet::compile(&[bad, good]);
        assert_eq!(set.len(), 1);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, "bad");
    }

    #[test]
    fn cooldowns_global_per_platform_and_per_user() {
        let r = rule(MatchKind::Contains, "discord");
        let (set, _) = RuleSet::compile(std::slice::from_ref(&r));
        let mut cd = Cooldowns::default();
        let tw = Platform::Twitch;
        let run1 = |cd: &mut Cooldowns, who: &str, p: Platform, at: i64| {
            let a = viewer(who);
            run(&set, cd, msg(p, &a, "discord?"), &Vars::new(), Millis(at))
        };
        assert_eq!(run1(&mut cd, "a", tw, 0).unwrap().reply, "hi a on Twitch");
        assert_eq!(run1(&mut cd, "b", tw, 10_000), Err(Skip::Cooldown { rule_id: r.id.clone(), seconds_left: 20 }));
        // Another platform's chat has its own global cooldown.
        assert!(run1(&mut cd, "b", Platform::Kick, 10_000).is_ok());
        assert!(run1(&mut cd, "b", tw, 30_000).is_ok());
        // a's own cooldown (120 s) outlasts the global one.
        assert_eq!(run1(&mut cd, "a", tw, 61_000), Err(Skip::Cooldown { rule_id: r.id.clone(), seconds_left: 59 }));
        assert!(run1(&mut cd, "a", tw, 120_000).is_ok());
        // Mods wait too.
        let mut m = viewer("m");
        m.roles = Roles::MODERATOR;
        assert!(matches!(
            run(&set, &mut cd, msg(tw, &m, "discord"), &Vars::new(), Millis(121_000)),
            Err(Skip::Cooldown { .. })
        ));
        cd.forget(&r.id);
        assert!(run(&set, &mut cd, msg(tw, &m, "discord"), &Vars::new(), Millis(121_000)).is_ok());
    }

    #[test]
    fn zero_cooldown_still_has_a_floor() {
        let mut r = rule(MatchKind::Contains, "x");
        r.cooldown_s = 0;
        r.user_cooldown_s = 0;
        let (set, _) = RuleSet::compile(&[r]);
        let mut cd = Cooldowns::default();
        let p = Platform::Twitch;
        let (a, b) = (viewer("a"), viewer("b"));
        assert!(run(&set, &mut cd, msg(p, &a, "x"), &Vars::new(), Millis(0)).is_ok());
        assert!(run(&set, &mut cd, msg(p, &b, "x"), &Vars::new(), Millis(4_000)).is_err());
        assert!(run(&set, &mut cd, msg(p, &b, "x"), &Vars::new(), Millis(5_000)).is_ok());
    }

    #[test]
    fn replies_render_variables_and_stay_safe() {
        let mut r = rule(MatchKind::Contains, "game");
        r.reply = "/ban {user} — we're on {game} in {channel}".into();
        let c = compile_rule(&r).unwrap();
        let mut extra = Vars::new();
        extra.insert("game".into(), "Hades II".into());
        extra.insert("channel".into(), "streamer".into());
        let mut a = viewer("x");
        a.display_name = "Evil\nName".into();
        assert_eq!(c.render(Platform::Kick, &a, &extra), "ban Evil Name — we're on Hades II in streamer");
    }

    #[test]
    fn try_it_ignores_conditions() {
        let mut r = rule(MatchKind::Contains, "x");
        r.platforms = vec![Platform::Kick];
        r.min_role = Access::Mods;
        let (set, _) = RuleSet::compile(&[r.clone()]);
        assert!(set.first_text_match("x").is_some());
        r.enabled = false;
        let (set, _) = RuleSet::compile(&[r]);
        assert!(set.first_text_match("x").is_none());
    }

    #[test]
    fn send_budget_windows_and_pauses() {
        let mut b = SendBudget::default();
        let tw = Platform::Twitch;
        for i in 0..6 {
            assert!(b.take(tw, Millis(i * 100)), "send {i}");
        }
        assert!(!b.take(tw, Millis(1_000)));
        assert_eq!(b.left(tw, Millis(1_000)), 0);
        assert!(b.take(Platform::Kick, Millis(1_000)), "each platform has its own budget");
        assert!(b.take(tw, Millis(30_000)), "the oldest send left the window");
        let yt = Platform::Youtube;
        assert!(b.take(yt, Millis(0)) && b.take(yt, Millis(1)));
        assert!(!b.take(yt, Millis(59_000)));
        assert!(b.take(yt, Millis(60_000)));
        b.pause(Platform::Kick, 10, Millis(2_000));
        assert!(!b.take(Platform::Kick, Millis(11_999)));
        assert!(b.take(Platform::Kick, Millis(12_000)));
    }

    #[test]
    fn starters_are_valid() {
        for s in starters() {
            s.validate().unwrap();
        }
    }
}
