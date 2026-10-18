//! The relay-wide character counter that keeps Ferrocast voices inside Google's free tier:
//! one row per calendar month (UTC), `scope` `all`. Only counts: never text, voices or audio.
//! Rows are tiny (one a month) and kept, so the owner can see past months' use.

use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::quota::Period;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tts_usage (scope TEXT NOT NULL, month TEXT NOT NULL, chars INTEGER NOT NULL, requests INTEGER NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY (scope, month));
";

const ALL: &str = "all";

impl Db {
    /// Counts `chars` against this month's relay-wide cap if they fit; one statement, so two
    /// requests at once can't both squeeze past it. Returns the month it counted in (for a
    /// give-back that lands in the same month), or `None` when it would go over.
    pub(crate) fn tts_take_global(&self, chars: i64, cap: i64, now: i64) -> rusqlite::Result<Option<String>> {
        let month = Period::Month.key(now);
        if chars <= 0 || chars > cap {
            return Ok(None);
        }
        let changed = self.c().execute(
            "INSERT INTO tts_usage(scope, month, chars, requests, updated_at) VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(scope, month) DO UPDATE SET chars = chars + excluded.chars, requests = requests + 1,
               updated_at = excluded.updated_at
               WHERE chars + excluded.chars <= ?5",
            params![ALL, month, chars, now, cap],
        )?;
        Ok((changed > 0).then_some(month))
    }

    /// Takes back what a request Google never worked on took.
    pub(crate) fn tts_give_back_global(&self, month: &str, chars: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE tts_usage SET chars = MAX(0, chars - ?3), requests = MAX(0, requests - 1)
             WHERE scope = ?1 AND month = ?2",
            params![ALL, month, chars],
        )?;
        Ok(())
    }

    /// Characters counted relay-wide this month.
    pub(crate) fn tts_global_used(&self, now: i64) -> rusqlite::Result<i64> {
        Ok(self
            .c()
            .query_row(
                "SELECT chars FROM tts_usage WHERE scope = ?1 AND month = ?2",
                params![ALL, Period::Month.key(now)],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }
}
