//! The stream's transcript (plan item 5, 30 Oct): while live captions listen and the streamer
//! asked for it, each finished sentence is kept with its time, on this computer, one file a
//! day (`<data>/transcripts/YYYY-MM-DD.jsonl`), to search later ("where did I say giveaway?").

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Engine, EngineError};

/// Days searched back.
const DAYS: i64 = 30;
/// Answers at most.
const MAX_HITS: usize = 200;

/// One sentence said, and when (Unix ms).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub at: i64,
    pub text: String,
}

/// A sentence found, with where it falls in the stream it was said in (when live).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub at: i64,
    pub text: String,
}

fn day_file(dir: &Path, at: i64) -> Option<PathBuf> {
    // UTC days, so a file's name never depends on the computer's time zone.
    let (y, m, d) = ferrocast_core::chatbot::ymd(at);
    Some(dir.join(format!("{y:04}-{m:02}-{d:02}.jsonl")))
}

/// Appends lines to their day's file.
pub(crate) fn keep(dir: &Path, lines: &[Line]) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    for l in lines {
        let Some(file) = day_file(dir, l.at) else { continue };
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(file)?;
        writeln!(f, "{}", serde_json::to_string(l).unwrap_or_default())?;
    }
    Ok(())
}

/// Lines said since `since` (Unix ms) whose words hold every word of `query` (any case, in any
/// order), newest first. An empty query lists them all.
pub(crate) fn search(dir: &Path, query: &str, since: i64, now: i64) -> Vec<Hit> {
    let want = super::voice_commands::words(query);
    let mut hits = Vec::new();
    let mut day = now;
    let first = since.max(now - DAYS * 86_400_000);
    // Day by day, newest first, so the newest hits come first and old days stop the search.
    while day >= first - 86_400_000 {
        if let Some(file) = day_file(dir, day)
            && let Ok(body) = std::fs::read_to_string(file)
        {
            let mut found: Vec<Hit> = body
                .lines()
                .filter_map(|l| serde_json::from_str::<Line>(l).ok())
                .filter(|l| l.at >= since && l.at <= now)
                .filter(|l| {
                    let w = super::voice_commands::words(&l.text);
                    want.iter().all(|q| w.contains(q))
                })
                .map(|l| Hit { at: l.at, text: l.text })
                .collect();
            found.reverse();
            hits.extend(found);
            if hits.len() >= MAX_HITS {
                break;
            }
        }
        day -= 86_400_000;
    }
    hits.truncate(MAX_HITS);
    hits
}

impl Engine {
    pub(crate) fn transcript_dir(&self) -> Option<PathBuf> {
        self.golive_data_dir().map(|d| d.join("transcripts"))
    }

    /// Sentences said since `since` (default: this stream, else the last day) holding the
    /// words of `q`.
    pub fn transcript_search(&self, q: &str, since: Option<i64>) -> Result<Vec<Hit>, EngineError> {
        if q.chars().count() > 200 {
            return Err(EngineError::Invalid("q: 200 characters at most.".into()));
        }
        let now = crate::now().0;
        let since = since.or_else(|| self.stream_started_at()).unwrap_or(now - 86_400_000);
        if since > now {
            return Err(EngineError::Invalid("since: not in the future.".into()));
        }
        Ok(self.transcript_dir().map(|d| search(&d, q, since, now)).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    #[test]
    fn kept_lines_are_found_by_their_words() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_793_300_000_000; // 30 Oct 2026
        let lines = vec![
            Line { at: now - 2 * DAY, text: "Old giveaway talk".into() },
            Line { at: now - 60_000, text: "The giveaway ends at ten!".into() },
            Line { at: now - 30_000, text: "Thanks for the raid".into() },
            Line { at: now - 10_000, text: "Giveaway winner is Sam".into() },
        ];
        keep(dir.path(), &lines).unwrap();
        let hits = search(dir.path(), "GIVEAWAY", now - DAY, now);
        assert_eq!(
            hits.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(),
            ["Giveaway winner is Sam", "The giveaway ends at ten!"]
        );
        // Every word must be there; older days only when asked for.
        assert_eq!(search(dir.path(), "giveaway sam", now - DAY, now).len(), 1);
        assert_eq!(search(dir.path(), "giveaway", now - 3 * DAY, now).len(), 3);
        assert_eq!(search(dir.path(), "", now - DAY, now).len(), 3);
        assert!(search(dir.path(), "zebra", now - DAY, now).is_empty());
    }

    #[test]
    fn a_broken_line_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_793_300_000_000;
        keep(dir.path(), &[Line { at: now - 1000, text: "hello there".into() }]).unwrap();
        let f = day_file(dir.path(), now).unwrap();
        let mut body = std::fs::read_to_string(&f).unwrap();
        body.push_str("{not json\n");
        std::fs::write(&f, body).unwrap();
        assert_eq!(search(dir.path(), "hello", now - DAY, now).len(), 1);
    }
}
