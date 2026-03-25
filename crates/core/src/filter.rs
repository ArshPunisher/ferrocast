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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedTerm {
    /// Written plainly; folding is applied to both sides.
    pub term: String,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterConfig {
    pub enabled: bool,
    pub terms: Vec<BlockedTerm>,
    /// Links are allowed for these roles and these domains only.
    pub links: LinkRule,
    /// Above this share of capital letters (0–100), once a message has at least
    /// `caps_min_letters` letters. 0 = off.
    pub caps_percent: u8,
    pub caps_min_letters: usize,
    /// More than this many symbols/emoji in a row. 0 = off.
    pub max_symbols: usize,
    /// More than this many emotes in one message. 0 = off.
    pub max_emotes: usize,
    /// A character repeated more than this many times in a row. 0 = off.
    pub max_repeat: usize,
    pub max_length: usize,
    /// Action for caps/symbols/emotes/repeat/length problems.
    pub spam_action: Action,
    /// Roles the filter never touches.
    pub exempt: Roles,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRule {
    pub enabled: bool,
    pub allowed_roles: Roles,
    pub allowed_domains: Vec<String>,
    /// First-time chatters posting a link is the classic scam opener.
    pub hold_first_time: bool,
    pub action: Action,
}

impl Default for FilterConfig {
    fn default() -> Self {
        FilterConfig {
            enabled: true,
            terms: Vec::new(),
            links: LinkRule {
                enabled: true,
                allowed_roles: Roles::BROADCASTER
                    .union(Roles::MODERATOR)
                    .union(Roles::VIP)
                    .union(Roles::SUBSCRIBER),
                allowed_domains: vec![
                    "clips.twitch.tv".into(),
                    "youtube.com".into(),
                    "youtu.be".into(),
                ],
                hold_first_time: true,
                action: Action::Delete,
            },
            caps_percent: 80,
            caps_min_letters: 15,
            max_symbols: 0,
            max_emotes: 0,
            max_repeat: 0,
            max_length: 500,
            spam_action: Action::Delete,
            exempt: Roles::BROADCASTER.union(Roles::MODERATOR),
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
        Verdict {
            action: Action::Allow,
            reason: String::new(),
        }
    }
    fn worse(self, other: Verdict) -> Verdict {
        if other.action.severity() > self.action.severity() {
            other
        } else {
            self
        }
    }
}

/// Checks one message. `emotes` is the number of emote fragments.
pub fn check(cfg: &FilterConfig, actor: &Actor, text: &str, emotes: usize) -> Verdict {
    if !cfg.enabled || actor.roles.0 & cfg.exempt.0 != 0 {
        return Verdict::allow();
    }
    let mut v = Verdict::allow();
    let folded = fold(text);
    for t in &cfg.terms {
        let needle = fold(&t.term);
        if !needle.is_empty() && contains_term(&folded, &needle) {
            v = v.worse(Verdict {
                action: t.action,
                reason: format!("blocked term \"{}\"", t.term),
            });
        }
    }
    if cfg.links.enabled && has_link(text) {
        let role_ok = actor.roles.0 & cfg.links.allowed_roles.0 != 0;
        let domain_ok = links(text)
            .iter()
            .all(|d| domain_allowed(d, &cfg.links.allowed_domains));
        if actor.roles.contains(Roles::FIRST_TIME) && cfg.links.hold_first_time && !domain_ok {
            v = v.worse(Verdict {
                action: Action::Hold,
                reason: "first message has a link".into(),
            });
        }
        // Independent of the hold above so the stronger action wins: a first-time
        // non-subscriber's scam link is deleted, not merely held.
        if !role_ok && !domain_ok {
            v = v.worse(Verdict {
                action: cfg.links.action,
                reason: "link not allowed".into(),
            });
        }
    }
    let spam = |reason: String| Verdict {
        action: cfg.spam_action,
        reason,
    };
    if cfg.max_length > 0 && text.chars().count() > cfg.max_length {
        v = v.worse(spam(format!("longer than {} characters", cfg.max_length)));
    }
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    if cfg.caps_percent > 0 && letters.len() >= cfg.caps_min_letters.max(1) {
        let caps = letters.iter().filter(|c| c.is_uppercase()).count();
        if caps * 100 >= letters.len() * usize::from(cfg.caps_percent) {
            v = v.worse(spam("too many capitals".into()));
        }
    }
    if cfg.max_symbols > 0
        && longest_run(text, |c| !c.is_alphanumeric() && !c.is_whitespace()) > cfg.max_symbols
    {
        v = v.worse(spam("too many symbols".into()));
    }
    if cfg.max_emotes > 0 && emotes > cfg.max_emotes {
        v = v.worse(spam(format!("more than {} emotes", cfg.max_emotes)));
    }
    if cfg.max_repeat > 0 && longest_same_char(text) > cfg.max_repeat {
        v = v.worse(spam("repeated characters".into()));
    }
    v
}

/// Folds text for matching (see module docs). Output is lowercase ASCII-ish words
/// separated by single spaces.
pub fn fold(text: &str) -> String {
    let mapped: String = text
        .nfkc()
        .flat_map(char::to_lowercase)
        .filter(|c| !is_invisible(*c))
        .map(lookalike)
        .collect();
    // Collapse "b.a.d" / "b a d" / "b-a-d": separators between single characters.
    let tokens: Vec<&str> = mapped
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
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
    words
        .iter()
        .map(|w| squeeze(w))
        .collect::<Vec<_>>()
        .join(" ")
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
    let lower = text
        .to_lowercase()
        .replace(" . ", ".")
        .replace("[.]", ".")
        .replace("(.)", ".")
        .replace(" dot ", ".");
    let mut out = Vec::new();
    for raw in lower.split_whitespace() {
        let w =
            raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '.' && c != ':');
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
        if parts.len() >= 2 && tld_ok && parts.iter().all(|p| !p.is_empty()) && !host.contains("..")
        {
            out.push(host.to_string());
        }
    }
    out
}

/// A real top-level domain that spammers and streamers actually use. Accepting any letters
/// made "Mr.Bean" and "great.stream" look like links.
fn is_tld(t: &str) -> bool {
    const GENERIC: [&str; 38] = [
        "com", "net", "org", "dev", "app", "xyz", "info", "biz", "live", "link", "site", "online",
        "store", "shop", "club", "top", "vip", "fun", "pro", "art", "blog", "chat", "click",
        "cloud", "games", "icu", "lol", "monster", "rest", "sbs", "space", "tech", "today",
        "website", "win", "zip", "gay", "ink",
    ];
    // Country codes that show up in chat links; "wp" or "gg.ez" style banter isn't a link.
    const COUNTRY: [&str; 120] = [
        "ac", "ae", "ag", "ai", "al", "am", "ar", "at", "au", "az", "ba", "bd", "be", "bg", "bh",
        "br", "by", "bz", "ca", "cc", "cf", "ch", "cl", "cn", "co", "cx", "cz", "de", "dk", "ee",
        "eg", "es", "fi", "fm", "fo", "fr", "ga", "ge", "gg", "gl", "gq", "gr", "gy", "hk", "hr",
        "hu", "id", "ie", "il", "im", "in", "io", "ir", "is", "it", "jo", "jp", "ke", "kr", "kw",
        "kz", "la", "lb", "lc", "li", "lk", "lt", "lu", "lv", "ly", "ma", "md", "me", "mk", "ml",
        "mn", "ms", "mu", "mx", "my", "ng", "nl", "no", "np", "nu", "nz", "om", "pe", "ph", "pk",
        "pl", "pt", "pw", "qa", "ro", "rs", "ru", "sa", "sc", "se", "sg", "sh", "si", "sk", "so",
        "su", "th", "tk", "to", "tr", "tv", "tw", "ua", "uk", "us", "uz", "vg", "vn", "ws", "za",
    ];
    GENERIC.contains(&t) || COUNTRY.contains(&t)
}

fn domain_allowed(domain: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|a| {
        let a = a.trim().trim_start_matches("www.").to_lowercase();
        !a.is_empty() && (domain == a || domain.ends_with(&format!(".{a}")))
    })
}

fn longest_run(text: &str, pred: impl Fn(char) -> bool) -> usize {
    let (mut best, mut cur) = (0, 0);
    for c in text.chars() {
        if pred(c) {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

fn longest_same_char(text: &str) -> usize {
    let (mut best, mut cur, mut last) = (0, 0, None);
    for c in text.chars() {
        cur = if Some(c) == last { cur + 1 } else { 1 };
        last = Some(c);
        best = best.max(cur);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Platform;

    fn viewer() -> Actor {
        Actor::named(Platform::Twitch, "v")
    }

    fn cfg_with(term: &str) -> FilterConfig {
        FilterConfig {
            terms: vec![BlockedTerm {
                term: term.into(),
                action: Action::Timeout { seconds: 600 },
            }],
            ..FilterConfig::default()
        }
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
            assert_eq!(
                check(&cfg, &viewer(), evasion, 0).action,
                Action::Timeout { seconds: 600 },
                "{evasion:?}"
            );
        }
    }

    #[test]
    fn short_terms_match_whole_words_only() {
        let cfg = cfg_with("ass");
        assert_eq!(
            check(&cfg, &viewer(), "first class assistant", 0).action,
            Action::Allow
        );
        assert_ne!(check(&cfg, &viewer(), "you a s s", 0).action, Action::Allow);
    }

    #[test]
    fn mods_and_broadcaster_are_exempt() {
        let cfg = cfg_with("badword");
        let mut m = viewer();
        m.roles = Roles::MODERATOR;
        assert_eq!(check(&cfg, &m, "badword", 0).action, Action::Allow);
    }

    #[test]
    fn links_by_role_domain_and_first_time() {
        let cfg = FilterConfig::default();
        assert_eq!(
            check(&cfg, &viewer(), "buy followers at cheap-viewz.com", 0).action,
            Action::Delete
        );
        assert_eq!(
            check(&cfg, &viewer(), "cheap-viewz . com", 0).action,
            Action::Delete
        );
        assert_eq!(
            check(&cfg, &viewer(), "hxxps://scam.ru/x", 0).action,
            Action::Delete
        );
        assert_eq!(
            check(
                &cfg,
                &viewer(),
                "watch https://www.youtube.com/watch?v=1",
                0
            )
            .action,
            Action::Allow
        );
        assert_eq!(
            check(&cfg, &viewer(), "clip https://clips.twitch.tv/abc", 0).action,
            Action::Allow
        );
        assert_eq!(
            check(&cfg, &viewer(), "evil-youtube.com.attacker.net", 0).action,
            Action::Delete
        );
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
        assert_eq!(
            check(&cfg, &viewer(), "version 1.2 is out... ok.", 0).action,
            Action::Allow,
            "not a link"
        );
        for normal in [
            "Mr.Bean is funny",
            "that was great.stream on",
            "gg.wp",
            "e.g. this",
            "U.S.A. vs IN",
        ] {
            assert_eq!(
                check(&cfg, &viewer(), normal, 0).action,
                Action::Allow,
                "{normal:?} isn't a link"
            );
        }
        for spam in [
            "cheap.xyz",
            "bit.ly/abc",
            "free-followers.gg",
            "discord.gg/scam",
        ] {
            assert_eq!(
                check(&cfg, &viewer(), spam, 0).action,
                Action::Delete,
                "{spam:?} is a link"
            );
        }
    }

    #[test]
    fn spam_rules() {
        let cfg = FilterConfig {
            max_symbols: 5,
            max_emotes: 3,
            max_repeat: 10,
            ..FilterConfig::default()
        };
        assert_eq!(
            check(&cfg, &viewer(), "THIS IS ALL CAPS SHOUTING", 0).action,
            Action::Delete
        );
        assert_eq!(
            check(&cfg, &viewer(), "OK", 0).action,
            Action::Allow,
            "short caps are fine"
        );
        assert_eq!(
            check(&cfg, &viewer(), "lol !!!!!!!!", 0).action,
            Action::Delete
        );
        assert_eq!(check(&cfg, &viewer(), "hi", 4).action, Action::Delete);
        assert_eq!(
            check(&cfg, &viewer(), "nooooooooooooooo", 0).action,
            Action::Delete
        );
        assert_eq!(
            check(&cfg, &viewer(), &"a ".repeat(400), 0).action,
            Action::Delete
        );
        assert_eq!(
            check(&cfg, &viewer(), "नमस्ते दोस्तों, कैसे हो", 0).action,
            Action::Allow,
            "Devanagari isn't 'symbols'"
        );
    }

    #[test]
    fn worst_action_wins_and_disabled_allows() {
        let mut cfg = cfg_with("badword");
        cfg.terms.push(BlockedTerm {
            term: "nuke".into(),
            action: Action::Ban,
        });
        let v = check(&cfg, &viewer(), "nuke badword", 0);
        assert_eq!(v.action, Action::Ban);
        assert!(v.reason.contains("nuke"));
        cfg.enabled = false;
        assert_eq!(check(&cfg, &viewer(), "nuke", 0).action, Action::Allow);
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
