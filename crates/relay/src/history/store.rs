//! Stream history's tables (migration 3). One row per stream per platform, owned by the
//! subscription (`sub_id`), so the three PCs that share Pro share one history. Samples are
//! the viewer count every five minutes while live. Nothing about viewers or chat is stored,
//! only counts (R-51).

use rusqlite::{OptionalExtension, params};

use super::{Money, PlatformAmount, StreamIn, StreamOut};
use crate::db::Db;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS streams (id INTEGER PRIMARY KEY, sub_id TEXT NOT NULL, platform TEXT NOT NULL, key TEXT NOT NULL, install_id TEXT NOT NULL, started_at INTEGER NOT NULL, ended_at INTEGER, title TEXT, category TEXT, peak_viewers INTEGER NOT NULL DEFAULT 0, avg_viewers INTEGER NOT NULL DEFAULT 0, chat_lines INTEGER NOT NULL DEFAULT 0, chatters INTEGER NOT NULL DEFAULT 0, new_chatters INTEGER NOT NULL DEFAULT 0, follows INTEGER NOT NULL DEFAULT 0, subs INTEGER NOT NULL DEFAULT 0, tips TEXT NOT NULL DEFAULT '[]', platform_value TEXT NOT NULL DEFAULT '[]', created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, UNIQUE (sub_id, platform, key));
CREATE INDEX IF NOT EXISTS streams_sub_started ON streams(sub_id, started_at);
CREATE TABLE IF NOT EXISTS stream_samples (stream_id INTEGER NOT NULL REFERENCES streams(id) ON DELETE CASCADE, at INTEGER NOT NULL, viewers INTEGER NOT NULL, PRIMARY KEY (stream_id, at)) WITHOUT ROWID;
";

/// What an upsert did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Upserted {
    Created,
    Updated,
    /// The stream ended more than a day ago: it can't change any more.
    Locked,
}

/// What adding samples did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sampled {
    Saved(usize),
    NotFound,
    Locked,
    /// This sample's time is outside the stream (ms).
    OutOfRange(i64),
}

const COLS: &str = "platform, key, started_at, ended_at, title, category, peak_viewers, avg_viewers, chat_lines, \
                    chatters, new_chatters, follows, subs, tips, platform_value, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StreamOut> {
    let tips: String = r.get(13)?;
    let value: String = r.get(14)?;
    Ok(StreamOut {
        platform: r.get(0)?,
        key: r.get(1)?,
        started_at: r.get(2)?,
        ended_at: r.get(3)?,
        title: r.get(4)?,
        category: r.get(5)?,
        peak_viewers: r.get(6)?,
        avg_viewers: r.get(7)?,
        chat_lines: r.get(8)?,
        chatters: r.get(9)?,
        new_chatters: r.get(10)?,
        follows: r.get(11)?,
        subs: r.get(12)?,
        // Written by this module only; a damaged value reads as none rather than failing the list.
        tips: serde_json::from_str::<Vec<Money>>(&tips).unwrap_or_default(),
        platform_value: serde_json::from_str::<Vec<PlatformAmount>>(&value).unwrap_or_default(),
        updated_at: r.get(15)?,
    })
}

fn locked(ended_at: Option<i64>, now: i64) -> bool {
    ended_at.is_some_and(|e| now > e.saturating_add(super::LOCK_AFTER_MS))
}

impl Db {
    /// Adds a stream or replaces its totals (the same `platform` + `key` is the same stream).
    pub(crate) fn upsert_stream(&self, sub: &str, install: &str, s: &StreamIn, now: i64) -> rusqlite::Result<Upserted> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<Option<i64>> = tx
            .query_row(
                "SELECT ended_at FROM streams WHERE sub_id = ?1 AND platform = ?2 AND key = ?3",
                params![sub, s.platform, s.key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(ended) = existing
            && locked(ended, now)
        {
            return Ok(Upserted::Locked);
        }
        let tips = serde_json::to_string(&s.tips).unwrap_or_else(|_| "[]".into());
        let value = serde_json::to_string(&s.platform_value).unwrap_or_else(|_| "[]".into());
        tx.execute(
            "INSERT INTO streams (sub_id, platform, key, install_id, started_at, ended_at, title, category, peak_viewers,
               avg_viewers, chat_lines, chatters, new_chatters, follows, subs, tips, platform_value, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?18)
             ON CONFLICT (sub_id, platform, key) DO UPDATE SET install_id = excluded.install_id,
               started_at = excluded.started_at, ended_at = excluded.ended_at, title = excluded.title,
               category = excluded.category, peak_viewers = MAX(excluded.peak_viewers, streams.peak_viewers),
               avg_viewers = excluded.avg_viewers, chat_lines = excluded.chat_lines, chatters = excluded.chatters,
               new_chatters = excluded.new_chatters, follows = excluded.follows, subs = excluded.subs,
               tips = excluded.tips, platform_value = excluded.platform_value, updated_at = excluded.updated_at",
            params![
                sub,
                s.platform,
                s.key,
                install,
                s.started_at,
                s.ended_at,
                s.title,
                s.category,
                s.peak_viewers,
                s.avg_viewers,
                s.chat_lines,
                s.chatters,
                s.new_chatters,
                s.follows,
                s.subs,
                tips,
                value,
                now
            ],
        )?;
        tx.commit()?;
        Ok(if existing.is_some() { Upserted::Updated } else { Upserted::Created })
    }

    /// Adds viewer samples to a stream (the same time twice keeps the newer count). While
    /// the stream is live its peak and average follow the samples, so a stream whose end
    /// never arrived (the PC crashed) still has numbers.
    pub(crate) fn add_samples(
        &self,
        sub: &str,
        platform: &str,
        key: &str,
        samples: &[(i64, i64)],
        now: i64,
    ) -> rusqlite::Result<Sampled> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some((id, started_at, ended_at)) = tx
            .query_row(
                "SELECT id, started_at, ended_at FROM streams WHERE sub_id = ?1 AND platform = ?2 AND key = ?3",
                params![sub, platform, key],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?)),
            )
            .optional()?
        else {
            return Ok(Sampled::NotFound);
        };
        if locked(ended_at, now) {
            return Ok(Sampled::Locked);
        }
        // A minute of clock difference either side is fine; anything else isn't this stream.
        let earliest = started_at - 60_000;
        let latest = ended_at.unwrap_or(now) + 60_000;
        if let Some(&(at, _)) = samples.iter().find(|(at, _)| *at < earliest || *at > latest) {
            return Ok(Sampled::OutOfRange(at));
        }
        {
            let mut ins = tx.prepare(
                "INSERT INTO stream_samples (stream_id, at, viewers) VALUES (?1, ?2, ?3)
                 ON CONFLICT (stream_id, at) DO UPDATE SET viewers = excluded.viewers",
            )?;
            for (at, viewers) in samples {
                ins.execute(params![id, at, viewers])?;
            }
        }
        if ended_at.is_none() {
            tx.execute(
                "UPDATE streams SET
                   peak_viewers = MAX(peak_viewers, (SELECT COALESCE(MAX(viewers), 0) FROM stream_samples WHERE stream_id = ?1)),
                   avg_viewers = (SELECT CAST(ROUND(AVG(viewers)) AS INTEGER) FROM stream_samples WHERE stream_id = ?1),
                   updated_at = ?2
                 WHERE id = ?1",
                params![id, now],
            )?;
        }
        tx.commit()?;
        Ok(Sampled::Saved(samples.len()))
    }

    /// Streams that started in `[from, to)`, newest first, at most `limit`.
    pub(crate) fn list_streams(
        &self,
        sub: &str,
        from: i64,
        to: i64,
        platform: Option<&str>,
        limit: usize,
    ) -> rusqlite::Result<Vec<StreamOut>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM streams WHERE sub_id = ?1 AND started_at >= ?2 AND started_at < ?3
               AND (?4 IS NULL OR platform = ?4) ORDER BY started_at DESC, platform LIMIT ?5"
        ))?;
        let rows = st.query_map(params![sub, from, to, platform, i64::try_from(limit).unwrap_or(i64::MAX)], row)?;
        rows.collect()
    }

    /// One stream's viewer samples, oldest first (tests and later the per-stream chart).
    #[cfg(test)]
    pub(crate) fn stream_samples(&self, sub: &str, platform: &str, key: &str) -> rusqlite::Result<Vec<(i64, i64)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT s.at, s.viewers FROM stream_samples s JOIN streams t ON t.id = s.stream_id
             WHERE t.sub_id = ?1 AND t.platform = ?2 AND t.key = ?3 ORDER BY s.at",
        )?;
        let rows = st.query_map(params![sub, platform, key], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Whether this subscription has any history kept (export stays open after a lapse, R-04).
    pub(crate) fn has_history(&self, sub: &str) -> rusqlite::Result<bool> {
        self.c().query_row("SELECT EXISTS(SELECT 1 FROM streams WHERE sub_id = ?1)", [sub], |r| r.get(0))
    }

    /// Hourly housekeeping: history goes [`super::KEEP_AFTER_LAPSE_MS`] after a subscription
    /// stopped granting Pro (lapsed, refunded, or gone), and samples older than a year and a
    /// bit go for everyone (the longest range the dashboard shows is a year).
    pub(crate) fn prune_history(&self) -> rusqlite::Result<()> {
        let now = crate::now_ms();
        let subs: Vec<String> = {
            let c = self.c();
            let mut st = c.prepare("SELECT DISTINCT sub_id FROM streams")?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for sub in subs {
            let row = self.pro_sub(&sub)?;
            let keep = match &row {
                None => false,
                Some(s) => {
                    let access = s.access();
                    if crate::billing::access::is_pro(&access, now) {
                        true
                    } else {
                        // When Pro stopped: the end of access, else the refund, else the
                        // last time the relay heard about it.
                        let stopped =
                            crate::billing::access::access_until(&access).or(s.refunded_at).unwrap_or(s.synced_at);
                        now < stopped.saturating_add(super::KEEP_AFTER_LAPSE_MS)
                    }
                }
            };
            if !keep {
                let n = self.c().execute("DELETE FROM streams WHERE sub_id = ?1", [&sub])?;
                tracing::info!(streams = n, "stream history removed 30 days after Pro ended");
            }
        }
        self.c().execute("DELETE FROM stream_samples WHERE at < ?1", [now - super::SAMPLE_KEEP_MS])?;
        Ok(())
    }
}
