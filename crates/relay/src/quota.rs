//! Pro's usage counters, per subscription (never per install: three PCs share one quota).
//! One row per `(sub_id, name, period)`: `name` is the counter (`tts_chars`, `backup_uploads`),
//! `period` the calendar day or month it counts in (UTC), `used` how much of it is gone.
//! Old periods are pruned with the hourly housekeeping.

use axum::http::{HeaderName, StatusCode};
use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::{ApiError, now_ms};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS quota_usage (sub_id TEXT NOT NULL, name TEXT NOT NULL, period TEXT NOT NULL, used INTEGER NOT NULL, reset_at INTEGER NOT NULL, PRIMARY KEY (sub_id, name, period));
CREATE INDEX IF NOT EXISTS quota_usage_reset ON quota_usage(reset_at);
";

/// Counters of periods that ended longer ago than this go.
pub const KEEP_MS: i64 = 62 * 86_400_000;

/// What a counter counts in. Calendar periods in UTC, so everyone's resets at the same time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// A UTC clock minute: request rates (`history_requests`), never shown as a quota.
    Minute,
    Day,
    Month,
}

/// (year, month 1–12, day 1–31) of a day number since 1970-01-01.
pub(crate) fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The day number of a date (the inverse of [`civil`]).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl Period {
    /// The period `now` falls in: `n2026-10-19T12:05`, `d2026-10-19` or `m2026-10`.
    pub fn key(self, now: i64) -> String {
        let (y, m, d) = civil(now.div_euclid(86_400_000));
        match self {
            Period::Minute => {
                let mins = now.rem_euclid(86_400_000) / 60_000;
                format!("n{y:04}-{m:02}-{d:02}T{:02}:{:02}", mins / 60, mins % 60)
            }
            Period::Day => format!("d{y:04}-{m:02}-{d:02}"),
            Period::Month => format!("m{y:04}-{m:02}"),
        }
    }

    /// When the period `now` falls in ends (ms): the next midnight, or the 1st of next month.
    pub fn reset_at(self, now: i64) -> i64 {
        let day = now.div_euclid(86_400_000);
        match self {
            Period::Minute => (now.div_euclid(60_000) + 1) * 60_000,
            Period::Day => (day + 1) * 86_400_000,
            Period::Month => {
                let (y, m, _) = civil(day);
                let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                days_from_civil(ny, nm, 1) * 86_400_000
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Period::Minute => "minute",
            Period::Day => "day",
            Period::Month => "month",
        }
    }
}

/// A take that would go over the limit: nothing was counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaErr {
    /// When the counter starts again (ms).
    pub reset_at: i64,
}

impl QuotaErr {
    /// 429 with the feature's own code, `X-Quota-Reset` (ms, like every time in the API)
    /// and `Retry-After` (seconds).
    pub fn into_api(self, code: &'static str, message: impl Into<String>) -> ApiError {
        let wait_s = u64::try_from(self.reset_at - now_ms()).unwrap_or(0).div_ceil(1000);
        ApiError::new(StatusCode::TOO_MANY_REQUESTS, code, message)
            .hint(format!("It starts again on {} (UTC).", crate::billing::date(self.reset_at)))
            .header(HeaderName::from_static("x-quota-reset"), self.reset_at.to_string())
            .header(axum::http::header::RETRY_AFTER, wait_s.to_string())
    }
}

/// One counter's state for `/v1/pro/status` `quotas`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Usage {
    pub used: i64,
    pub limit: i64,
    pub period: &'static str,
    pub reset_at: i64,
}

/// A counter a feature declares: its name, what it counts in, and the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub name: &'static str,
    pub period: Period,
    pub limit: i64,
}

impl Db {
    /// Counts `amount` against a subscription's counter if it stays within `limit`; one
    /// statement, so two requests at once can't both squeeze past it.
    pub fn try_take(&self, sub: &str, name: &str, period: Period, amount: i64, limit: i64) -> Result<(), TakeErr> {
        self.try_take_at(sub, name, period, amount, limit, now_ms())
    }

    pub(crate) fn try_take_at(
        &self,
        sub: &str,
        name: &str,
        period: Period,
        amount: i64,
        limit: i64,
        now: i64,
    ) -> Result<(), TakeErr> {
        let reset_at = period.reset_at(now);
        if amount < 0 || amount > limit {
            return Err(TakeErr::Over(QuotaErr { reset_at }));
        }
        let changed = self.c().execute(
            "INSERT INTO quota_usage(sub_id, name, period, used, reset_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(sub_id, name, period) DO UPDATE SET used = used + excluded.used
               WHERE used + excluded.used <= ?6",
            params![sub, name, period.key(now), amount, reset_at, limit],
        )?;
        if changed == 0 { Err(TakeErr::Over(QuotaErr { reset_at })) } else { Ok(()) }
    }

    /// Gives back what a failed request took (the provider refused, the upload broke).
    pub fn give_back(&self, sub: &str, name: &str, period: Period, amount: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE quota_usage SET used = MAX(0, used - ?4) WHERE sub_id = ?1 AND name = ?2 AND period = ?3",
            params![sub, name, period.key(now_ms()), amount],
        )?;
        Ok(())
    }

    pub fn quota_used(&self, sub: &str, name: &str, period: Period) -> rusqlite::Result<i64> {
        self.quota_used_at(sub, name, period, now_ms())
    }

    pub(crate) fn quota_used_at(&self, sub: &str, name: &str, period: Period, now: i64) -> rusqlite::Result<i64> {
        Ok(self
            .c()
            .query_row(
                "SELECT used FROM quota_usage WHERE sub_id = ?1 AND name = ?2 AND period = ?3",
                params![sub, name, period.key(now)],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// A counter's state now, for the status answer.
    pub(crate) fn usage(&self, sub: &str, l: &Limit) -> rusqlite::Result<Usage> {
        let now = now_ms();
        Ok(Usage {
            used: self.quota_used_at(sub, l.name, l.period, now)?,
            limit: l.limit,
            period: l.period.label(),
            reset_at: l.period.reset_at(now),
        })
    }

    pub(crate) fn prune_quotas(&self) -> rusqlite::Result<()> {
        let now = now_ms();
        let c = self.c();
        c.execute("DELETE FROM quota_usage WHERE reset_at < ?1", [now - KEEP_MS])?;
        // Request-rate minutes are only worth anything while they last.
        c.execute("DELETE FROM quota_usage WHERE period LIKE 'n%' AND reset_at < ?1", [now - 3_600_000])?;
        Ok(())
    }
}

/// Why a take didn't count.
#[derive(Debug)]
pub enum TakeErr {
    Over(QuotaErr),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for TakeErr {
    fn from(e: rusqlite::Error) -> Self {
        TakeErr::Db(e)
    }
}

impl TakeErr {
    /// The API error: 429 with `code` when over, 503 `storage` when the database failed.
    pub fn into_api(self, code: &'static str, message: impl Into<String>) -> ApiError {
        match self {
            TakeErr::Over(q) => q.into_api(code, message),
            TakeErr::Db(e) => ApiError::db(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;

    use super::*;

    // 2026-10-19 12:00 UTC.
    const NOW: i64 = 1_792_411_200_000;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("relay.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn periods_are_utc_calendar_days_and_months() {
        assert_eq!(Period::Day.key(NOW), "d2026-10-19");
        assert_eq!(Period::Month.key(NOW), "m2026-10");
        assert_eq!(Period::Minute.key(NOW + 5 * 60_000 + 59_999), "n2026-10-19T12:05");
        assert_eq!(Period::Minute.reset_at(NOW + 1), NOW + 60_000, "the next minute");
        assert_eq!(Period::Day.reset_at(NOW), 1_792_454_400_000, "midnight on the 20th");
        assert_eq!(Period::Month.reset_at(NOW), 1_793_491_200_000, "1 Nov 2026");
        // December rolls into January of the next year.
        let dec = 1_797_681_600_000; // 2026-12-19 12:00 UTC
        assert_eq!(Period::Month.key(dec), "m2026-12");
        assert_eq!(Period::Month.reset_at(dec), 1_798_761_600_000, "1 Jan 2027");
        for days in [-1, 0, 59, 365, 10_957, 20_745, 30_000] {
            let (y, m, d) = civil(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
    }

    #[test]
    fn takes_count_up_to_the_limit_and_never_past_it() {
        let (_d, db) = db();
        let take = |n| db.try_take_at("sub_1", "tts_chars", Period::Month, n, 100, NOW);
        assert!(take(60).is_ok());
        assert!(take(40).is_ok(), "exactly the limit is fine");
        let Err(TakeErr::Over(q)) = take(1) else { panic!("over the limit") };
        assert_eq!(q.reset_at, Period::Month.reset_at(NOW));
        assert_eq!(
            db.quota_used_at("sub_1", "tts_chars", Period::Month, NOW).unwrap(),
            100,
            "a refusal counts nothing"
        );
        // Other subscriptions, counters and periods count on their own.
        assert!(db.try_take_at("sub_2", "tts_chars", Period::Month, 100, 100, NOW).is_ok());
        assert!(db.try_take_at("sub_1", "uploads", Period::Month, 1, 100, NOW).is_ok());
        let next_month = Period::Month.reset_at(NOW) + 1;
        assert!(db.try_take_at("sub_1", "tts_chars", Period::Month, 100, 100, next_month).is_ok());
        // One take bigger than the whole limit, or a negative one, never counts.
        assert!(db.try_take_at("sub_3", "tts_chars", Period::Month, 101, 100, NOW).is_err());
        assert!(db.try_take_at("sub_3", "tts_chars", Period::Month, -5, 100, NOW).is_err());
        assert_eq!(db.quota_used_at("sub_3", "tts_chars", Period::Month, NOW).unwrap(), 0);
    }

    #[test]
    fn giving_back_never_goes_below_zero() {
        let (_d, db) = db();
        db.try_take("sub_1", "uploads", Period::Day, 3, 6).unwrap();
        db.give_back("sub_1", "uploads", Period::Day, 2).unwrap();
        assert_eq!(db.quota_used("sub_1", "uploads", Period::Day).unwrap(), 1);
        db.give_back("sub_1", "uploads", Period::Day, 9).unwrap();
        assert_eq!(db.quota_used("sub_1", "uploads", Period::Day).unwrap(), 0);
    }

    #[test]
    fn old_periods_are_pruned() {
        let (_d, db) = db();
        let long_ago = now_ms() - KEEP_MS - 40 * 86_400_000;
        db.try_take_at("sub_1", "uploads", Period::Month, 1, 6, long_ago).unwrap();
        db.try_take("sub_1", "uploads", Period::Month, 1, 6).unwrap();
        db.prune().unwrap();
        let rows: i64 = db.c().query_row("SELECT COUNT(*) FROM quota_usage", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1, "only this month's counter is left");
    }

    #[test]
    fn over_quota_is_a_429_with_the_reset_time() {
        let reset_at = now_ms() + 90_500;
        let res =
            QuotaErr { reset_at }.into_api("tts_quota", "You've used this month's voice characters.").into_response();
        assert_eq!(res.status(), 429);
        assert_eq!(res.headers()["x-quota-reset"], reset_at.to_string().as_str());
        let wait: i64 = res.headers()["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((90..=91).contains(&wait), "{wait}");
    }
}
