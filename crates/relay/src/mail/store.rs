//! The weekly email's tables (migration 9). One row per subscription with up to two
//! addresses, each sealed with the vault (`(key_v, nonce, ct)`, AAD `sub_id|mail_address`):
//! the **active** one (`on`, or `paused` when the provider refused it) that gets the report,
//! and a **pending** one waiting for its confirm link. Saving a new address fills only the
//! pending part, so the confirmed address keeps working until the new one is confirmed;
//! confirming swaps it in. The database never holds an address in plain text: next to each
//! sealed one is only its tag, a keyed hash (HMAC with the link key) that unsubscribe links
//! carry. `mail_sent` makes the weekly report at most once per subscription and week;
//! `mail_budget` counts the relay's emails per UTC day.

use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::vault::Sealed;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mail_addresses (sub_id TEXT PRIMARY KEY, key_v INTEGER, nonce BLOB, ct BLOB, addr_tag TEXT, status TEXT, confirmed_at INTEGER, paused_reason TEXT, p_key_v INTEGER, p_nonce BLOB, p_ct BLOB, p_addr_tag TEXT, confirm_hash TEXT, confirm_expires INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS mail_sent (sub_id TEXT NOT NULL, week INTEGER NOT NULL, sent_at INTEGER NOT NULL, PRIMARY KEY (sub_id, week)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS mail_budget (day INTEGER PRIMARY KEY, used INTEGER NOT NULL);
";

/// `mail_sent.sent_at` of a claimed week whose send hasn't finished (or crashed midway, or
/// whose answer was lost: it then counts as sent, so a restart never sends twice).
pub(crate) const CLAIMED: i64 = 0;
/// `mail_sent.sent_at` of a week with no streams: nothing was sent, on purpose. Looked at
/// again until the catch-up window ends (a late upload still counts).
pub(crate) const SKIPPED: i64 = -1;

/// One sealed address and its tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Addr {
    pub sealed: Sealed,
    /// HMAC of the normalized address with the link key: what unsubscribe links match.
    pub tag: String,
}

/// One subscription's addresses (still sealed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MailRow {
    pub sub_id: String,
    /// The address that gets the report (`status` `on`), or got it (`paused`).
    pub active: Option<Addr>,
    /// `on` or `paused`, with `active`.
    pub status: Option<String>,
    pub confirmed_at: Option<i64>,
    pub paused_reason: Option<String>,
    /// A new address waiting for its confirm link.
    pub pending: Option<Addr>,
    /// The newest confirm link's nonce hash (kept after the confirm, so pressing it again
    /// says "all set").
    pub confirm_hash: Option<String>,
    pub confirm_expires: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl MailRow {
    /// `on`, `paused`, `pending` (only a pending address) or `none`.
    pub fn view_status(&self) -> &str {
        match (&self.status, &self.pending) {
            (Some(s), _) if self.active.is_some() => s.as_str(),
            (_, Some(_)) => "pending",
            _ => "none",
        }
    }

    /// The active address, if it gets the report.
    pub fn on(&self) -> Option<&Addr> {
        self.active.as_ref().filter(|_| self.status.as_deref() == Some("on"))
    }
}

const COLS: &str = "sub_id, key_v, nonce, ct, addr_tag, status, confirmed_at, paused_reason, p_key_v, p_nonce, p_ct, p_addr_tag, confirm_hash, confirm_expires, created_at, updated_at";

fn addr(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Option<Addr>> {
    let key_v: Option<i64> = r.get(at)?;
    let nonce: Option<Vec<u8>> = r.get(at + 1)?;
    let ct: Option<Vec<u8>> = r.get(at + 2)?;
    let tag: Option<String> = r.get(at + 3)?;
    Ok(match (key_v, nonce, ct, tag) {
        (Some(key_v), Some(nonce), Some(ct), Some(tag)) => Some(Addr { sealed: Sealed { key_v, nonce, ct }, tag }),
        _ => None,
    })
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MailRow> {
    Ok(MailRow {
        sub_id: r.get(0)?,
        active: addr(r, 1)?,
        status: r.get(5)?,
        confirmed_at: r.get(6)?,
        paused_reason: r.get(7)?,
        pending: addr(r, 8)?,
        confirm_hash: r.get(12)?,
        confirm_expires: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

/// What a confirm click found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Confirmed {
    /// The pending address is the active one now.
    On,
    /// It was on already (the same link pressed twice).
    Already,
    Expired,
    /// No such pending link: used, replaced by a newer one, or removed.
    Unknown,
}

/// (has a pending address, status, confirm hash, confirm expiry).
type ConfirmState = (bool, Option<String>, Option<String>, Option<i64>);

const CLEAR_ACTIVE: &str =
    "key_v = NULL, nonce = NULL, ct = NULL, addr_tag = NULL, status = NULL, confirmed_at = NULL, paused_reason = NULL";
const CLEAR_PENDING: &str =
    "p_key_v = NULL, p_nonce = NULL, p_ct = NULL, p_addr_tag = NULL, confirm_hash = NULL, confirm_expires = NULL";
const EMPTY: &str = "ct IS NULL AND p_ct IS NULL";

impl Db {
    pub(crate) fn mail_row(&self, sub: &str) -> rusqlite::Result<Option<MailRow>> {
        self.c().query_row(&format!("SELECT {COLS} FROM mail_addresses WHERE sub_id = ?1"), [sub], row).optional()
    }

    /// Saves a new (or the same) address waiting for its confirm link; an older link stops
    /// working. A confirmed address stays on until this one is confirmed; a paused one is
    /// dropped (it gets nothing anyway: saving again starts over).
    pub(crate) fn mail_put_pending(
        &self,
        sub: &str,
        address: &Addr,
        confirm_hash: &str,
        expires: i64,
        now: i64,
    ) -> rusqlite::Result<()> {
        let s = &address.sealed;
        self.c().execute(
            "INSERT INTO mail_addresses (sub_id, p_key_v, p_nonce, p_ct, p_addr_tag, confirm_hash, confirm_expires, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT (sub_id) DO UPDATE SET p_key_v = excluded.p_key_v, p_nonce = excluded.p_nonce, p_ct = excluded.p_ct,
               p_addr_tag = excluded.p_addr_tag, confirm_hash = excluded.confirm_hash, confirm_expires = excluded.confirm_expires,
               updated_at = excluded.updated_at,
               key_v = CASE WHEN status = 'on' THEN key_v END, nonce = CASE WHEN status = 'on' THEN nonce END,
               ct = CASE WHEN status = 'on' THEN ct END, addr_tag = CASE WHEN status = 'on' THEN addr_tag END,
               confirmed_at = CASE WHEN status = 'on' THEN confirmed_at END, paused_reason = NULL,
               status = CASE WHEN status = 'on' THEN status END",
            params![sub, s.key_v, s.nonce, s.ct, address.tag, confirm_hash, expires, now],
        )?;
        Ok(())
    }

    /// Swaps the pending address in if `hash` is its current, unexpired confirm link
    /// (single use).
    pub(crate) fn mail_confirm(&self, sub: &str, hash: &str, now: i64) -> rusqlite::Result<Confirmed> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let found: Option<ConfirmState> = tx
            .query_row(
                "SELECT p_ct IS NOT NULL, status, confirm_hash, confirm_expires FROM mail_addresses WHERE sub_id = ?1",
                [sub],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((pending, status, stored, expires)) = found else { return Ok(Confirmed::Unknown) };
        use subtle::ConstantTimeEq;
        let same = stored.as_deref().is_some_and(|s| bool::from(s.as_bytes().ct_eq(hash.as_bytes())));
        if !same {
            return Ok(Confirmed::Unknown);
        }
        if !pending {
            return Ok(if status.as_deref() == Some("on") { Confirmed::Already } else { Confirmed::Unknown });
        }
        if expires.is_none_or(|e| now > e) {
            return Ok(Confirmed::Expired);
        }
        // The hash is kept so pressing the same link again says "already on"; it can't swap
        // anything in twice (the pending part is gone).
        tx.execute(
            "UPDATE mail_addresses SET key_v = p_key_v, nonce = p_nonce, ct = p_ct, addr_tag = p_addr_tag, status = 'on',
               confirmed_at = ?2, paused_reason = NULL, p_key_v = NULL, p_nonce = NULL, p_ct = NULL, p_addr_tag = NULL,
               confirm_expires = NULL, updated_at = ?2
             WHERE sub_id = ?1",
            params![sub, now],
        )?;
        tx.commit()?;
        Ok(Confirmed::On)
    }

    /// Removes both addresses (the dashboard's Stop emails). True if there was one.
    pub(crate) fn mail_delete(&self, sub: &str) -> rusqlite::Result<bool> {
        Ok(self.c().execute("DELETE FROM mail_addresses WHERE sub_id = ?1", [sub])? > 0)
    }

    /// An unsubscribe link: removes the address it was sent to (active or pending, matched
    /// by tag), never another one. True if something matched.
    pub(crate) fn mail_unsubscribe(&self, sub: &str, tag: &str) -> rusqlite::Result<bool> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let a = tx.execute(
            &format!("UPDATE mail_addresses SET {CLEAR_ACTIVE} WHERE sub_id = ?1 AND addr_tag = ?2"),
            params![sub, tag],
        )?;
        let p = tx.execute(
            &format!("UPDATE mail_addresses SET {CLEAR_PENDING} WHERE sub_id = ?1 AND p_addr_tag = ?2"),
            params![sub, tag],
        )?;
        tx.execute(&format!("DELETE FROM mail_addresses WHERE sub_id = ?1 AND {EMPTY}"), [sub])?;
        tx.commit()?;
        Ok(a + p > 0)
    }

    /// The provider refused this address (or it can't be read): no more mail until an
    /// address is saved again. Only the active address it was about.
    pub(crate) fn mail_pause(&self, sub: &str, tag: &str, reason: &str, now: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE mail_addresses SET status = 'paused', paused_reason = ?3, updated_at = ?4
             WHERE sub_id = ?1 AND addr_tag = ?2 AND status = 'on'",
            params![sub, tag, reason, now],
        )?;
        Ok(())
    }

    /// Every row whose active address gets the weekly report.
    pub(crate) fn mail_on_rows(&self) -> rusqlite::Result<Vec<MailRow>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM mail_addresses WHERE status = 'on' AND ct IS NOT NULL ORDER BY sub_id"
        ))?;
        let rows = st.query_map([], row)?;
        rows.collect()
    }

    /// Every subscription with an address (the lapse sweep).
    pub(crate) fn mail_subs(&self) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT sub_id FROM mail_addresses")?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    /// Claims (sub, week) with `sent_at`. `CLAIMED` also takes a week that was skipped (no
    /// streams then, some now). False when it was claimed or sent before, or skipped again.
    pub(crate) fn mail_claim(&self, sub: &str, week: i64, sent_at: i64) -> rusqlite::Result<bool> {
        Ok(self.c().execute(
            "INSERT INTO mail_sent (sub_id, week, sent_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (sub_id, week) DO UPDATE SET sent_at = excluded.sent_at
               WHERE mail_sent.sent_at = ?4 AND excluded.sent_at = ?5",
            params![sub, week, sent_at, SKIPPED, CLAIMED],
        )? > 0)
    }

    pub(crate) fn mail_mark_sent(&self, sub: &str, week: i64, at: i64) -> rusqlite::Result<()> {
        self.c()
            .execute("UPDATE mail_sent SET sent_at = ?3 WHERE sub_id = ?1 AND week = ?2", params![sub, week, at])?;
        Ok(())
    }

    /// A send that didn't go for a reason worth retrying: the next run tries again.
    pub(crate) fn mail_unclaim(&self, sub: &str, week: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "DELETE FROM mail_sent WHERE sub_id = ?1 AND week = ?2 AND sent_at = ?3",
            params![sub, week, CLAIMED],
        )?;
        Ok(())
    }

    /// `sent_at` for (sub, week), if claimed.
    pub(crate) fn mail_sent_at(&self, sub: &str, week: i64) -> rusqlite::Result<Option<i64>> {
        self.c()
            .query_row("SELECT sent_at FROM mail_sent WHERE sub_id = ?1 AND week = ?2", params![sub, week], |r| {
                r.get(0)
            })
            .optional()
    }

    /// The newest real send for a subscription (the dashboard's "last sent").
    pub(crate) fn mail_last_sent(&self, sub: &str) -> rusqlite::Result<Option<i64>> {
        self.c().query_row("SELECT MAX(sent_at) FROM mail_sent WHERE sub_id = ?1 AND sent_at > 0", [sub], |r| r.get(0))
    }

    /// Forgets everything about a subscription's email (30 days after Pro ended).
    pub(crate) fn mail_forget(&self, sub: &str) -> rusqlite::Result<()> {
        let c = self.c();
        c.execute("DELETE FROM mail_addresses WHERE sub_id = ?1", [sub])?;
        c.execute("DELETE FROM mail_sent WHERE sub_id = ?1", [sub])?;
        Ok(())
    }

    /// Pending addresses whose link expired before `before`: only the pending part goes (a
    /// confirmed address next to it stays). Rows left with no address go too.
    pub(crate) fn mail_drop_unconfirmed(&self, before: i64) -> rusqlite::Result<usize> {
        let c = self.c();
        let n = c.execute(
            &format!("UPDATE mail_addresses SET {CLEAR_PENDING} WHERE p_ct IS NOT NULL AND confirm_expires < ?1"),
            [before],
        )?;
        c.execute(&format!("DELETE FROM mail_addresses WHERE {EMPTY}"), [])?;
        Ok(n)
    }

    /// Weeks older than `before` aren't needed to stop a double send.
    pub(crate) fn mail_prune_sent(&self, before: i64) -> rusqlite::Result<usize> {
        self.c().execute("DELETE FROM mail_sent WHERE week < ?1", [before])
    }

    // ── The relay-wide daily budget (`day` = UTC days since 1970) ───────────────────────

    /// One more email today if it stays within `limit`; one statement, so two sends at once
    /// can't both squeeze past it.
    pub(crate) fn mail_budget_take(&self, day: i64, limit: i64) -> rusqlite::Result<bool> {
        if limit < 1 {
            return Ok(false);
        }
        Ok(self.c().execute(
            "INSERT INTO mail_budget (day, used) VALUES (?1, 1)
             ON CONFLICT (day) DO UPDATE SET used = used + 1 WHERE used + 1 <= ?2",
            params![day, limit],
        )? > 0)
    }

    /// An email that didn't go gives its place back.
    pub(crate) fn mail_budget_give_back(&self, day: i64) -> rusqlite::Result<()> {
        self.c().execute("UPDATE mail_budget SET used = MAX(0, used - 1) WHERE day = ?1", [day])?;
        Ok(())
    }

    /// The provider said its limit is reached: nothing more goes today.
    pub(crate) fn mail_budget_exhaust(&self, day: i64, limit: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO mail_budget (day, used) VALUES (?1, ?2) ON CONFLICT (day) DO UPDATE SET used = MAX(used, ?2)",
            params![day, limit.max(1)],
        )?;
        Ok(())
    }

    /// Emails counted against the budget on a UTC day.
    pub(crate) fn mail_budget_used(&self, day: i64) -> rusqlite::Result<i64> {
        Ok(self
            .c()
            .query_row("SELECT used FROM mail_budget WHERE day = ?1", [day], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }

    pub(crate) fn mail_budget_prune(&self, before_day: i64) -> rusqlite::Result<usize> {
        self.c().execute("DELETE FROM mail_budget WHERE day < ?1", [before_day])
    }
}
