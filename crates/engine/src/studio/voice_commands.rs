//! Voice commands (plan item 5, 30 Oct): while live captions listen, a few plain phrases do
//! what they say. Read from what's being said now, once per sentence; a scene's name only once
//! the words have stopped changing, so "switch to just" never picks "Just chatting" early.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Viewers' messages read aloud lately (when, Unix ms; what): a speaker the mic hears must
/// never let a viewer give a voice command ("stop recording" in a tip message).
static READ_ALOUD: Mutex<VecDeque<(i64, String)>> = Mutex::new(VecDeque::new());
/// How long a message read aloud keeps its commands from working.
const READ_ALOUD_MS: i64 = 3 * 60_000;

/// A viewer's message is about to be read aloud (an alert's text to speech).
pub(crate) fn reading_aloud(text: &str, now: i64) {
    if let Ok(mut r) = READ_ALOUD.lock() {
        r.push_back((now, text.to_string()));
        while r.len() > 50 || r.front().is_some_and(|(t, _)| now - t > READ_ALOUD_MS) {
            r.pop_front();
        }
    }
}

/// Whether a viewer's message read aloud lately would itself give `c`: then it isn't the
/// streamer's voice asking (or can't be told apart), and it's ignored.
pub(crate) fn from_a_viewer(c: &Command, scenes: &[String], now: i64) -> bool {
    let Ok(r) = READ_ALOUD.lock() else { return true };
    r.iter()
        .filter(|(t, _)| now - t <= READ_ALOUD_MS)
        .any(|(_, text)| commands(text).contains(c) || scene_command(text, scenes).as_ref() == Some(c))
}

/// What a sentence asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Brb,
    Back,
    RecordOn,
    RecordOff,
    /// A scene by its exact name.
    Scene(String),
}

/// Lower case words, without punctuation or apostrophes ("I'm" is "im").
pub(crate) fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .replace(['\u{2019}', '\''], "")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn has(w: &[String], phrase: &[&str]) -> bool {
    w.windows(phrase.len()).any(|x| x.iter().zip(phrase).all(|(a, b)| a == b))
}

/// The commands in `said` (not scenes: see [`scene_command`]).
pub(crate) fn commands(said: &str) -> Vec<Command> {
    let w = words(said);
    let mut out = Vec::new();
    if has(&w, &["be", "right", "back"]) || has(&w, &["brb"]) {
        out.push(Command::Brb);
    } else if has(&w, &["im", "back"]) || has(&w, &["i", "am", "back"]) {
        out.push(Command::Back);
    }
    if has(&w, &["start", "recording"]) || has(&w, &["start", "the", "recording"]) {
        out.push(Command::RecordOn);
    } else if has(&w, &["stop", "recording"]) || has(&w, &["stop", "the", "recording"]) {
        out.push(Command::RecordOff);
    }
    out
}

/// "switch to <scene>" or "go to <scene>": the scene whose name's words the rest of the
/// sentence says best (all of them, in any order; the one with the fewest words left over).
/// None when nothing or more than one fits as well.
pub(crate) fn scene_command(said: &str, scenes: &[String]) -> Option<Command> {
    let w = words(said);
    let at = w.windows(2).position(|x| (x[0] == "switch" || x[0] == "go") && x[1] == "to")?;
    let rest: Vec<&String> = w[at + 2..].iter().filter(|x| *x != "the" && *x != "scene").collect();
    if rest.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &String)> = None;
    let mut tie = false;
    for s in scenes {
        let name = words(s);
        if name.is_empty() || !name.iter().filter(|n| *n != "the").all(|n| rest.contains(&n)) {
            // Or the scene's words beyond a brand prefix: every word said is in the name.
            if !rest.iter().all(|r| name.contains(r)) {
                continue;
            }
        }
        let extra = name.len().abs_diff(rest.len());
        match best {
            Some((e, _)) if e < extra => {}
            Some((e, _)) if e == extra => tie = true,
            _ => {
                best = Some((extra, s));
                tie = false;
            }
        }
    }
    if tie { None } else { best.map(|(_, s)| Command::Scene(s.clone())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_phrases_are_heard() {
        assert_eq!(commands("Okay guys, I'll be right back."), vec![Command::Brb]);
        assert_eq!(commands("BRB"), vec![Command::Brb]);
        assert_eq!(commands("Alright I'm back!"), vec![Command::Back]);
        assert_eq!(commands("I’m back"), vec![Command::Back]);
        assert_eq!(commands("start recording now"), vec![Command::RecordOn]);
        assert_eq!(commands("Stop the recording."), vec![Command::RecordOff]);
        // Near misses do nothing.
        assert!(commands("he came right back").is_empty());
        assert!(commands("my back hurts").is_empty());
        assert!(commands("I started recording yesterday").is_empty());
        assert!(commands("").is_empty());
    }

    #[test]
    fn scenes_are_found_by_their_words() {
        let scenes: Vec<String> =
            ["Main", "Just chatting", "Forge · In game", "Forge · Be right back", "Ending"].map(String::from).to_vec();
        let s = |t: &str| scene_command(t, &scenes);
        assert_eq!(s("switch to just chatting"), Some(Command::Scene("Just chatting".into())));
        assert_eq!(s("Go to the in game scene"), Some(Command::Scene("Forge · In game".into())));
        assert_eq!(s("switch to main"), Some(Command::Scene("Main".into())));
        assert_eq!(s("SWITCH TO ENDING."), Some(Command::Scene("Ending".into())));
        // Nothing said after it, a scene that isn't there, or no "switch to" at all.
        assert_eq!(s("switch to"), None);
        assert_eq!(s("switch to minecraft"), None);
        assert_eq!(s("just chatting"), None);
    }

    #[test]
    fn a_viewers_message_read_aloud_gives_no_commands() {
        let scenes: Vec<String> = ["Main", "Ending"].map(String::from).to_vec();
        let now = 1_000_000_000;
        reading_aloud("$5 from troll: switch to ending and stop recording lol", now);
        assert!(from_a_viewer(&Command::Scene("Ending".into()), &scenes, now + 10_000));
        assert!(from_a_viewer(&Command::RecordOff, &scenes, now + 10_000));
        // What the message didn't ask for still works, and the message stops counting after a while.
        assert!(!from_a_viewer(&Command::Brb, &scenes, now + 10_000));
        assert!(!from_a_viewer(&Command::RecordOff, &scenes, now + READ_ALOUD_MS + 1));
    }

    #[test]
    fn two_scenes_that_fit_as_well_pick_neither() {
        let scenes: Vec<String> = ["Game A", "Game B"].map(String::from).to_vec();
        assert_eq!(scene_command("switch to game", &scenes), None);
        assert_eq!(scene_command("switch to game b", &scenes), Some(Command::Scene("Game B".into())));
    }
}
