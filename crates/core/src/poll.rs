//! Stream polls: native platform polls (Twitch Helix, YouTube live chat) when supported
//! and `!vote` chat polls elsewhere (or as fallback).
//!
//! Chat polls allow one vote per person (linked accounts count once); voting again changes
//! the vote. Votes after the end, or after the poll closes early, are refused.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::event::{Millis, Platform};

pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 5;
pub const MIN_DURATION_S: u32 = 10;
pub const MAX_DURATION_S: u32 = 30 * 60;
pub const DEFAULT_DURATION_S: u32 = 60;
/// More voters than any real chat, so a bot flood can't grow memory without bound.
pub const MAX_VOTERS: usize = 200_000;

pub const DEFAULT_LIMITS: PollLimits = PollLimits {
    min_options: MIN_OPTIONS,
    max_options: MAX_OPTIONS,
    min_duration_s: MIN_DURATION_S,
    max_duration_s: MAX_DURATION_S,
    supports_no_timer: true,
    question_chars: 120,
    option_chars: 60,
};

fn default_duration_s() -> Option<u32> {
    Some(DEFAULT_DURATION_S)
}

fn default_chat_fallback() -> bool {
    true
}

/// Whether a connected platform can run native polls right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PollCapabilityMode {
    Native,
    ChatFallback,
    ReconnectNeeded,
}

/// Platform constraints reported to the UI so warnings are proactive and concise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollLimits {
    pub min_options: usize,
    pub max_options: usize,
    pub min_duration_s: u32,
    pub max_duration_s: u32,
    pub supports_no_timer: bool,
    pub question_chars: usize,
    pub option_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformPollCapability {
    pub platform: Platform,
    pub mode: PollCapabilityMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub limits: PollLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollsCapabilities {
    pub platforms: Vec<PlatformPollCapability>,
}

/// How an active poll runs on a given platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformPollMode {
    Native,
    Chat,
}

/// Lifecycle state of an active poll on a single platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformPollState {
    Active,
    Ended,
    FailedFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformPollStatus {
    pub platform: Platform,
    pub mode: PlatformPollMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    pub state: PlatformPollState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What the dashboard asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollSpec {
    pub question: String,
    pub options: Vec<String>,
    /// `None` starts a "No timer" poll that runs until closed manually.
    #[serde(default = "default_duration_s")]
    pub duration_s: Option<u32>,
    #[serde(default = "default_chat_fallback")]
    pub chat_fallback: bool,
}

impl PollSpec {
    /// Trims everything and checks it; errors name the field.
    pub fn cleaned(&self) -> Result<PollSpec, String> {
        let question = one_line(&self.question);
        if question.is_empty() || question.chars().count() > 120 {
            return Err("question: ask something 1 to 120 characters long.".into());
        }
        let options: Vec<String> = self.options.iter().map(|o| one_line(o)).collect();
        if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&options.len()) {
            return Err("options: give 2 to 5 answers.".into());
        }
        for (i, o) in options.iter().enumerate() {
            if o.is_empty() || o.chars().count() > 60 {
                return Err(format!("options: answer {} must be 1 to 60 characters.", i + 1));
            }
            if options[..i].iter().any(|p| p.to_lowercase() == o.to_lowercase()) {
                return Err(format!("options: \"{o}\" is in the list twice."));
            }
        }
        if let Some(secs) = self.duration_s
            && !(MIN_DURATION_S..=MAX_DURATION_S).contains(&secs)
        {
            return Err("duration_s: polls run for 10 seconds to 30 minutes.".into());
        }
        Ok(PollSpec { question, options, duration_s: self.duration_s, chat_fallback: self.chat_fallback })
    }
}

/// Collapses whitespace and control characters: poll text goes into chat on one line.
fn one_line(s: &str) -> String {
    s.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Poll {
    pub id: String,
    pub question: String,
    pub options: Vec<String>,
    pub started_at: Millis,
    pub ends_at: Option<Millis>,
    /// Closed early, or finished and announced.
    pub closed: bool,
    /// Per-platform delivery state (`Native` vs `Chat`). Empty means legacy chat-everywhere mode.
    pub platforms: Vec<PlatformPollStatus>,
    /// Voter key (one per person) → the option index, from 0.
    votes: HashMap<String, usize>,
    /// Latest option vote counts reported by native platform polls, keyed by platform.
    native_tallies: HashMap<Platform, Vec<u64>>,
}

/// Why a vote didn't count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteRefused {
    Ended,
    NoSuchOption,
    Full,
}

/// A vote that counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Voted {
    New,
    Changed,
    Same,
}

/// A poll as the dashboard and overlays see it. Who voted for what is never shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollView {
    pub id: String,
    pub question: String,
    pub options: Vec<OptionView>,
    pub total: u64,
    pub started_at: Millis,
    pub ends_at: Option<Millis>,
    pub closed: bool,
    /// Indexes (from 0) of the leading answers; several on a tie, none without votes.
    pub leaders: Vec<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<PlatformPollStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionView {
    pub text: String,
    pub votes: u64,
}

impl Poll {
    pub fn start(id: String, spec: PollSpec, now: Millis) -> Poll {
        let ends_at = spec.duration_s.map(|secs| Millis(now.0.saturating_add(i64::from(secs) * 1000)));
        Poll {
            id,
            question: spec.question,
            options: spec.options,
            started_at: now,
            ends_at,
            closed: false,
            platforms: Vec::new(),
            votes: HashMap::new(),
            native_tallies: HashMap::new(),
        }
    }

    pub fn is_open(&self, now: Millis) -> bool {
        !self.closed && self.ends_at.is_none_or(|end| now < end)
    }

    pub fn set_platforms(&mut self, platforms: Vec<PlatformPollStatus>) {
        self.platforms = platforms;
    }

    pub fn allows_chat_vote(&self, platform: Platform) -> bool {
        self.platforms.is_empty()
            || self.platforms.iter().any(|p| p.platform == platform && p.mode == PlatformPollMode::Chat)
    }

    pub fn has_chat_platforms(&self) -> bool {
        self.platforms.is_empty() || self.platforms.iter().any(|p| p.mode == PlatformPollMode::Chat)
    }

    pub fn chat_platforms(&self) -> Option<HashSet<Platform>> {
        if self.platforms.is_empty() {
            return None;
        }
        Some(self.platforms.iter().filter(|p| p.mode == PlatformPollMode::Chat).map(|p| p.platform).collect())
    }

    /// Replaces the cached native counts for `platform`; returns `true` if values changed.
    pub fn set_native_tally(&mut self, platform: Platform, counts: &[u64]) -> bool {
        let mut next = vec![0u64; self.options.len()];
        for (dst, &src) in next.iter_mut().zip(counts.iter()) {
            *dst = src;
        }
        if self.native_tallies.get(&platform) == Some(&next) {
            return false;
        }
        self.native_tallies.insert(platform, next);
        true
    }

    pub fn set_platform_state(&mut self, platform: Platform, state: PlatformPollState, note: Option<String>) -> bool {
        let Some(entry) = self.platforms.iter_mut().find(|p| p.platform == platform) else {
            return false;
        };
        if entry.state == state && entry.note == note {
            return false;
        }
        entry.state = state;
        entry.note = note;
        true
    }

    /// `true` once every platform tracking this poll has ended (used to auto-finish native-only polls).
    pub fn all_platforms_ended(&self) -> bool {
        !self.platforms.is_empty() && self.platforms.iter().all(|p| p.state == PlatformPollState::Ended)
    }

    /// `choice` is what the viewer typed: 1 for the first answer.
    pub fn vote(&mut self, voter: &str, choice: usize, now: Millis) -> Result<Voted, VoteRefused> {
        if !self.is_open(now) {
            return Err(VoteRefused::Ended);
        }
        let Some(index) = choice.checked_sub(1).filter(|i| *i < self.options.len()) else {
            return Err(VoteRefused::NoSuchOption);
        };
        match self.votes.get(voter) {
            Some(prev) if *prev == index => Ok(Voted::Same),
            Some(_) => {
                self.votes.insert(voter.to_string(), index);
                Ok(Voted::Changed)
            }
            None if self.votes.len() >= MAX_VOTERS => Err(VoteRefused::Full),
            None => {
                self.votes.insert(voter.to_string(), index);
                Ok(Voted::New)
            }
        }
    }

    /// Closes the poll. `false` when it was already closed (a second "end" does nothing).
    pub fn close(&mut self) -> bool {
        let changed = !std::mem::replace(&mut self.closed, true);
        if changed {
            for p in &mut self.platforms {
                if p.state == PlatformPollState::Active {
                    p.state = PlatformPollState::Ended;
                }
            }
        }
        changed
    }

    pub fn tally(&self) -> Vec<u64> {
        let mut t = vec![0u64; self.options.len()];
        for i in self.votes.values() {
            if let Some(slot) = t.get_mut(*i) {
                *slot = slot.saturating_add(1);
            }
        }
        for counts in self.native_tallies.values() {
            for (slot, &n) in t.iter_mut().zip(counts.iter()) {
                *slot = slot.saturating_add(n);
            }
        }
        t
    }

    pub fn view(&self) -> PollView {
        let tally = self.tally();
        let total: u64 = tally.iter().copied().fold(0u64, u64::saturating_add);
        let top = tally.iter().copied().max().unwrap_or(0);
        let leaders = if top == 0 { vec![] } else { (0..tally.len()).filter(|i| tally[*i] == top).collect() };
        PollView {
            id: self.id.clone(),
            question: self.question.clone(),
            options: self
                .options
                .iter()
                .zip(&tally)
                .map(|(text, votes)| OptionView { text: text.clone(), votes: *votes })
                .collect(),
            total,
            started_at: self.started_at,
            ends_at: self.ends_at,
            closed: self.closed,
            leaders,
            platforms: self.platforms.clone(),
        }
    }

    /// The chat message that opens the poll.
    pub fn announcement(&self) -> String {
        let choices: Vec<String> =
            self.options.iter().enumerate().map(|(i, o)| format!("!vote {} for {o}", i + 1)).collect();
        match self.ends_at {
            Some(ends_at) => {
                let secs = (ends_at.0 - self.started_at.0).max(0) / 1000;
                let time = if secs >= 60 && secs % 60 == 0 {
                    let m = secs / 60;
                    format!("{m} minute{}", if m == 1 { "" } else { "s" })
                } else {
                    format!("{secs} seconds")
                };
                format!("Poll: {} Type {}. You have {time}.", self.question, choices.join(", "))
            }
            None => format!("Poll: {} Type {}. Open until closed.", self.question, choices.join(", ")),
        }
    }

    /// The chat message with the result.
    pub fn result(&self) -> String {
        let v = self.view();
        if v.total == 0 {
            return format!("Poll closed: {} Nobody voted.", self.question);
        }
        let pct = |n: u64| (n * 100 + v.total / 2) / v.total;
        let votes = |n: u64| format!("{n} vote{}", if n == 1 { "" } else { "s" });
        match v.leaders.as_slice() {
            [one] => {
                let o = &v.options[*one];
                format!(
                    "Poll closed: {} Winner: {} with {} ({}%).",
                    self.question,
                    o.text,
                    votes(o.votes),
                    pct(o.votes)
                )
            }
            many => {
                let names: Vec<&str> = many.iter().map(|i| v.options[*i].text.as_str()).collect();
                let n = many.first().map(|i| v.options[*i].votes).unwrap_or(0);
                format!(
                    "Poll closed: {} It's a tie between {} ({} each).",
                    self.question,
                    names.join(" and "),
                    votes(n)
                )
            }
        }
    }
}

/// `!vote 2` → `Some(2)`. Anything else, including `!vote two`, is `None`.
pub fn parse_vote(text: &str) -> Option<usize> {
    let inv = crate::command::parse(text)?;
    if !inv.word.eq_ignore_ascii_case("!vote") {
        return None;
    }
    let n = inv.args.first()?.trim_start_matches('#');
    if n.is_empty() || n.len() > 2 || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(opts: &[&str]) -> PollSpec {
        PollSpec {
            question: "Which map?".into(),
            options: opts.iter().map(|s| s.to_string()).collect(),
            duration_s: Some(60),
            chat_fallback: true,
        }
    }

    #[test]
    fn specs_are_cleaned_and_checked_with_field_names() {
        let s = PollSpec {
            question: "  Which\n map? ".into(),
            options: vec![" Ascent ".into(), "Bind".into()],
            duration_s: Some(60),
            chat_fallback: true,
        };
        let c = s.cleaned().unwrap();
        assert_eq!(c.question, "Which map?");
        assert_eq!(c.options, ["Ascent", "Bind"]);
        let err = |s: PollSpec| s.cleaned().unwrap_err();
        assert!(err(PollSpec { question: " ".into(), ..spec(&["a", "b"]) }).starts_with("question:"));
        assert!(err(PollSpec { question: "q".repeat(121), ..spec(&["a", "b"]) }).starts_with("question:"));
        assert!(err(spec(&["a"])).starts_with("options:"));
        assert!(err(spec(&["a", "b", "c", "d", "e", "f"])).starts_with("options:"));
        assert!(err(spec(&["a", " "])).starts_with("options: answer 2"));
        assert!(err(spec(&["Bind", "bind"])).contains("twice"));
        assert!(err(spec(&["a", &"x".repeat(61)])).starts_with("options:"));
        assert!(err(PollSpec { duration_s: Some(9), ..spec(&["a", "b"]) }).starts_with("duration_s:"));
        assert!(err(PollSpec { duration_s: Some(1801), ..spec(&["a", "b"]) }).starts_with("duration_s:"));
        assert_eq!(PollSpec { duration_s: None, ..spec(&["a", "b"]) }.cleaned().unwrap().duration_s, None);
    }

    #[test]
    fn one_vote_per_person_changes_allowed_and_nothing_after_the_end() {
        let mut p = Poll::start("p".into(), spec(&["A", "B", "C"]), Millis(0));
        assert_eq!(p.vote("v1", 1, Millis(1)), Ok(Voted::New));
        assert_eq!(p.vote("v1", 1, Millis(2)), Ok(Voted::Same));
        assert_eq!(p.vote("v1", 2, Millis(3)), Ok(Voted::Changed));
        assert_eq!(p.vote("v2", 2, Millis(4)), Ok(Voted::New));
        assert_eq!(p.vote("v3", 0, Millis(5)), Err(VoteRefused::NoSuchOption));
        assert_eq!(p.vote("v3", 4, Millis(5)), Err(VoteRefused::NoSuchOption));
        assert_eq!(p.vote("v3", usize::MAX, Millis(5)), Err(VoteRefused::NoSuchOption));
        assert_eq!(p.vote("v3", 3, Millis(60_000)), Err(VoteRefused::Ended), "the end time is exclusive");
        assert_eq!(p.tally(), vec![0, 2, 0]);
        let v = p.view();
        assert_eq!((v.total, v.leaders.clone()), (2, vec![1]));
        assert!(p.close());
        assert!(!p.close(), "a second close does nothing");
        assert_eq!(p.vote("v4", 1, Millis(10)), Err(VoteRefused::Ended), "closed early");
    }

    #[test]
    fn untimed_polls_stay_open_until_closed_and_combine_native_and_chat_tallies() {
        let spec_none = PollSpec { duration_s: None, ..spec(&["Yes", "No"]) };
        let mut p = Poll::start("u1".into(), spec_none, Millis(0));
        assert_eq!(p.ends_at, None);
        assert!(p.is_open(Millis(99_999_999)));
        assert!(p.announcement().ends_with("Open until closed."));
        p.set_platforms(vec![
            PlatformPollStatus {
                platform: Platform::Twitch,
                mode: PlatformPollMode::Native,
                external_id: Some("tw-1".into()),
                state: PlatformPollState::Active,
                note: None,
            },
            PlatformPollStatus {
                platform: Platform::Kick,
                mode: PlatformPollMode::Chat,
                external_id: None,
                state: PlatformPollState::Active,
                note: Some("platform_unsupported".into()),
            },
        ]);
        assert!(!p.allows_chat_vote(Platform::Twitch));
        assert!(p.allows_chat_vote(Platform::Kick));
        assert!(p.set_native_tally(Platform::Twitch, &[4, 2]));
        assert!(!p.set_native_tally(Platform::Twitch, &[4, 2]));
        p.vote("k1", 2, Millis(1_000_000)).unwrap();
        assert_eq!(p.tally(), vec![4, 3]);
        assert_eq!(p.view().total, 7);
        assert!(p.close());
        assert!(!p.is_open(Millis(1_000_001)));
        assert_eq!(p.view().platforms[0].state, PlatformPollState::Ended);
    }

    #[test]
    fn poll_spec_serde_distinguishes_omitted_null_and_seconds() {
        let omitted: PollSpec = serde_json::from_str(r#"{"question":"Q?","options":["A","B"]}"#).unwrap();
        assert_eq!(omitted.duration_s, Some(60));
        assert!(omitted.chat_fallback);
        let null_dur: PollSpec =
            serde_json::from_str(r#"{"question":"Q?","options":["A","B"],"duration_s":null,"chat_fallback":false}"#)
                .unwrap();
        assert_eq!(null_dur.duration_s, None);
        assert!(!null_dur.chat_fallback);
        let timed: PollSpec = serde_json::from_str(r#"{"question":"Q?","options":["A","B"],"duration_s":30}"#).unwrap();
        assert_eq!(timed.duration_s, Some(30));
    }

    #[test]
    fn results_and_announcements_read_well() {
        let mut p = Poll::start("p".into(), spec(&["Ascent", "Bind"]), Millis(0));
        assert_eq!(p.announcement(), "Poll: Which map? Type !vote 1 for Ascent, !vote 2 for Bind. You have 1 minute.");
        assert_eq!(p.result(), "Poll closed: Which map? Nobody voted.");
        p.vote("a", 1, Millis(1)).unwrap();
        p.vote("b", 1, Millis(1)).unwrap();
        p.vote("c", 2, Millis(1)).unwrap();
        assert_eq!(p.result(), "Poll closed: Which map? Winner: Ascent with 2 votes (67%).");
        p.vote("d", 2, Millis(1)).unwrap();
        assert_eq!(p.result(), "Poll closed: Which map? It's a tie between Ascent and Bind (2 votes each).");
        let short = Poll::start("s".into(), PollSpec { duration_s: Some(45), ..spec(&["a", "b"]) }, Millis(0));
        assert!(short.announcement().ends_with("You have 45 seconds."));
    }

    #[test]
    fn vote_parsing_is_strict() {
        assert_eq!(parse_vote("!vote 2"), Some(2));
        assert_eq!(parse_vote("!VOTE #3 because"), Some(3));
        assert_eq!(parse_vote("!vote"), None);
        assert_eq!(parse_vote("!vote two"), None);
        assert_eq!(parse_vote("!vote -1"), None);
        assert_eq!(parse_vote("!vote 999"), None);
        assert_eq!(parse_vote("I !vote 1"), None);
        assert_eq!(parse_vote("!voter 1"), None);
    }
}
