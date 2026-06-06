//! The chat filter: blocked words, links, caps, symbols, emotes and repeats (R-48).
//!
//! Word filters are "notoriously easy to get around", so before matching, text is folded:
//! compatibility-normalised (𝐛𝐚𝐝, ｂａｄ → bad), lookalikes mapped (Cyrillic а, 0→o,
//! 3→e, @→a…), invisible characters removed, separators between single letters dropped
//! (b.a.d, b a d → bad) and letter runs squeezed (baaaad → bad).

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::event::{Actor, Roles};

/// What to do with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Allow,
    Delete,
    Timeout {
        seconds: u32,
    },
    Ban,
    /// Keep it out of chat overlays and TTS until a mod approves.
    Hold,
}

impl Action {
    fn severity(self) -> u8 {
        match self {
            Action::Allow => 0,
            Action::Hold => 1,
            Action::Delete => 2,
            Action::Timeout { .. } => 3,
            Action::Ban => 4,
        }
    }
}

/// What a filter rule does to a message it catches. Timeouts use
/// [`FilterConfig::timeout_seconds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    #[default]
    Delete,
    Timeout,
    Ban,
    Hold,
}

/// Blocked words and phrases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WordRule {
    pub enabled: bool,
    /// Written plainly; folding is applied to both sides.
    pub list: Vec<String>,
    /// Match lookalike and disguised spellings (b4dw0rd, ｂａｄ, b.a.d). Off = plain,
    /// case-insensitive whole-word matching.
    pub lookalikes: bool,
    pub action: RuleAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRule {
    pub enabled: bool,
    /// Links are fine from these roles…
    pub allow_roles: Roles,
    /// …and to these domains (and their subdomains) from anyone.
    pub allow_domains: Vec<String>,
    pub action: RuleAction,
    /// A first-time chatter's link waits for a mod even when their role allows links:
    /// it is the classic scam opener.
    #[serde(default = "yes")]
    pub hold_first_time: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsRule {
    pub enabled: bool,
    /// Only messages with at least this many letters are checked.
    pub min_length: usize,
    /// Caught at or above this share of capitals (1–100).
    pub max_percent: u8,
    pub action: RuleAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmoteRule {
    pub enabled: bool,
    /// Caught above this many emotes in one message.
    pub max: usize,
    pub action: RuleAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRule {
    pub enabled: bool,
    /// Caught above this share (1–100) of symbols and emoji among non-space characters,
    /// once a message has at least 8 of them.
    pub max_percent: u8,
    pub action: RuleAction,
}

/// "Buy followers", "I do commissions": the bots that work through every small channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolicitationRule {
    pub enabled: bool,
    pub action: RuleAction,
}

/// The moderation filters, in the dashboard's shape (API.md §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterConfig {
    pub words: WordRule,
    pub links: LinkRule,
    pub caps: CapsRule,
    pub emotes: EmoteRule,
    pub symbols: SymbolRule,
    pub solicitation: SolicitationRule,
    /// How long a "timeout" action lasts: 1 second to 14 days.
    pub timeout_seconds: u32,
    /// Roles the filters never touch.
    pub exempt_roles: Roles,
    /// Longer messages are deleted (0 = no limit). Not on the dashboard yet.
    #[serde(default = "default_max_length")]
    pub max_length: usize,
}

fn yes() -> bool {
    true
}

fn default_max_length() -> usize {
    500
}

/// The longest timeout every platform accepts (Twitch's limit).
pub const MAX_TIMEOUT_SECONDS: u32 = 1_209_600;

impl Default for FilterConfig {
    fn default() -> Self {
        FilterConfig {
            words: WordRule { enabled: true, list: Vec::new(), lookalikes: true, action: RuleAction::Delete },
            links: LinkRule {
                enabled: true,
                allow_roles: Roles::BROADCASTER.union(Roles::MODERATOR).union(Roles::VIP).union(Roles::SUBSCRIBER),
                allow_domains: vec!["clips.twitch.tv".into(), "youtube.com".into(), "youtu.be".into()],
                action: RuleAction::Delete,
                hold_first_time: true,
            },
            caps: CapsRule { enabled: true, min_length: 15, max_percent: 80, action: RuleAction::Delete },
            emotes: EmoteRule { enabled: false, max: 15, action: RuleAction::Delete },
            symbols: SymbolRule { enabled: false, max_percent: 50, action: RuleAction::Delete },
            // Delete, not ban: a false match must cost a viewer one message, never their account.
            solicitation: SolicitationRule { enabled: true, action: RuleAction::Delete },
            timeout_seconds: 600,
            exempt_roles: Roles::BROADCASTER.union(Roles::MODERATOR),
            max_length: 500,
        }
    }
}

impl FilterConfig {
    /// Checks limits; the message names the field so the dashboard can point at it.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_TIMEOUT_SECONDS).contains(&self.timeout_seconds) {
            return Err("timeout_seconds: must be between 1 second and 14 days.".into());
        }
        if self.words.list.len() > 2_000 {
            return Err("words.list: up to 2,000 words or phrases.".into());
        }
        if self.words.list.iter().any(|w| w.chars().count() > 100) {
            return Err("words.list: each word or phrase can be up to 100 characters.".into());
        }
        if self.links.allow_domains.len() > 200 || self.links.allow_domains.iter().any(|d| d.chars().count() > 253) {
            return Err("links.allow_domains: up to 200 domains of up to 253 characters.".into());
        }
        if !(1..=100).contains(&self.caps.max_percent) {
            return Err("caps.max_percent: must be between 1 and 100.".into());
        }
        if self.caps.min_length > 500 {
            return Err("caps.min_length: must be 500 or less.".into());
        }
        if !(1..=100).contains(&self.symbols.max_percent) {
            return Err("symbols.max_percent: must be between 1 and 100.".into());
        }
        if !(1..=500).contains(&self.emotes.max) {
            return Err("emotes.max: must be between 1 and 500.".into());
        }
        Ok(())
    }

    fn action(&self, a: RuleAction) -> Action {
        match a {
            RuleAction::Delete => Action::Delete,
            RuleAction::Timeout => Action::Timeout { seconds: self.timeout_seconds.clamp(1, MAX_TIMEOUT_SECONDS) },
            RuleAction::Ban => Action::Ban,
            RuleAction::Hold => Action::Hold,
        }
    }
}

/// The filter's decision, with a reason a mod can read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub action: Action,
    pub reason: String,
}

impl Verdict {
    fn allow() -> Self {
        Verdict { action: Action::Allow, reason: String::new() }
    }
    fn worse(self, other: Verdict) -> Verdict {
        if other.action.severity() > self.action.severity() { other } else { self }
    }
}

/// Phrases the follower-selling and "commission" bots use, folded.
const SOLICITATION: &[&str] = &[
    "buy followers",
    "buy viewers",
    "buy primes",
    "cheap followers",
    "cheap viewers",
    "followers primes and viewers",
    "do you need viewers",
    "need more viewers",
    "promote your stream",
    "promote your channel",
    "grow your stream",
    "grow your channel",
    "streamboo",
    "dogehype",
    "i do commissions",
    "open for commissions",
    "i make emotes",
    "i can make emotes",
    "i do emotes",
    "emotes and overlays",
    "overlays and emotes",
    "remove the space",
];

/// Checks one message. `emotes` is the number of emote fragments.
pub fn check(cfg: &FilterConfig, actor: &Actor, text: &str, emotes: usize) -> Verdict {
    if actor.roles.0 & cfg.exempt_roles.0 != 0 {
        return Verdict::allow();
    }
    let mut v = Verdict::allow();
    let folded = fold(text);
    if cfg.words.enabled {
        let plain = text.to_lowercase();
        for term in &cfg.words.list {
            let hit = if cfg.words.lookalikes {
                let needle = fold(term);
                !needle.is_empty() && contains_term(&folded, &needle)
            } else {
                let needle = term.trim().to_lowercase();
                !needle.is_empty() && contains_plain(&plain, &needle)
            };
            if hit {
                v = v.worse(Verdict {
                    action: cfg.action(cfg.words.action),
                    reason: format!("blocked term \"{term}\""),
                });
            }
        }
    }
    if cfg.solicitation.enabled {
        // Whole words only: "hi do emotes work" must not read as "i do emotes". A phrase
        // spelled out letter by letter ("b u y f o l l o w e r s") folds into one word, so
        // that word may also equal the phrase with its spaces removed.
        let padded = format!(" {folded} ");
        let words: Vec<&str> = folded.split(' ').collect();
        let hit = SOLICITATION.iter().find(|p| {
            let f = fold(p);
            padded.contains(&format!(" {f} ")) || words.contains(&f.replace(' ', "").as_str())
        });
        if let Some(p) = hit {
            v = v.worse(Verdict { action: cfg.action(cfg.solicitation.action), reason: format!("selling (\"{p}\")") });
        }
    }
    if cfg.links.enabled && has_link(text) {
        let role_ok = actor.roles.0 & cfg.links.allow_roles.0 != 0;
        let domain_ok = links(text).iter().all(|d| domain_allowed(d, &cfg.links.allow_domains));
        if actor.roles.contains(Roles::FIRST_TIME) && cfg.links.hold_first_time && !domain_ok {
            v = v.worse(Verdict { action: Action::Hold, reason: "first message has a link".into() });
        }
        // Independent of the hold above so the stronger action wins: a first-time
        // non-subscriber's scam link is deleted, not merely held.
        if !role_ok && !domain_ok {
            v = v.worse(Verdict { action: cfg.action(cfg.links.action), reason: "link not allowed".into() });
        }
    }
    if cfg.max_length > 0 && text.chars().count() > cfg.max_length {
        v = v.worse(Verdict { action: Action::Delete, reason: format!("longer than {} characters", cfg.max_length) });
    }
    if cfg.caps.enabled {
        let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
        if letters.len() >= cfg.caps.min_length.max(1) {
            let caps = letters.iter().filter(|c| c.is_uppercase()).count();
            if caps * 100 >= letters.len() * usize::from(cfg.caps.max_percent.max(1)) {
                v = v.worse(Verdict { action: cfg.action(cfg.caps.action), reason: "too many capitals".into() });
            }
        }
    }
    if cfg.symbols.enabled {
        let visible: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
        let symbols = visible.iter().filter(|c| is_symbol(**c)).count();
        if visible.len() >= 8 && symbols * 100 > visible.len() * usize::from(cfg.symbols.max_percent.max(1)) {
            v = v.worse(Verdict { action: cfg.action(cfg.symbols.action), reason: "too many symbols".into() });
        }
    }
    if cfg.emotes.enabled && emotes > cfg.emotes.max {
        v = v.worse(Verdict {
            action: cfg.action(cfg.emotes.action),
            reason: format!("more than {} emotes", cfg.emotes.max),
        });
    }
    v
}

/// Punctuation, symbols and emoji, but not letters or marks of any script (Devanagari
/// vowel signs are marks, not symbols).
fn is_symbol(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace() && !is_combining(c)
}

fn is_combining(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036f}' | '\u{0900}'..='\u{097f}' | '\u{0980}'..='\u{0dff}' | '\u{200c}' | '\u{200d}' | '\u{fe0f}')
}

/// Case-insensitive whole-word (or whole-phrase) match without folding.
fn contains_plain(text: &str, needle: &str) -> bool {
    text.match_indices(needle).any(|(i, m)| {
        let before = text[..i].chars().next_back();
        let after = text[i + m.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// Folds text for matching (see module docs). Output is lowercase ASCII-ish words
/// separated by single spaces.
pub fn fold(text: &str) -> String {
    let mapped: String =
        text.nfkc().flat_map(char::to_lowercase).filter(|c| !is_invisible(*c)).map(lookalike).collect();
    // Collapse "b.a.d" / "b a d" / "b-a-d": separators between single characters.
    let tokens: Vec<&str> = mapped.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect();
    let mut words: Vec<String> = Vec::new();
    let mut run = String::new();
    for t in tokens {
        if t.chars().count() == 1 {
            run.push_str(t);
        } else {
            if !run.is_empty() {
                words.push(std::mem::take(&mut run));
            }
            words.push(t.to_string());
        }
    }
    if !run.is_empty() {
        words.push(run);
    }
    words.iter().map(|w| squeeze(w)).collect::<Vec<_>>().join(" ")
}

/// Whole-word match for single words, substring match inside words for longer terms so
/// "badword" inside "xbadwordx" is caught but "ass" doesn't fire on "class".
fn contains_term(folded: &str, needle: &str) -> bool {
    if needle.contains(' ') || needle.chars().count() >= 5 {
        let joined = folded.replace(' ', "");
        return folded.contains(needle) || joined.contains(&needle.replace(' ', ""));
    }
    folded.split(' ').any(|w| w == needle)
}

fn squeeze(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut last = None;
    for c in word.chars() {
        if Some(c) != last {
            out.push(c);
        }
        last = Some(c);
    }
    out
}

fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{2060}'..='\u{2064}' | '\u{feff}' | '\u{00ad}' | '\u{034f}' | '\u{180e}')
        || ('\u{0300}'..='\u{036f}').contains(&c) // combining marks (zalgo)
        || ('\u{e0000}'..='\u{e007f}').contains(&c) // tag characters
}

fn lookalike(c: char) -> char {
    match c {
        '0' => 'o',
        '1' | '!' | '|' => 'i',
        '3' => 'e',
        '4' | '@' => 'a',
        '5' | '$' => 's',
        '7' => 't',
        '8' => 'b',
        '9' => 'g',
        // Cyrillic and Greek letters that look Latin
        'а' | 'α' => 'a',
        'е' | 'ε' => 'e',
        'о' | 'ο' => 'o',
        'р' | 'ρ' => 'p',
        'с' => 'c',
        'у' => 'y',
        'х' | 'χ' => 'x',
        'і' | 'ι' => 'i',
        'ј' => 'j',
        'ѕ' => 's',
        'к' | 'κ' => 'k',
        'м' => 'm',
        'н' | 'η' => 'n',
        'т' | 'τ' => 't',
        'в' | 'β' => 'b',
        'ν' => 'v',
        'ɡ' => 'g',
        other => other,
    }
}

fn has_link(text: &str) -> bool {
    !links(text).is_empty()
}

/// Domains of anything that looks like a link, including "example . com" and "hxxp" tricks.
pub(crate) fn links(text: &str) -> Vec<String> {
    let lower = text.to_lowercase().replace(" . ", ".").replace("[.]", ".").replace("(.)", ".").replace(" dot ", ".");
    let mut out = Vec::new();
    for raw in lower.split_whitespace() {
        let w = raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '.' && c != ':');
        let w = w
            .strip_prefix("https://")
            .or_else(|| w.strip_prefix("http://"))
            .or_else(|| w.strip_prefix("hxxps://"))
            .or_else(|| w.strip_prefix("hxxp://"))
            .unwrap_or(w);
        let host = w.split(['/', '?', '#']).next().unwrap_or("");
        let host = host.strip_prefix("www.").unwrap_or(host);
        let parts: Vec<&str> = host.split('.').collect();
        let tld_ok = parts.last().is_some_and(|t| is_tld(t));
        if parts.len() >= 2 && tld_ok && parts.iter().all(|p| !p.is_empty()) && !host.contains("..") {
            out.push(host.to_string());
        }
    }
    out
}

/// A real top-level domain that spammers and streamers actually use. Accepting any letters
/// made "Mr.Bean" and "great.stream" look like links.
fn is_tld(t: &str) -> bool {
    const GENERIC: [&str; 38] = [
        "com", "net", "org", "dev", "app", "xyz", "info", "biz", "live", "link", "site", "online", "store", "shop",
        "club", "top", "vip", "fun", "pro", "art", "blog", "chat", "click", "cloud", "games", "icu", "lol", "monster",
        "rest", "sbs", "space", "tech", "today", "website", "win", "zip", "gay", "ink",
    ];
    // Country codes that show up in chat links; "wp" or "gg.ez" style banter isn't a link.
    const COUNTRY: [&str; 120] = [
        "ac", "ae", "ag", "ai", "al", "am", "ar", "at", "au", "az", "ba", "bd", "be", "bg", "bh", "br", "by", "bz",
        "ca", "cc", "cf", "ch", "cl", "cn", "co", "cx", "cz", "de", "dk", "ee", "eg", "es", "fi", "fm", "fo", "fr",
        "ga", "ge", "gg", "gl", "gq", "gr", "gy", "hk", "hr", "hu", "id", "ie", "il", "im", "in", "io", "ir", "is",
        "it", "jo", "jp", "ke", "kr", "kw", "kz", "la", "lb", "lc", "li", "lk", "lt", "lu", "lv", "ly", "ma", "md",
        "me", "mk", "ml", "mn", "ms", "mu", "mx", "my", "ng", "nl", "no", "np", "nu", "nz", "om", "pe", "ph", "pk",
        "pl", "pt", "pw", "qa", "ro", "rs", "ru", "sa", "sc", "se", "sg", "sh", "si", "sk", "so", "su", "th", "tk",
        "to", "tr", "tv", "tw", "ua", "uk", "us", "uz", "vg", "vn", "ws", "za",
    ];
    GENERIC.contains(&t) || COUNTRY.contains(&t)
}

fn domain_allowed(domain: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|a| {
        let a = a.trim().trim_start_matches("www.").to_lowercase();
        !a.is_empty() && (domain == a || domain.ends_with(&format!(".{a}")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Platform;

    fn viewer() -> Actor {
        Actor::named(Platform::Twitch, "v")
    }

    fn cfg_with(term: &str) -> FilterConfig {
        let mut c = FilterConfig { timeout_seconds: 600, ..FilterConfig::default() };
        c.words.list = vec![term.into()];
        c.words.action = RuleAction::Timeout;
        c
    }

    #[test]
    fn catches_common_evasions() {
        let cfg = cfg_with("badword");
        for evasion in [
            "badword",
            "BADWORD",
            "b a d w o r d",
            "b.a.d.w.o.r.d",
            "b4dw0rd",
            "baaadwooord",
            "bad\u{200b}word",
            "ｂａｄｗｏｒｄ",
            "𝐛𝐚𝐝𝐰𝐨𝐫𝐝",
            "bаdwоrd", // Cyrillic а and о
            "b̷a̷d̷w̷o̷r̷d̷",
            "xxbadwordxx",
        ] {
            assert_eq!(check(&cfg, &viewer(), evasion, 0).action, Action::Timeout { seconds: 600 }, "{evasion:?}");
        }
    }

    #[test]
    fn lookalikes_off_matches_plain_words_only() {
        let mut cfg = cfg_with("badword");
        cfg.words.lookalikes = false;
        assert_eq!(check(&cfg, &viewer(), "a BadWord here", 0).action, Action::Timeout { seconds: 600 });
        assert_eq!(check(&cfg, &viewer(), "b4dw0rd", 0).action, Action::Allow);
        assert_eq!(check(&cfg, &viewer(), "xxbadwordxx", 0).action, Action::Allow, "whole words");
        cfg.words.enabled = false;
        assert_eq!(check(&cfg, &viewer(), "badword", 0).action, Action::Allow);
    }

    #[test]
    fn short_terms_match_whole_words_only() {
        let cfg = cfg_with("ass");
        assert_eq!(check(&cfg, &viewer(), "first class assistant", 0).action, Action::Allow);
        assert_ne!(check(&cfg, &viewer(), "you a s s", 0).action, Action::Allow);
    }

    #[test]
    fn exempt_roles_are_never_filtered() {
        let cfg = cfg_with("badword");
        let mut m = viewer();
        m.roles = Roles::MODERATOR;
        assert_eq!(check(&cfg, &m, "badword", 0).action, Action::Allow);
    }

    #[test]
    fn follower_sellers_and_commission_bots() {
        let cfg = FilterConfig::default();
        for spam in [
            "Wanna become famous? Buy followers, primes and viewers on example . com",
            "Hey! I'm a graphic designer, I make emotes and overlays",
            "Hey! I'm a graphic designer and i do commissions",
            "b u y  f o l l o w e r s",
            "Do you need viewers?",
            "streamboo .com (remove the space)",
        ] {
            assert_eq!(check(&cfg, &viewer(), spam, 0).action, Action::Delete, "{spam:?}");
        }
        for fine in [
            "how do you get followers",
            "nice emotes!",
            "I need more coffee",
            "the designer of this game",
            "hi do emotes work here",
            "hi make emotes for the sub tiers?",
            "you are the best viewers",
            "I want to become famous",
            "I'm a graphic designer too",
        ] {
            assert_eq!(check(&cfg, &viewer(), fine, 0).action, Action::Allow, "{fine:?}");
        }
    }

    #[test]
    fn links_by_role_domain_and_first_time() {
        let mut cfg = FilterConfig::default();
        cfg.solicitation.enabled = false;
        assert_eq!(check(&cfg, &viewer(), "check cheap-viewz.com", 0).action, Action::Delete);
        assert_eq!(check(&cfg, &viewer(), "cheap-viewz . com", 0).action, Action::Delete);
        assert_eq!(check(&cfg, &viewer(), "hxxps://scam.ru/x", 0).action, Action::Delete);
        assert_eq!(check(&cfg, &viewer(), "watch https://www.youtube.com/watch?v=1", 0).action, Action::Allow);
        assert_eq!(check(&cfg, &viewer(), "clip https://clips.twitch.tv/abc", 0).action, Action::Allow);
        assert_eq!(check(&cfg, &viewer(), "evil-youtube.com.attacker.net", 0).action, Action::Delete);
        let mut sub = viewer();
        sub.roles = Roles::SUBSCRIBER;
        assert_eq!(check(&cfg, &sub, "my site.dev", 0).action, Action::Allow);
        let mut first = viewer();
        first.roles = Roles::FIRST_TIME;
        assert_eq!(
            check(&cfg, &first, "hey check bestviewers.net", 0).action,
            Action::Delete,
            "a first-time non-sub's link is deleted, not just held"
        );
        first.roles = Roles::FIRST_TIME.union(Roles::SUBSCRIBER);
        assert_eq!(
            check(&cfg, &first, "hey check bestviewers.net", 0).action,
            Action::Hold,
            "a first-time sub's link waits for a mod"
        );
        assert_eq!(check(&cfg, &viewer(), "version 1.2 is out... ok.", 0).action, Action::Allow, "not a link");
        for normal in ["Mr.Bean is funny", "that was great.stream on", "gg.wp", "e.g. this", "U.S.A. vs IN"] {
            assert_eq!(check(&cfg, &viewer(), normal, 0).action, Action::Allow, "{normal:?} isn't a link");
        }
        for spam in ["cheap.xyz", "bit.ly/abc", "free-followers.gg", "discord.gg/scam"] {
            assert_eq!(check(&cfg, &viewer(), spam, 0).action, Action::Delete, "{spam:?} is a link");
        }
        cfg.links.action = RuleAction::Timeout;
        assert_eq!(check(&cfg, &viewer(), "cheap.xyz", 0).action, Action::Timeout { seconds: 600 });
    }

    #[test]
    fn spam_rules() {
        let mut cfg = FilterConfig::default();
        cfg.symbols.enabled = true;
        cfg.emotes.enabled = true;
        cfg.emotes.max = 3;
        assert_eq!(check(&cfg, &viewer(), "THIS IS ALL CAPS SHOUTING", 0).action, Action::Delete);
        assert_eq!(check(&cfg, &viewer(), "OK", 0).action, Action::Allow, "short caps are fine");
        assert_eq!(check(&cfg, &viewer(), "lol !!!!!!!!!!!!", 0).action, Action::Delete);
        assert_eq!(check(&cfg, &viewer(), "wow!!", 0).action, Action::Allow, "a little punctuation is fine");
        assert_eq!(check(&cfg, &viewer(), "hi", 4).action, Action::Delete);
        assert_eq!(check(&cfg, &viewer(), &"a ".repeat(400), 0).action, Action::Delete);
        assert_eq!(
            check(&cfg, &viewer(), "नमस्ते दोस्तों, कैसे हो आप सब", 0).action,
            Action::Allow,
            "Devanagari isn't 'symbols'"
        );
        cfg.caps.enabled = false;
        assert_eq!(check(&cfg, &viewer(), "THIS IS ALL CAPS SHOUTING", 0).action, Action::Allow);
    }

    #[test]
    fn worst_action_wins() {
        let mut cfg = cfg_with("badword");
        cfg.links.action = RuleAction::Ban;
        let v = check(&cfg, &viewer(), "badword cheap.xyz", 0);
        assert_eq!(v.action, Action::Ban);
        assert!(v.reason.contains("link"));
    }

    #[test]
    fn limits_name_the_field() {
        assert!(FilterConfig::default().validate().is_ok());
        let bad = [
            FilterConfig { timeout_seconds: 0, ..FilterConfig::default() },
            FilterConfig { timeout_seconds: MAX_TIMEOUT_SECONDS + 1, ..FilterConfig::default() },
        ];
        for b in bad {
            assert!(b.validate().unwrap_err().starts_with("timeout_seconds"));
        }
        let mut c = FilterConfig::default();
        c.caps.max_percent = 0;
        assert!(c.validate().unwrap_err().starts_with("caps.max_percent"));
        let mut c = FilterConfig::default();
        c.words.list = vec!["x".repeat(101)];
        assert!(c.validate().unwrap_err().starts_with("words.list"));
    }

    #[test]
    fn the_dashboard_shape_round_trips() {
        let json = r#"{"words":{"enabled":true,"list":["a"],"lookalikes":true,"action":"delete"},
            "links":{"enabled":true,"allow_roles":14,"allow_domains":["youtu.be"],"action":"delete"},
            "caps":{"enabled":true,"min_length":12,"max_percent":70,"action":"delete"},
            "emotes":{"enabled":false,"max":15,"action":"delete"},
            "symbols":{"enabled":false,"max_percent":50,"action":"timeout"},
            "solicitation":{"enabled":true,"action":"ban"},"timeout_seconds":600,"exempt_roles":3}"#;
        let c: FilterConfig = serde_json::from_str(json).unwrap();
        assert!(c.links.hold_first_time, "fields the dashboard doesn't send keep their defaults");
        assert_eq!(c.max_length, 500);
        assert_eq!(c.symbols.action, RuleAction::Timeout);
        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(back["links"]["allow_roles"], 14);
    }

    #[test]
    fn empty_terms_never_match() {
        let cfg = cfg_with("   ");
        assert_eq!(check(&cfg, &viewer(), "anything", 0).action, Action::Allow);
    }

    #[test]
    fn huge_hostile_input_is_fine() {
        let cfg = cfg_with("badword");
        let zalgo = "z\u{0301}\u{0302}\u{0303}".repeat(20_000);
        let _ = check(&cfg, &viewer(), &zalgo, 0);
    }
}
