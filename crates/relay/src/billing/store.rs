//! Pro's tables in the relay database. Minimal by design (R-51): the Razorpay
//! subscription id, plan, status and period end; a hash of the email given at checkout
//! (only to let the same person move Pro to a new install); which installs share it; the
//! payment ids and amounts of its charges (to tell a full refund); handled webhook ids.
//! Never a card, UPI id, name, phone or the email itself.

use rusqlite::{OptionalExtension, params};

use super::access::SubState;
use crate::db::Db;
use crate::now_ms;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS pro_subs (id TEXT PRIMARY KEY, plan TEXT NOT NULL, email_hash TEXT NOT NULL, status TEXT NOT NULL, current_end INTEGER, cancel_at_end INTEGER NOT NULL DEFAULT 0, refunded_at INTEGER, created_at INTEGER NOT NULL, synced_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pro_installs (install_id TEXT PRIMARY KEY REFERENCES installs(id) ON DELETE CASCADE, sub_id TEXT NOT NULL REFERENCES pro_subs(id), linked_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS pro_installs_sub ON pro_installs(sub_id, linked_at);
CREATE TABLE IF NOT EXISTS pro_payments (payment_id TEXT PRIMARY KEY, sub_id TEXT NOT NULL REFERENCES pro_subs(id), amount INTEGER NOT NULL, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS billing_events (event_id TEXT PRIMARY KEY, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pro_manage (token_hash TEXT PRIMARY KEY, sub_id TEXT NOT NULL REFERENCES pro_subs(id), expires_at INTEGER NOT NULL);
";

/// Every subscription an install was ever linked to (`pro_installs` keeps only the current
/// one), so a new subscription bought from the same install can take over a lapsed one's data
/// within the keep window (`carry`). Filled from `pro_installs` when it's created.
pub(crate) const SCHEMA_LINKS: &str = "
CREATE TABLE IF NOT EXISTS pro_install_subs (install_id TEXT NOT NULL, sub_id TEXT NOT NULL, linked_at INTEGER NOT NULL, PRIMARY KEY (install_id, sub_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS pro_install_subs_sub ON pro_install_subs(sub_id);
INSERT OR IGNORE INTO pro_install_subs(install_id, sub_id, linked_at) SELECT install_id, sub_id, linked_at FROM pro_installs;
";

/// Installs that can share one subscription (a streaming PC, a laptop, a spare).
pub const MAX_INSTALLS: i64 = 3;
/// Handled webhook ids are kept this long (Razorpay retries for about a day).
pub const EVENT_KEEP_MS: i64 = 30 * 86_400_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubRow {
    pub id: String,
    pub plan: String,
    pub email_hash: String,
    pub status: String,
    pub current_end: Option<i64>,
    pub cancel_at_end: bool,
    pub refunded_at: Option<i64>,
    pub synced_at: i64,
}

impl SubRow {
    pub fn access(&self) -> SubState {
        SubState {
            status: self.status.clone(),
            current_end: self.current_end,
            cancel_at_end: self.cancel_at_end,
            refunded: self.refunded_at.is_some(),
        }
    }

    /// When this subscription stopped granting Pro (ms), for the "kept 30 days after Pro
    /// ended" rules: the end of its access; a refund's moment; for one that lost access
    /// without an end (`halted`, `paused`) the paid period plus the late-renewal grace, the
    /// longest a renewal being retried could have kept it. Never the last sync time: the daily
    /// re-check syncs halted subscriptions every day, which would keep their data forever.
    /// Only a subscription that was never charged (no period) falls back to that.
    pub fn stopped_at(&self) -> i64 {
        super::access::access_until(&self.access())
            .or(self.refunded_at)
            .or(self.current_end.map(|end| end.saturating_add(super::access::GRACE_MS)))
            .unwrap_or(self.synced_at)
    }
}

/// Razorpay's view of a subscription, as the relay copies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Synced {
    pub status: String,
    pub current_end: Option<i64>,
    /// `Some` when this change says it (a cancel request); `None` keeps the stored flag.
    pub cancel_at_end: Option<bool>,
}

/// How applying a webhook went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Done,
    /// This event id was handled before; nothing changed.
    Duplicate,
}

const COLS: &str = "id, plan, email_hash, status, current_end, cancel_at_end, refunded_at, synced_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SubRow> {
    Ok(SubRow {
        id: r.get(0)?,
        plan: r.get(1)?,
        email_hash: r.get(2)?,
        status: r.get(3)?,
        current_end: r.get(4)?,
        cancel_at_end: r.get::<_, i64>(5)? != 0,
        refunded_at: r.get(6)?,
        synced_at: r.get(7)?,
    })
}

/// Links `install` to `sub` (moving it off any other), keeping at most [`MAX_INSTALLS`]:
/// the ones linked longest ago drop off first. The link is also kept in `pro_install_subs`.
pub(super) fn link(tx: &rusqlite::Transaction<'_>, install: &str, sub: &str, now: i64) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO pro_install_subs(install_id, sub_id, linked_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(install_id, sub_id) DO UPDATE SET linked_at = excluded.linked_at",
        params![install, sub, now],
    )?;
    tx.execute(
        "INSERT INTO pro_installs(install_id, sub_id, linked_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(install_id) DO UPDATE SET sub_id = excluded.sub_id, linked_at = excluded.linked_at",
        params![install, sub, now],
    )?;
    tx.execute(
        "DELETE FROM pro_installs WHERE sub_id = ?1 AND install_id NOT IN
           (SELECT install_id FROM pro_installs WHERE sub_id = ?1 ORDER BY linked_at DESC, install_id LIMIT ?2)",
        params![sub, MAX_INSTALLS],
    )?;
    Ok(())
}

fn write_sync(tx: &rusqlite::Transaction<'_>, id: &str, s: &Synced, now: i64) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE pro_subs SET status = ?2, current_end = COALESCE(?3, current_end),
           cancel_at_end = COALESCE(?4, cancel_at_end), synced_at = ?5 WHERE id = ?1",
        params![id, s.status, s.current_end, s.cancel_at_end.map(i64::from), now],
    )?;
    Ok(())
}

impl Db {
    /// A new checkout: the subscription Razorpay just made, linked to the install that
    /// asked for it.
    pub fn add_pro_sub(
        &self,
        id: &str,
        plan: &str,
        email_hash: &str,
        status: &str,
        install: &str,
    ) -> rusqlite::Result<()> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO pro_subs(id, plan, email_hash, status, created_at, synced_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(id) DO NOTHING",
            params![id, plan, email_hash, status, now],
        )?;
        link(&tx, install, id, now)?;
        tx.commit()
    }

    pub fn pro_sub(&self, id: &str) -> rusqlite::Result<Option<SubRow>> {
        self.c().query_row(&format!("SELECT {COLS} FROM pro_subs WHERE id = ?1"), [id], row).optional()
    }

    pub fn pro_sub_for_install(&self, install: &str) -> rusqlite::Result<Option<SubRow>> {
        self.c()
            .query_row(
                &format!(
                    "SELECT {COLS} FROM pro_subs WHERE id = (SELECT sub_id FROM pro_installs WHERE install_id = ?1)"
                ),
                [install],
                row,
            )
            .optional()
    }

    /// Moves `install` onto `sub` (restoring Pro on a new install).
    pub fn link_pro_install(&self, install: &str, sub: &str) -> rusqlite::Result<()> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        link(&tx, install, sub, now_ms())?;
        tx.commit()
    }

    pub fn pro_installs(&self, sub: &str) -> rusqlite::Result<i64> {
        self.c().query_row("SELECT COUNT(*) FROM pro_installs WHERE sub_id = ?1", [sub], |r| r.get(0))
    }

    /// Copies Razorpay's view. With an event id, records it in the same transaction, so a
    /// failure leaves the event unrecorded for Razorpay's retry, and a second delivery of
    /// the same event changes nothing.
    pub fn sync_pro_sub(&self, event: Option<&str>, id: &str, s: &Synced) -> rusqlite::Result<Applied> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        if let Some(e) = event
            && tx.execute("INSERT OR IGNORE INTO billing_events(event_id, at) VALUES (?1, ?2)", params![e, now])? == 0
        {
            return Ok(Applied::Duplicate);
        }
        write_sync(&tx, id, s, now)?;
        tx.commit()?;
        Ok(Applied::Done)
    }

    /// A charge: its payment id and amount (so a later refund can be matched), plus the
    /// subscription's new state, all with the event id.
    pub fn record_pro_charge(
        &self,
        event: &str,
        id: &str,
        payment: Option<(&str, i64)>,
        s: &Synced,
    ) -> rusqlite::Result<Applied> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        if tx.execute("INSERT OR IGNORE INTO billing_events(event_id, at) VALUES (?1, ?2)", params![event, now])? == 0 {
            return Ok(Applied::Duplicate);
        }
        if let Some((pay, amount)) = payment {
            tx.execute(
                "INSERT OR IGNORE INTO pro_payments(payment_id, sub_id, amount, at) VALUES (?1, ?2, ?3, ?4)",
                params![pay, id, amount, now],
            )?;
        }
        write_sync(&tx, id, s, now)?;
        tx.commit()?;
        Ok(Applied::Done)
    }

    /// The subscription and amount of one of Pro's payments.
    pub fn pro_payment(&self, payment: &str) -> rusqlite::Result<Option<(String, i64)>> {
        self.c()
            .query_row("SELECT sub_id, amount FROM pro_payments WHERE payment_id = ?1", [payment], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
    }

    /// A full refund: Pro ends now, whatever Razorpay's status says.
    pub fn refund_pro_sub(&self, event: Option<&str>, id: &str, s: Option<&Synced>) -> rusqlite::Result<Applied> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        if let Some(e) = event
            && tx.execute("INSERT OR IGNORE INTO billing_events(event_id, at) VALUES (?1, ?2)", params![e, now])? == 0
        {
            return Ok(Applied::Duplicate);
        }
        tx.execute("UPDATE pro_subs SET refunded_at = COALESCE(refunded_at, ?2) WHERE id = ?1", params![id, now])?;
        if let Some(s) = s {
            write_sync(&tx, id, s, now)?;
        }
        tx.commit()?;
        Ok(Applied::Done)
    }

    /// Records an event that needs no change (not ours, or nothing to do), so a retry
    /// is answered at once. `true` if it wasn't recorded before.
    pub fn note_billing_event(&self, event: &str) -> rusqlite::Result<bool> {
        Ok(self
            .c()
            .execute("INSERT OR IGNORE INTO billing_events(event_id, at) VALUES (?1, ?2)", params![event, now_ms()])?
            > 0)
    }

    pub fn billing_event_seen(&self, event: &str) -> rusqlite::Result<bool> {
        Ok(self
            .c()
            .query_row("SELECT 1 FROM billing_events WHERE event_id = ?1", [event], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// A manage-page link for one subscription.
    pub fn put_pro_manage(&self, token_hash: &str, sub: &str, expires_at: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO pro_manage(token_hash, sub_id, expires_at) VALUES (?1, ?2, ?3)",
            params![token_hash, sub, expires_at],
        )?;
        Ok(())
    }

    /// The subscription a manage-page link opens, while it's valid.
    pub fn pro_manage(&self, token_hash: &str) -> rusqlite::Result<Option<String>> {
        self.c()
            .query_row(
                "SELECT sub_id FROM pro_manage WHERE token_hash = ?1 AND expires_at > ?2",
                params![token_hash, now_ms()],
                |r| r.get(0),
            )
            .optional()
    }

    /// Subscriptions that aren't finished, for the daily re-check with Razorpay.
    pub fn open_pro_subs(&self) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id FROM pro_subs WHERE status IN ('created','authenticated','active','pending','halted','paused')
               AND refunded_at IS NULL AND id NOT LIKE 'comp\\_%' ESCAPE '\\'",
        )?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    /// Recent payments, for the daily refund re-check (a refund whose webhooks were all
    /// lost still ends Pro).
    pub fn recent_pro_payments(&self, since: i64) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT payment_id FROM pro_payments WHERE at >= ?1")?;
        let rows = st.query_map([since], |r| r.get(0))?;
        rows.collect()
    }

    pub(crate) fn prune_billing(&self) -> rusqlite::Result<()> {
        let now = now_ms();
        let c = self.c();
        c.execute("DELETE FROM billing_events WHERE at < ?1", [now - EVENT_KEEP_MS])?;
        c.execute("DELETE FROM pro_manage WHERE expires_at < ?1", [now])?;
        // Link history for installs or subscriptions that are gone.
        c.execute(
            "DELETE FROM pro_install_subs WHERE install_id NOT IN (SELECT id FROM installs)
               OR sub_id NOT IN (SELECT id FROM pro_subs)",
            [],
        )?;
        // Checkouts nobody paid for (Razorpay expires them after `expire_by`).
        c.execute(
            "DELETE FROM pro_installs WHERE sub_id IN (SELECT id FROM pro_subs WHERE status = 'created' AND created_at < ?1)",
            [now - 7 * 86_400_000],
        )?;
        c.execute(
            "DELETE FROM pro_subs WHERE status = 'created' AND created_at < ?1
               AND id NOT IN (SELECT sub_id FROM pro_payments) AND id NOT IN (SELECT sub_id FROM pro_manage)",
            [now - 7 * 86_400_000],
        )?;
        Ok(())
    }
}
