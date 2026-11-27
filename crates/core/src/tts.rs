//! What text-to-speech may read, and when it must wait for approval (R-48).
//!
//! TTS is off by default. When on, it reads only what passes these rules, never numbers
//! that look like phone numbers or addresses, never links, and it can hold everything for a
//! mod to approve. Offensive TTS has cost streamers their channels; this is the guard.

use serde::{Deserialize, Serialize};

use crate::event::{Content, Event, EventKind, Fragment, Origin, Roles};
use crate::filter::{self, FilterConfig};
use crate::money::Money;

/// Who in live chat can trigger Chat TTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChatTtsMode {
    Everyone,
    #[default]
    SubsMods,
    Command,
}

/// Settings for reading live chat messages aloud.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatTtsConfig {
    pub enabled: bool,
    pub mode: ChatTtsMode,
    pub command: String,
    pub cooldown_s: u32,
    pub global_cooldown_s: u32,
    pub queue_limit: usize,
    pub max_chars: usize,
    pub skip_links: bool,
    pub skip_emote_spam: bool,
}

impl Default for ChatTtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: ChatTtsMode::SubsMods,
            command: "!tts".into(),
            cooldown_s: 15,
            global_cooldown_s: 3,
            queue_limit: 5,
            max_chars: 160,
            skip_links: true,
            skip_emote_spam: true,
        }
    }
}

impl ChatTtsConfig {
    /// Checks limits; error messages name the field so the dashboard can highlight it.
    pub fn validate(&self) -> Result<(), String> {
        let cmd = self.command.trim();
        let cmd_ok = cmd.starts_with('!')
            && (2..=20).contains(&cmd.len())
            && cmd[1..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !cmd_ok {
            return Err("chat.command: start with ! followed by 1 to 19 letters, numbers, - or _.".into());
        }
        if self.cooldown_s > 300 {
            return Err("chat.cooldown_s: per-person cooldown must be 0 to 300 seconds.".into());
        }
        if self.global_cooldown_s > 60 {
            return Err("chat.global_cooldown_s: gap between messages must be 0 to 60 seconds.".into());
        }
        if !(1..=20).contains(&self.queue_limit) {
            return Err("chat.queue_limit: queue cap must be between 1 and 20 messages.".into());
        }
        if !(10..=300).contains(&self.max_chars) {
            return Err("chat.max_chars: chat TTS can read 10 to 300 characters.".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtsPolicy {
    pub enabled: bool,
    pub voice: Option<String>,
    /// Minimum paid amounts that get read, per currency (minor units). A currency not
    /// listed is never read: no silent conversions.
    pub min_amounts: Vec<Money>,
    /// Minimum bits/KICKs/jewels.
    pub min_platform_value: u64,
    pub max_chars: usize,
    pub read_names: bool,
    /// Everything waits in the approval list until a mod presses approve.
    pub hold_for_approval: bool,
    /// Optional settings for reading live chat messages aloud.
    #[serde(default)]
    pub chat: ChatTtsConfig,
}

impl Default for TtsPolicy {
    fn default() -> Self {
        TtsPolicy {
            enabled: false,
            voice: None,
            min_amounts: vec![Money::new(10_000, "INR"), Money::new(200, "USD"), Money::new(200, "EUR")],
            min_platform_value: 200,
            max_chars: 200,
            read_names: true,
            hold_for_approval: false,
            chat: ChatTtsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Decision {
    Skip { reason: String },
    Hold { text: String },
    Speak { text: String },
}

/// Decides what to say for `event`'s message.
pub fn decide(policy: &TtsPolicy, filter_cfg: &FilterConfig, event: &Event) -> Decision {
    let skip = |r: &str| Decision::Skip { reason: r.into() };
    if !policy.enabled {
        return skip("TTS is off");
    }
    let (actor, message, paid_ok) = match &event.kind {
        EventKind::Tip { actor, money, message, .. } => (actor, message.clone(), meets(&policy.min_amounts, money)),
        EventKind::PaidMessage { actor, money, content, .. } => {
            (actor, content.as_ref().map(|c| c.text.clone()), meets(&policy.min_amounts, money))
        }
        EventKind::PlatformGift { actor, value, message } => {
            (actor, message.as_ref().map(|c| c.text.clone()), value.amount >= policy.min_platform_value)
        }
        EventKind::RewardRedemption { actor, input, .. } => (actor, input.clone(), true),
        _ => return skip("this event type isn't read aloud"),
    };
    if !paid_ok {
        return skip("below the minimum amount");
    }
    let Some(message) = message.filter(|m| !m.trim().is_empty()) else {
        return skip("no message");
    };
    let verdict = filter::check(filter_cfg, actor, &message, 0);
    if verdict.action != filter::Action::Allow {
        return Decision::Hold { text: clean(&message, policy.max_chars) };
    }
    let body = clean(&message, policy.max_chars);
    if body.is_empty() {
        return skip("nothing readable left");
    }
    let text = if policy.read_names && !actor.anonymous {
        format!("{} says: {}", clean(&actor.display_name, 40), body)
    } else {
        body
    };
    if policy.hold_for_approval { Decision::Hold { text } } else { Decision::Speak { text } }
}

/// Decides whether a moderated, visible live chat message should be read aloud.
///
/// Enforces live-origin and non-shared-chat guards, mode/role eligibility, `!command`
/// prefix rules, link detection, custom blocked words (without role exemptions), built-in
/// severe-slur hash guard, emote/repetition spam checks, and PII/length cleaning.
pub fn decide_chat(policy: &TtsPolicy, filter_cfg: &FilterConfig, event: &Event) -> Decision {
    let skip = |r: &str| Decision::Skip { reason: r.into() };
    if !policy.enabled || !policy.chat.enabled {
        return skip("chat TTS is off");
    }
    if event.origin != Origin::Live {
        return skip("only live chat is read aloud");
    }
    if event.channel.is_some() {
        return skip("shared chat messages aren't read aloud");
    }
    let EventKind::ChatMessage { actor, content, .. } = &event.kind else {
        return skip("not a chat message");
    };
    if actor.roles.contains(Roles::BOT) {
        return skip("bot messages aren't read aloud");
    }
    let sub_mod_roles = Roles::SUBSCRIBER
        .union(Roles::MEMBER)
        .union(Roles::FOUNDER)
        .union(Roles::VIP)
        .union(Roles::MODERATOR)
        .union(Roles::BROADCASTER);
    if policy.chat.mode == ChatTtsMode::SubsMods && (actor.roles.0 & sub_mod_roles.0) == 0 {
        return skip("chat TTS is limited to subscribers and moderators");
    }

    let (extracted, emote_count, has_link_fragment) = extract_chat_content(content);
    let candidate = match policy.chat.mode {
        ChatTtsMode::Everyone | ChatTtsMode::SubsMods => {
            if content.text.trim_start().starts_with('!') || extracted.trim_start().starts_with('!') {
                return skip("bot commands aren't read aloud");
            }
            extracted
        }
        ChatTtsMode::Command => {
            if strip_command(&content.text, &policy.chat.command).is_none() {
                return skip("missing TTS command");
            }
            match strip_command(&extracted, &policy.chat.command) {
                Some(rest) => rest.to_string(),
                None => return skip("missing TTS command"),
            }
        }
    };

    if policy.chat.skip_links && (has_link_fragment || has_any_link(&content.text) || has_any_link(&candidate)) {
        return skip("links aren't read aloud");
    }

    if contains_severe_slur(&content.text)
        || contains_severe_slur(&candidate)
        || contains_severe_slur(&actor.display_name)
    {
        return skip("blocked content");
    }

    if filter::blocked_word(filter_cfg, &content.text).is_some()
        || filter::blocked_word(filter_cfg, &candidate).is_some()
        || filter::blocked_word(filter_cfg, &actor.display_name).is_some()
    {
        return skip("blocked word");
    }

    if policy.chat.skip_emote_spam && is_emote_or_repeat_spam(&candidate, emote_count) {
        return skip("emote or repeat spam");
    }

    let body = clean(&candidate, policy.chat.max_chars);
    if body.is_empty() {
        return skip("nothing readable left");
    }

    let text = if policy.read_names && !actor.anonymous {
        let name = clean(&actor.display_name, 40);
        if name.is_empty() { body } else { format!("{name} says: {body}") }
    } else {
        body
    };
    if policy.hold_for_approval { Decision::Hold { text } } else { Decision::Speak { text } }
}

fn extract_chat_content(content: &Content) -> (String, usize, bool) {
    if content.fragments.is_empty() {
        return (content.text.clone(), 0, false);
    }
    let mut out = String::with_capacity(content.text.len());
    let mut emotes = 0usize;
    let mut has_link = false;
    let push_sep = |buf: &mut String| {
        if !buf.is_empty() && !buf.ends_with(char::is_whitespace) {
            buf.push(' ');
        }
    };
    for frag in &content.fragments {
        match frag {
            Fragment::Text { text } => out.push_str(text),
            Fragment::Mention { login } => {
                let name = login.trim_start_matches('@').trim();
                if !name.is_empty() {
                    push_sep(&mut out);
                    out.push_str(name);
                }
            }
            Fragment::Emote { .. } => {
                emotes += 1;
                push_sep(&mut out);
            }
            Fragment::Link { .. } => {
                has_link = true;
                push_sep(&mut out);
            }
        }
    }
    (out, emotes, has_link)
}

fn strip_command<'a>(text: &'a str, command: &str) -> Option<&'a str> {
    let trimmed = text.trim_start();
    let cmd = command.trim();
    if cmd.is_empty() || trimmed.len() < cmd.len() {
        return None;
    }
    if !trimmed.is_char_boundary(cmd.len()) {
        return None;
    }
    let (prefix, rest) = trimmed.split_at(cmd.len());
    if !prefix.eq_ignore_ascii_case(cmd) {
        return None;
    }
    if let Some(next) = rest.chars().next()
        && !next.is_whitespace()
    {
        return None;
    }
    Some(rest.trim_start())
}

fn is_email_token(w: &str) -> bool {
    let trimmed = w.trim_matches(|c: char| matches!(c, ',' | '.' | ';' | ':' | '!' | '?' | ')' | '(' | '"' | '\''));
    let Some((local, domain)) = trimmed.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !local.contains(['/', '\\', '?', '#', ':'])
        && !domain.is_empty()
        && !domain.contains(['/', '\\', '?', '#', '@', ':'])
        && domain.contains('.')
}

fn has_any_link(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("://") || lower.contains("www.") {
        return true;
    }
    let without_emails: String = text.split_whitespace().filter(|w| !is_email_token(w)).collect::<Vec<_>>().join(" ");
    !filter::links(&without_emails).is_empty()
}

fn is_emote_or_repeat_spam(text: &str, emote_count: usize) -> bool {
    if emote_count > 5 {
        return true;
    }
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let spoken_words: Vec<String> =
        tokens.iter().filter(|t| t.chars().any(char::is_alphanumeric)).map(|t| t.to_lowercase()).collect();
    if emote_count >= 3 && spoken_words.len() < 2 {
        return true;
    }
    let symbol_tokens = tokens.len().saturating_sub(spoken_words.len());
    let total_tokens = emote_count + tokens.len();
    if total_tokens > 0 && (emote_count + symbol_tokens) * 100 > total_tokens * 60 {
        return true;
    }
    if spoken_words.is_empty() {
        return false;
    }
    let mut consecutive = 1usize;
    let mut collapsed_words = 1usize;
    for i in 1..spoken_words.len() {
        if spoken_words[i] == spoken_words[i - 1] {
            consecutive += 1;
            if consecutive > 3 {
                return true;
            }
            if consecutive <= 2 {
                collapsed_words += 1;
            }
        } else {
            consecutive = 1;
            collapsed_words += 1;
        }
    }
    if collapsed_words == 0 {
        return true;
    }
    if spoken_words.len() >= 4 {
        let mut unique = spoken_words.clone();
        unique.sort_unstable();
        unique.dedup();
        if unique.len() == 1 || unique.len() * 3 < spoken_words.len() {
            return true;
        }
    }
    false
}

fn contains_severe_slur_in(
    folded: &str,
    whole_word: &[[u8; 16]],
    substring: &[[u8; 16]],
    min_len: usize,
    max_len: usize,
) -> bool {
    let tokens: Vec<&str> = folded.split(' ').filter(|t| !t.is_empty()).collect();
    for token in &tokens {
        let h = sha256_prefix16(token.as_bytes());
        if whole_word.contains(&h) {
            return true;
        }
        if contains_subtoken_hash(token, substring, min_len, max_len) {
            return true;
        }
    }
    // Check adjacent 2..=4 token concatenations against stem/substring hashes (catches `"nig ger"` /
    // `"re t ard"` / `"ni-gga"` splits without causing cross-boundary short-word false positives).
    for span in 2..=4usize {
        if tokens.len() < span {
            break;
        }
        for i in 0..=tokens.len() - span {
            let joined = tokens[i..i + span].concat();
            let len = joined.chars().count();
            if len >= min_len && len <= max_len + 1 {
                if substring.contains(&sha256_prefix16(joined.as_bytes())) {
                    return true;
                }
                if let Some(stem) = joined.strip_suffix('s')
                    && stem.chars().count() >= min_len
                    && substring.contains(&sha256_prefix16(stem.as_bytes()))
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Checks folded tokens against SHA-256 prefixes of severe slurs (none stored in plaintext).
fn contains_severe_slur(text: &str) -> bool {
    let folded_1 = filter::fold(text);
    if contains_severe_slur_in(&folded_1, WHOLE_WORD_FOLDED_1, SUBSTRING_FOLDED_1, 5, 12) {
        return true;
    }
    let folded_go = filter::fold_with(text, |c| c == 'g' || c == 'o');
    contains_severe_slur_in(&folded_go, WHOLE_WORD_FOLDED_GO, SUBSTRING_FOLDED_GO, 5, 8)
}

fn contains_subtoken_hash(token: &str, hashes: &[[u8; 16]], min_len: usize, max_len: usize) -> bool {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() < min_len {
        return false;
    }
    for len in min_len..=max_len.min(chars.len()) {
        for start in 0..=chars.len() - len {
            let sub: String = chars[start..start + len].iter().collect();
            if hashes.contains(&sha256_prefix16(sub.as_bytes())) {
                return true;
            }
        }
    }
    false
}

// 16-byte SHA-256 prefixes of 1-squeezed folded tokens.
const WHOLE_WORD_FOLDED_1: &[[u8; 16]] = &[
    [0x99, 0x15, 0xba, 0x2d, 0x82, 0x22, 0x80, 0xf2, 0x2c, 0x28, 0x3d, 0xf4, 0xe7, 0x65, 0x84, 0xa4],
    [0x17, 0xbd, 0xe8, 0xb4, 0x06, 0x46, 0x12, 0xe2, 0x66, 0xea, 0xa1, 0xb5, 0xb1, 0xdf, 0xa0, 0x54],
    [0x63, 0xbd, 0x93, 0x33, 0xb1, 0xa0, 0xb1, 0x1d, 0xa8, 0xd0, 0xb2, 0x06, 0xc4, 0xb8, 0x6c, 0xd2],
    [0x69, 0x3e, 0xe9, 0x96, 0x1b, 0xef, 0x39, 0xc1, 0x02, 0xf0, 0x77, 0xc0, 0x46, 0xc9, 0x3c, 0xc9],
    [0x15, 0x88, 0x69, 0xa9, 0x73, 0x79, 0x22, 0x9b, 0x76, 0x81, 0xef, 0xae, 0x9d, 0x7f, 0x9c, 0x92],
    [0xc1, 0xca, 0xbb, 0x6f, 0x6e, 0x43, 0x1f, 0x9c, 0x4d, 0xea, 0x4c, 0x3b, 0x32, 0x64, 0xd6, 0xfe],
    [0xbd, 0x33, 0x1f, 0xb1, 0xd2, 0x42, 0x98, 0xf5, 0x29, 0x43, 0x03, 0x4a, 0x24, 0x3a, 0x34, 0x18],
    [0xb2, 0x7c, 0x1d, 0x7f, 0x1e, 0x7f, 0xf7, 0xb3, 0x4e, 0x25, 0xac, 0x2d, 0xb5, 0xdc, 0x5f, 0x2f],
    [0x76, 0xb3, 0x0a, 0x2c, 0xe8, 0x5f, 0x13, 0xc1, 0xa3, 0x83, 0x43, 0x97, 0x9a, 0x83, 0xdb, 0x15],
    [0xc3, 0xde, 0x53, 0x3e, 0x9b, 0x7f, 0xe6, 0x3b, 0x79, 0xf6, 0x48, 0x68, 0x7a, 0x30, 0xd2, 0x86],
    [0x26, 0x86, 0x51, 0xb3, 0xec, 0xe9, 0x80, 0x10, 0x2f, 0x18, 0x87, 0x1f, 0xde, 0x07, 0x18, 0x93],
    [0x3c, 0x84, 0x00, 0xb4, 0x02, 0xba, 0xa7, 0xf1, 0xb8, 0xca, 0xc9, 0x4e, 0x00, 0xb9, 0xb9, 0x13],
    [0x79, 0x30, 0x1d, 0x9c, 0xce, 0xda, 0xe2, 0xa2, 0x93, 0xa9, 0x95, 0x5a, 0x2c, 0x6e, 0xbc, 0x1a],
    [0xf9, 0xd0, 0xd9, 0xb1, 0x8a, 0xe9, 0x03, 0x3a, 0x5e, 0xa3, 0x6d, 0xf1, 0x9b, 0xf2, 0x79, 0xb0],
    [0x45, 0xcd, 0x3e, 0x1b, 0xb4, 0x72, 0xd8, 0xe2, 0x85, 0xa1, 0x60, 0xa9, 0x5f, 0x64, 0x09, 0xec],
    [0x98, 0xb5, 0x2c, 0x4b, 0x6b, 0x7d, 0x1f, 0x48, 0xe7, 0x47, 0x7a, 0x5c, 0xcc, 0x10, 0x95, 0x5d],
    [0x04, 0x4e, 0xb9, 0x8b, 0x18, 0x76, 0x9b, 0x88, 0x7d, 0x5a, 0x02, 0x58, 0xf6, 0x75, 0xe4, 0x13],
    [0xee, 0xf3, 0xbd, 0x09, 0x16, 0x70, 0xc3, 0x44, 0x70, 0x22, 0xd6, 0x19, 0xc0, 0x6a, 0xd1, 0x5d],
    [0x0c, 0xe8, 0x75, 0xd6, 0x20, 0x07, 0x6b, 0x53, 0x3c, 0x6e, 0x68, 0x1a, 0xba, 0xa0, 0xdc, 0x5d],
    [0x56, 0x6f, 0x53, 0x2d, 0x48, 0x6c, 0x94, 0x77, 0x09, 0xd3, 0xd0, 0xe6, 0xb7, 0x57, 0x5a, 0xf8],
    [0x51, 0x7a, 0xe3, 0xf7, 0x32, 0x16, 0xda, 0x64, 0x1d, 0x8d, 0x31, 0x87, 0xca, 0x35, 0x4f, 0xec],
    [0x88, 0x6d, 0x51, 0xe9, 0x7a, 0xd7, 0x93, 0x1d, 0x0d, 0x2a, 0xf8, 0x43, 0x9c, 0xa6, 0xd9, 0xe4],
    [0x92, 0xa9, 0xbf, 0x81, 0x8e, 0x7e, 0x00, 0x02, 0x1b, 0x6d, 0x69, 0x59, 0xf8, 0x77, 0xf5, 0xdf],
    [0x12, 0xe6, 0x27, 0x4e, 0x43, 0x09, 0x29, 0x3e, 0x2d, 0x48, 0x02, 0x72, 0xb4, 0x9a, 0x6c, 0x7c],
    [0xbb, 0x20, 0x9d, 0x50, 0x6b, 0xb8, 0xb1, 0x78, 0x15, 0xaf, 0x0e, 0x50, 0x92, 0x3c, 0x3d, 0x8f],
    [0xcc, 0x02, 0x03, 0x23, 0x49, 0xc8, 0x33, 0xac, 0x5e, 0x97, 0xba, 0xc0, 0x94, 0x56, 0x0e, 0xd4],
    [0x82, 0x15, 0x9d, 0xd0, 0x28, 0x70, 0xc1, 0x3e, 0xc3, 0x0e, 0x4f, 0x14, 0x6d, 0xb7, 0xc3, 0x17],
    [0xe7, 0x42, 0xd1, 0x7f, 0xf4, 0x90, 0xc8, 0x7e, 0x19, 0xd0, 0xcf, 0xbf, 0xdb, 0x18, 0xc6, 0x91],
    [0x22, 0xfc, 0x75, 0xe6, 0x5a, 0x0e, 0x9d, 0x34, 0x32, 0x40, 0x92, 0xa7, 0xc6, 0xa8, 0xdb, 0xa9],
    [0x33, 0x3f, 0x76, 0x18, 0x09, 0x29, 0x58, 0xc7, 0x5b, 0x8c, 0x5a, 0xf6, 0xf1, 0xec, 0x77, 0xb4],
];

// Substring-safe 1-squeezed severe tokens (excluding terms that match unrelated substrings).
const SUBSTRING_FOLDED_1: &[[u8; 16]] = &[
    [0x63, 0xbd, 0x93, 0x33, 0xb1, 0xa0, 0xb1, 0x1d, 0xa8, 0xd0, 0xb2, 0x06, 0xc4, 0xb8, 0x6c, 0xd2],
    [0x69, 0x3e, 0xe9, 0x96, 0x1b, 0xef, 0x39, 0xc1, 0x02, 0xf0, 0x77, 0xc0, 0x46, 0xc9, 0x3c, 0xc9],
    [0x15, 0x88, 0x69, 0xa9, 0x73, 0x79, 0x22, 0x9b, 0x76, 0x81, 0xef, 0xae, 0x9d, 0x7f, 0x9c, 0x92],
    [0xc1, 0xca, 0xbb, 0x6f, 0x6e, 0x43, 0x1f, 0x9c, 0x4d, 0xea, 0x4c, 0x3b, 0x32, 0x64, 0xd6, 0xfe],
    [0xbd, 0x33, 0x1f, 0xb1, 0xd2, 0x42, 0x98, 0xf5, 0x29, 0x43, 0x03, 0x4a, 0x24, 0x3a, 0x34, 0x18],
    [0xee, 0xf3, 0xbd, 0x09, 0x16, 0x70, 0xc3, 0x44, 0x70, 0x22, 0xd6, 0x19, 0xc0, 0x6a, 0xd1, 0x5d],
    [0x0c, 0xe8, 0x75, 0xd6, 0x20, 0x07, 0x6b, 0x53, 0x3c, 0x6e, 0x68, 0x1a, 0xba, 0xa0, 0xdc, 0x5d],
    [0x12, 0xe6, 0x27, 0x4e, 0x43, 0x09, 0x29, 0x3e, 0x2d, 0x48, 0x02, 0x72, 0xb4, 0x9a, 0x6c, 0x7c],
    [0xbb, 0x20, 0x9d, 0x50, 0x6b, 0xb8, 0xb1, 0x78, 0x15, 0xaf, 0x0e, 0x50, 0x92, 0x3c, 0x3d, 0x8f],
    [0xe7, 0x42, 0xd1, 0x7f, 0xf4, 0x90, 0xc8, 0x7e, 0x19, 0xd0, 0xcf, 0xbf, 0xdb, 0x18, 0xc6, 0x91],
    [0x22, 0xfc, 0x75, 0xe6, 0x5a, 0x0e, 0x9d, 0x34, 0x32, 0x40, 0x92, 0xa7, 0xc6, 0xa8, 0xdb, 0xa9],
    [0x33, 0x3f, 0x76, 0x18, 0x09, 0x29, 0x58, 0xc7, 0x5b, 0x8c, 0x5a, 0xf6, 0xf1, 0xec, 0x77, 0xb4],
];

// Double-'g'/'o'-preserving folded tokens so benign single-letter words aren't matched.
const WHOLE_WORD_FOLDED_GO: &[[u8; 16]] = &[
    [0x12, 0x0f, 0x6e, 0x5b, 0x4e, 0xa3, 0x2f, 0x65, 0xbd, 0xa6, 0x84, 0x52, 0xfc, 0xfa, 0xae, 0xf0],
    [0x5b, 0x3a, 0xe4, 0x8b, 0xe1, 0x22, 0xf7, 0xed, 0x19, 0xb4, 0xcc, 0x58, 0x76, 0x49, 0xf4, 0x1f],
    [0x08, 0xa8, 0x41, 0xe9, 0x96, 0x78, 0x1e, 0x9e, 0x77, 0xd3, 0x0a, 0x4e, 0x44, 0x20, 0xa8, 0xf5],
    [0x34, 0x1d, 0x56, 0x38, 0x4a, 0xfc, 0x0f, 0x47, 0xb3, 0x4c, 0xa1, 0x82, 0x73, 0xe7, 0x93, 0xbe],
    [0xe7, 0xb9, 0x8c, 0x6a, 0xa5, 0xb9, 0x44, 0xe0, 0xb3, 0x15, 0xd3, 0x50, 0xd4, 0x23, 0xf8, 0x95],
    [0xda, 0xec, 0x0c, 0x23, 0x5d, 0x31, 0x35, 0xfc, 0x1c, 0x60, 0xae, 0xe7, 0x87, 0xf7, 0x84, 0xc5],
];

const SUBSTRING_FOLDED_GO: &[[u8; 16]] = &[
    [0x12, 0x0f, 0x6e, 0x5b, 0x4e, 0xa3, 0x2f, 0x65, 0xbd, 0xa6, 0x84, 0x52, 0xfc, 0xfa, 0xae, 0xf0],
    [0x5b, 0x3a, 0xe4, 0x8b, 0xe1, 0x22, 0xf7, 0xed, 0x19, 0xb4, 0xcc, 0x58, 0x76, 0x49, 0xf4, 0x1f],
    [0x08, 0xa8, 0x41, 0xe9, 0x96, 0x78, 0x1e, 0x9e, 0x77, 0xd3, 0x0a, 0x4e, 0x44, 0x20, 0xa8, 0xf5],
    [0x34, 0x1d, 0x56, 0x38, 0x4a, 0xfc, 0x0f, 0x47, 0xb3, 0x4c, 0xa1, 0x82, 0x73, 0xe7, 0x93, 0xbe],
];

fn sha256_prefix16(data: &[u8]) -> [u8; 16] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] =
        [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, b) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*b);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, delta) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(delta);
        }
    }
    let mut out = [0u8; 16];
    for (i, word) in h.iter().take(4).enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn meets(mins: &[Money], money: &Money) -> bool {
    mins.iter().any(|m| m.currency.eq_ignore_ascii_case(&money.currency) && money.minor >= m.minor)
}

/// Removes links, emails, long digit runs (phones, addresses, card numbers), repeated
/// characters and control characters, then caps the length at a word boundary.
pub fn clean(text: &str, max_chars: usize) -> String {
    let words: Vec<String> = text
        .split_whitespace()
        .filter(|w| !looks_like_link(w) && !w.contains('@'))
        .map(|w| {
            let digits = w.chars().filter(char::is_ascii_digit).count();
            if digits >= 5 { "number".to_string() } else { squeeze_runs(w, 3) }
        })
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .collect();
    let mut out = String::new();
    for w in words {
        let w: String = w.chars().filter(|c| !c.is_control()).collect();
        let sep = usize::from(!out.is_empty());
        if out.chars().count() + w.chars().count() + sep > max_chars {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&w);
    }
    out
}

fn looks_like_link(w: &str) -> bool {
    let l = w.to_lowercase();
    l.contains("://") || l.starts_with("www.") || !filter::links(&l).is_empty()
}

fn squeeze_runs(w: &str, max: usize) -> String {
    let mut out = String::new();
    let (mut last, mut run) = (None, 0);
    for c in w.chars() {
        run = if Some(c) == last { run + 1 } else { 1 };
        last = Some(c);
        if run <= max {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Actor, Millis, PaymentStatus, Platform, TipProvider};

    fn tip(minor: i64, cur: &str, msg: &str) -> Event {
        Event::live(
            Platform::Tips,
            Some("t".into()),
            Millis(0),
            EventKind::Tip {
                actor: Actor::named(Platform::Tips, "Asha"),
                money: Money::new(minor, cur),
                message: Some(msg.into()),
                provider: TipProvider::Razorpay,
                payment_ref: "p".into(),
                status: PaymentStatus::Captured,
            },
        )
    }

    fn on() -> TtsPolicy {
        TtsPolicy { enabled: true, ..TtsPolicy::default() }
    }

    #[test]
    fn off_by_default() {
        assert!(matches!(
            decide(&TtsPolicy::default(), &FilterConfig::default(), &tip(50_000, "INR", "hi")),
            Decision::Skip { .. }
        ));
    }

    #[test]
    fn reads_above_minimum_in_known_currencies_only() {
        let f = FilterConfig::default();
        assert_eq!(
            decide(&on(), &f, &tip(50_000, "INR", "great run")),
            Decision::Speak { text: "Asha says: great run".into() }
        );
        assert!(matches!(decide(&on(), &f, &tip(5_000, "INR", "hi")), Decision::Skip { .. }));
        assert!(matches!(decide(&on(), &f, &tip(1_000_000, "JPY", "hi")), Decision::Skip { .. }), "JPY not listed");
    }

    #[test]
    fn strips_doxxing_and_links() {
        let out = clean("call me 9876543210 or visit evil.com / https://x.y at 221B Baker st, mail a@b.co", 200);
        assert_eq!(out, "call me number or visit at 221B Baker st, mail");
        assert_eq!(clean("hiiiiiiiiii !!!!!!", 200), "hiii");
        assert_eq!(clean("one two three four", 9), "one two");
        assert_eq!(clean("abcdefghij", 10), "abcdefghij");
        assert_eq!(clean("abcdefghij", 200), "abcdefghij");
    }

    #[test]
    fn filtered_messages_are_held_not_spoken() {
        let mut f = FilterConfig::default();
        f.words.list.push("badword".into());
        assert!(matches!(decide(&on(), &f, &tip(50_000, "INR", "b4dw0rd lol")), Decision::Hold { .. }));
    }

    #[test]
    fn approval_mode_holds_everything() {
        let p = TtsPolicy { hold_for_approval: true, ..on() };
        assert!(matches!(decide(&p, &FilterConfig::default(), &tip(50_000, "INR", "hi")), Decision::Hold { .. }));
    }

    #[test]
    fn empty_or_unreadable_messages_are_skipped() {
        let f = FilterConfig::default();
        assert!(matches!(decide(&on(), &f, &tip(50_000, "INR", "   ")), Decision::Skip { .. }));
        assert!(matches!(decide(&on(), &f, &tip(50_000, "INR", "!!!! ???")), Decision::Skip { .. }));
    }

    fn chat_ev(roles: Roles, origin: Origin, shared: bool, content: Content) -> Event {
        let mut actor = Actor::named(Platform::Twitch, "Riya");
        actor.id = Some("u1".into());
        actor.roles = roles;
        let mut e = Event::live(
            Platform::Twitch,
            Some("msg-1".into()),
            Millis(100),
            EventKind::ChatMessage { actor, content, reply_to: None, is_action: false },
        );
        e.origin = origin;
        if shared {
            e.channel = Some(crate::event::ChannelRef {
                platform: Platform::Twitch,
                id: "partner".into(),
                name: "Partner".into(),
            });
        }
        e
    }

    fn chat_on(mode: ChatTtsMode) -> TtsPolicy {
        TtsPolicy {
            enabled: true,
            chat: ChatTtsConfig { enabled: true, mode, ..ChatTtsConfig::default() },
            ..TtsPolicy::default()
        }
    }

    fn emote(name: &str) -> Fragment {
        Fragment::Emote {
            provider: crate::event::EmoteProvider::Native,
            id: name.into(),
            name: name.into(),
            url: "https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/1.0".into(),
            zero_width: false,
        }
    }

    #[test]
    fn sha256_prefix_matches_known_vectors() {
        assert_eq!(
            sha256_prefix16(b"abc"),
            [0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23]
        );
        assert_eq!(
            sha256_prefix16(b""),
            [0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24]
        );
    }

    #[test]
    fn chat_tts_config_validates_bounds_and_command() {
        assert!(ChatTtsConfig::default().validate().is_ok());
        for bad_cmd in ["tts", "!", "!bad cmd", "!too_long_command_name_123", "!bad!"] {
            let cfg = ChatTtsConfig { command: bad_cmd.into(), ..ChatTtsConfig::default() };
            assert!(cfg.validate().unwrap_err().starts_with("chat.command"), "{bad_cmd}");
        }
        assert!(ChatTtsConfig { cooldown_s: 301, ..ChatTtsConfig::default() }.validate().is_err());
        assert!(ChatTtsConfig { global_cooldown_s: 61, ..ChatTtsConfig::default() }.validate().is_err());
        assert!(ChatTtsConfig { queue_limit: 0, ..ChatTtsConfig::default() }.validate().is_err());
        assert!(ChatTtsConfig { queue_limit: 21, ..ChatTtsConfig::default() }.validate().is_err());
        assert!(ChatTtsConfig { max_chars: 9, ..ChatTtsConfig::default() }.validate().is_err());
        assert!(ChatTtsConfig { max_chars: 301, ..ChatTtsConfig::default() }.validate().is_err());
    }

    #[test]
    fn chat_tts_serde_backwards_compat() {
        let old = r#"{"enabled":true,"voice":null,"min_amounts":[],"min_platform_value":200,"max_chars":200,"read_names":true,"hold_for_approval":false}"#;
        let parsed: TtsPolicy = serde_json::from_str(old).unwrap();
        assert!(!parsed.chat.enabled);
        assert_eq!(parsed.chat.mode, ChatTtsMode::SubsMods);
        assert_eq!(parsed.chat.command, "!tts");
    }

    #[test]
    fn chat_tts_modes_and_origin_guards() {
        let f = FilterConfig::default();
        // Everyone reads plain chat, skips bot commands.
        let p_all = chat_on(ChatTtsMode::Everyone);
        assert_eq!(
            decide_chat(&p_all, &f, &chat_ev(Roles::empty(), Origin::Live, false, Content::plain("hello chat"))),
            Decision::Speak { text: "Riya says: hello chat".into() }
        );
        assert!(matches!(
            decide_chat(&p_all, &f, &chat_ev(Roles::empty(), Origin::Live, false, Content::plain("!vote 1"))),
            Decision::Skip { .. }
        ));

        // Subs & mods skips non-sub viewers and allows sub/member/vip/mod/broadcaster.
        let p_sub = chat_on(ChatTtsMode::SubsMods);
        assert!(matches!(
            decide_chat(&p_sub, &f, &chat_ev(Roles::empty(), Origin::Live, false, Content::plain("hello"))),
            Decision::Skip { .. }
        ));
        for role in [Roles::SUBSCRIBER, Roles::MEMBER, Roles::FOUNDER, Roles::VIP, Roles::MODERATOR, Roles::BROADCASTER]
        {
            assert_eq!(
                decide_chat(&p_sub, &f, &chat_ev(role, Origin::Live, false, Content::plain("nice play"))),
                Decision::Speak { text: "Riya says: nice play".into() }
            );
        }
        assert!(matches!(
            decide_chat(&p_sub, &f, &chat_ev(Roles::SUBSCRIBER, Origin::Live, false, Content::plain("!lurk"))),
            Decision::Skip { .. }
        ));

        // Command mode strips case-insensitive !tts prefix and rejects other commands or partial matches.
        let p_cmd = chat_on(ChatTtsMode::Command);
        assert_eq!(
            decide_chat(
                &p_cmd,
                &f,
                &chat_ev(Roles::empty(), Origin::Live, false, Content::plain("  !TTS  what a clutch"))
            ),
            Decision::Speak { text: "Riya says: what a clutch".into() }
        );
        assert!(matches!(
            decide_chat(&p_cmd, &f, &chat_ev(Roles::empty(), Origin::Live, false, Content::plain("!ttsextra hi"))),
            Decision::Skip { .. }
        ));
        assert!(matches!(
            decide_chat(&p_cmd, &f, &chat_ev(Roles::empty(), Origin::Live, false, Content::plain("hello without cmd"))),
            Decision::Skip { .. }
        ));

        // Backfill, Shared Chat, and Bot roles are always skipped.
        assert!(matches!(
            decide_chat(&p_all, &f, &chat_ev(Roles::SUBSCRIBER, Origin::Backfill, false, Content::plain("old"))),
            Decision::Skip { .. }
        ));
        assert!(matches!(
            decide_chat(&p_all, &f, &chat_ev(Roles::SUBSCRIBER, Origin::Live, true, Content::plain("shared"))),
            Decision::Skip { .. }
        ));
        assert!(matches!(
            decide_chat(
                &p_all,
                &f,
                &chat_ev(Roles::BOT.union(Roles::MODERATOR), Origin::Live, false, Content::plain("bot msg"))
            ),
            Decision::Skip { .. }
        ));
    }

    #[test]
    fn chat_tts_skips_links_and_emote_spam_and_strips_emotes() {
        let f = FilterConfig::default();
        let p = chat_on(ChatTtsMode::Everyone);

        // Links skipped whole, including URLs containing `@` in path/handle.
        for msg in [
            "check https://example.com right now",
            "visit www.evil.org for free skins",
            "go to cheap . xyz today",
            "free coins at evil.com/@promo click now",
            "subscribe youtube.com/@Handle here",
        ] {
            assert!(
                matches!(
                    decide_chat(&p, &f, &chat_ev(Roles::empty(), Origin::Live, false, Content::plain(msg))),
                    Decision::Skip { .. }
                ),
                "{msg}"
            );
        }
        let link_frag = Content {
            text: "look at link".into(),
            fragments: vec![
                Fragment::Text { text: "look at ".into() },
                Fragment::Link { url: "https://x.test".into() },
            ],
        };
        assert!(matches!(
            decide_chat(&p, &f, &chat_ev(Roles::empty(), Origin::Live, false, link_frag)),
            Decision::Skip { .. }
        ));

        // Emote fragments are stripped so only spoken text + mentions remain.
        let mixed = Content {
            text: "gg Kappa @Alex PogChamp".into(),
            fragments: vec![
                Fragment::Text { text: "gg ".into() },
                emote("Kappa"),
                Fragment::Text { text: " ".into() },
                Fragment::Mention { login: "@Alex".into() },
                Fragment::Text { text: " ".into() },
                emote("PogChamp"),
            ],
        };
        assert_eq!(
            decide_chat(&p, &f, &chat_ev(Roles::empty(), Origin::Live, false, mixed)),
            Decision::Speak { text: "Riya says: gg Alex".into() }
        );

        // Emote-only after !tts command returns empty -> skipped.
        let p_cmd = chat_on(ChatTtsMode::Command);
        let cmd_emote_only = Content {
            text: "!tts Kappa PogChamp".into(),
            fragments: vec![
                Fragment::Text { text: "!tts ".into() },
                emote("Kappa"),
                Fragment::Text { text: " ".into() },
                emote("PogChamp"),
            ],
        };
        assert!(matches!(
            decide_chat(&p_cmd, &f, &chat_ev(Roles::empty(), Origin::Live, false, cmd_emote_only)),
            Decision::Skip { .. }
        ));

        // >5 emotes skipped; >=3 emotes with <2 spoken words skipped; repetition spam skipped.
        let six_emotes = Content {
            text: "hi".into(),
            fragments: vec![
                Fragment::Text { text: "nice shot ".into() },
                emote("e1"),
                emote("e2"),
                emote("e3"),
                emote("e4"),
                emote("e5"),
                emote("e6"),
            ],
        };
        assert!(matches!(
            decide_chat(&p, &f, &chat_ev(Roles::empty(), Origin::Live, false, six_emotes)),
            Decision::Skip { .. }
        ));

        let three_emotes_one_word = Content {
            text: "hi".into(),
            fragments: vec![Fragment::Text { text: "wow ".into() }, emote("e1"), emote("e2"), emote("e3")],
        };
        assert!(matches!(
            decide_chat(&p, &f, &chat_ev(Roles::empty(), Origin::Live, false, three_emotes_one_word)),
            Decision::Skip { .. }
        ));
        assert!(matches!(
            decide_chat(&p, &f, &chat_ev(Roles::empty(), Origin::Live, false, Content::plain("777 777 777 777"))),
            Decision::Skip { .. }
        ));
        assert!(matches!(
            decide_chat(
                &p,
                &f,
                &chat_ev(Roles::empty(), Origin::Live, false, Content::plain("ha lol ha lol ha lol ha lol"))
            ),
            Decision::Skip { .. }
        ));
    }

    #[test]
    fn chat_tts_enforces_slur_guard_and_blocked_words_even_for_exempt_roles_without_false_positives() {
        let mut f = FilterConfig::default();
        f.words.list.push("spoiled".into());
        f.exempt_roles = Roles::MODERATOR.union(Roles::VIP).union(Roles::SUBSCRIBER);
        let p = chat_on(ChatTtsMode::Everyone);

        // Custom blocked word is skipped even for VIP/Mod/Sub, not held.
        for role in [Roles::VIP, Roles::MODERATOR, Roles::SUBSCRIBER] {
            assert!(matches!(
                decide_chat(&p, &f, &chat_ev(role, Origin::Live, false, Content::plain("sp01led ending"))),
                Decision::Skip { .. }
            ));
        }

        // Obfuscated built-in severe slurs constructed dynamically (zero plaintext slurs in test).
        let spaced = ["r", "e", "t", "a", "r", "d"].join(".");
        let cyrillic = format!("{}3{}4rd", "r", "\u{0442}");
        let stretched = format!("{}a{}g{}o{}t", "f", "aaaa", "ggg", "ooo");
        let zws = ["n", "i", "g", "g", "a"].join("\u{200b}");
        let embedded = format!("xx{}123", ["n", "1", "g", "g", "3", "r"].concat());
        let split2_space = format!("{} {}", ["n", "i", "g"].concat(), ["g", "e", "r"].concat());
        let split2_dot = format!("{}.{}", ["f", "a", "g"].concat(), ["g", "o", "t"].concat());
        let split3 = format!("{} t {}", ["r", "e"].concat(), ["a", "r", "d"].concat());
        let split2_dash = format!("{}-{}", ["n", "i"].concat(), ["g", "g", "a"].concat());
        for hostile in [spaced, cyrillic, stretched, zws, embedded, split2_space, split2_dot, split3, split2_dash] {
            assert!(
                matches!(
                    decide_chat(&p, &f, &chat_ev(Roles::VIP, Origin::Live, false, Content::plain(&hostile))),
                    Decision::Skip { .. }
                ),
                "obfuscated input must be skipped"
            );
        }

        // Ordinary words near boundaries must never false-positive.
        for ok in [
            "Greetings from Nigeria and Niger",
            "first class assistant",
            "we're tardy to the party",
            "let's pic together",
            "visited Scunthorpe today",
            "comic con spicy raccoon",
        ] {
            assert!(
                matches!(
                    decide_chat(&p, &f, &chat_ev(Roles::empty(), Origin::Live, false, Content::plain(ok))),
                    Decision::Speak { .. }
                ),
                "{ok}"
            );
        }
    }

    #[test]
    fn chat_tts_cleans_pii_respects_max_chars_and_hold_for_approval() {
        let f = FilterConfig::default();
        let mut p = chat_on(ChatTtsMode::Everyone);
        p.chat.max_chars = 24;
        p.read_names = false;
        assert_eq!(
            decide_chat(
                &p,
                &f,
                &chat_ev(Roles::empty(), Origin::Live, false, Content::plain("call 9876543210 a@b.co now please"))
            ),
            Decision::Speak { text: "call number now please".into() }
        );
        p.hold_for_approval = true;
        assert_eq!(
            decide_chat(&p, &f, &chat_ev(Roles::empty(), Origin::Live, false, Content::plain("hello world"))),
            Decision::Hold { text: "hello world".into() }
        );
    }
}
