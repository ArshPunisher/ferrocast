//! Chat polls that work the same on every platform: viewers type `!vote 2`. No platform
//! poll API is used, so no extra permissions are needed.
//!
//! One vote per person (linked accounts count once); voting again changes the vote. A
//! vote after the end, or after the poll was closed early, is refused.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::event::Millis;

pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 5;
pub const MIN_DURATION_S: u32 = 10;
pub const MAX_DURATION_S: u32 = 30 * 60;
/// More voters than any real chat, so a bot flood can't grow memory without bound.
pub const MAX_VOTERS: usize = 200_000;

/// What the dashboard asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollSpec {
    pub question: String,
    pub options: Vec<String>,
    pub duration_s: u32,
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
        if !(MIN_DURATION_S..=MAX_DURATION_S).contains(&self.duration_s) {
            return Err("duration_s: polls run for 10 seconds to 30 minutes.".into());
        }
        Ok(PollSpec { question, options, duration_s: self.duration_s })
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
    pub ends_at: Millis,
    /// Closed early, or finished and announced.
    pub closed: bool,
    /// Voter key (one per person) → the option index, from 0.
    votes: HashMap<String, usize>,
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
    pub ends_at: Millis,
    pub closed: bool,
    /// Indexes (from 0) of the leading answers; several on a tie, none without votes.
    pub leaders: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionView {
    pub text: String,
    pub votes: u64,
}

impl Poll {
    pub fn start(id: String, spec: PollSpec, now: Millis) -> Poll {
        Poll {
            id,
            question: spec.question,
            options: spec.options,
            started_at: now,
            ends_at: Millis(now.0.saturating_add(i64::from(spec.duration_s) * 1000)),
            closed: false,
            votes: HashMap::new(),
        }
    }

    pub fn is_open(&self, now: Millis) -> bool {
        !self.closed && now < self.ends_at
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
        !std::mem::replace(&mut self.closed, true)
    }

    pub fn tally(&self) -> Vec<u64> {
        let mut t = vec![0u64; self.options.len()];
        for i in self.votes.values() {
            if let Some(slot) = t.get_mut(*i) {
                *slot += 1;
            }
        }
        t
    }

    pub fn view(&self) -> PollView {
        let tally = self.tally();
        let total: u64 = tally.iter().sum();
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
        }
    }

    /// The chat message that opens the poll.
    pub fn announcement(&self) -> String {
        let choices: Vec<String> =
            self.options.iter().enumerate().map(|(i, o)| format!("!vote {} for {o}", i + 1)).collect();
        let secs = (self.ends_at.0 - self.started_at.0).max(0) / 1000;
        let time = if secs >= 60 && secs % 60 == 0 {
            let m = secs / 60;
            format!("{m} minute{}", if m == 1 { "" } else { "s" })
        } else {
            format!("{secs} seconds")
        };
        format!("Poll: {} Type {}. You have {time}.", self.question, choices.join(", "))
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
            duration_s: 60,
        }
    }

    #[test]
    fn specs_are_cleaned_and_checked_with_field_names() {
        let s = PollSpec {
            question: "  Which\n map? ".into(),
            options: vec![" Ascent ".into(), "Bind".into()],
            duration_s: 60,
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
        assert!(err(PollSpec { duration_s: 9, ..spec(&["a", "b"]) }).starts_with("duration_s:"));
        assert!(err(PollSpec { duration_s: 1801, ..spec(&["a", "b"]) }).starts_with("duration_s:"));
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
        let short = Poll::start("s".into(), PollSpec { duration_s: 45, ..spec(&["a", "b"]) }, Millis(0));
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
