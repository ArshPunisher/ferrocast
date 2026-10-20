//! Pro codes the owner makes in the Control Center:
//!
//! - `grant`: free Pro for N days. The app redeems it (`POST /v1/pro/redeem`); it becomes a
//!   comp grant (`comp`) labelled with the code.
//! - `discount`: a percent off the list price for buyers who enter it at checkout. Razorpay
//!   bills it through a plan made for that amount, so the buyer keeps the price on renewals.
//!
//! Codes are upper-case letters, digits and dashes, matched case-blind.

use rusqlite::{OptionalExtension, params};

use super::comp::{self, CompError, GrantBy, Granted};
use crate::db::Db;

pub const KINDS: [&str; 2] = ["grant", "discount"];
/// The alphabet generated codes use (no 0/O, 1/I/L).
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRow {
    pub code: String,
    pub kind: String,
    /// `grant`: days of Pro.
    pub days: Option<i64>,
    /// `discount`: percent off.
    pub percent_off: Option<i64>,
    /// `discount`: `monthly`, `yearly`, or `None` for both.
    pub applies_period: Option<String>,
    pub max_uses: Option<i64>,
    pub valid_until: Option<i64>,
    pub active: bool,
    pub note: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    /// Redeemed (grant) or bought with (discount; unpaid checkouts older than two days don't count).
    pub used: i64,
}

impl CodeRow {
    /// Why it can't be used now, if it can't.
    pub fn refusal(&self, now: i64) -> Option<CodeRefusal> {
        if !self.active {
            Some(CodeRefusal::NotFound)
        } else if self.valid_until.is_some_and(|u| u <= now) {
            Some(CodeRefusal::Expired)
        } else if self.max_uses.is_some_and(|m| self.used >= m) {
            Some(CodeRefusal::UsedUp)
        } else {
            None
        }
    }

    /// Whether a discount code applies to this plan.
    pub fn applies_to(&self, period: &str) -> bool {
        self.applies_period.as_deref().is_none_or(|p| p == period)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeRefusal {
    /// Unknown or switched off.
    NotFound,
    Expired,
    UsedUp,
}

/// Why redeeming didn't work.
#[derive(Debug)]
pub enum RedeemError {
    Refused(CodeRefusal),
    /// This install redeemed it before.
    AlreadyRedeemed,
    /// It's a discount code: it goes with a checkout, not here.
    Discount(Box<CodeRow>),
    Comp(CompError),
}

impl From<rusqlite::Error> for RedeemError {
    fn from(e: rusqlite::Error) -> Self {
        RedeemError::Comp(CompError::Db(e))
    }
}

/// Upper-cased and trimmed; `None` if it has anything but letters, digits and dashes, or
/// isn't 4 to 32 characters.
pub fn normalize(code: &str) -> Option<String> {
    let c = code.trim().to_ascii_uppercase();
    ((4..=32).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')).then_some(c)
}

/// `prefix` plus eight random characters.
pub fn generate(prefix: &str) -> String {
    let mut b = [0u8; 8];
    // Without OS randomness nothing secure works; same as `crate::random`.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    let tail: String = b.iter().map(|x| ALPHABET[usize::from(*x) % ALPHABET.len()] as char).collect();
    format!("{}{tail}", prefix.trim().to_ascii_uppercase())
}

const COLS: &str = "c.code, c.kind, c.days, c.percent_off, c.applies_period, c.max_uses, c.valid_until, c.active, c.note, \
                    c.created_by, c.created_at";

/// How many times a code counts as used (see [`CodeRow::used`]).
const USED: &str = "(SELECT COUNT(*) FROM pro_code_uses u WHERE u.code = c.code AND (c.kind = 'grant' OR EXISTS (
      SELECT 1 FROM pro_subs s WHERE s.id = u.sub_id AND (s.status <> 'created' OR s.created_at > ?1))))";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CodeRow> {
    Ok(CodeRow {
        code: r.get(0)?,
        kind: r.get(1)?,
        days: r.get(2)?,
        percent_off: r.get(3)?,
        applies_period: r.get(4)?,
        max_uses: r.get(5)?,
        valid_until: r.get(6)?,
        active: r.get::<_, i64>(7)? != 0,
        note: r.get(8)?,
        created_by: r.get(9)?,
        created_at: r.get(10)?,
        used: r.get(11)?,
    })
}

fn unpaid_cutoff(now: i64) -> i64 {
    now - super::CHECKOUT_EXPIRES_S * 1000
}

fn read(c: &rusqlite::Connection, code: &str, now: i64) -> rusqlite::Result<Option<CodeRow>> {
    c.query_row(
        &format!("SELECT {COLS}, {USED} FROM pro_codes c WHERE c.code = ?2"),
        params![unpaid_cutoff(now), code],
        row,
    )
    .optional()
}

impl Db {
    /// Adds a code; `false` if one with this name exists.
    pub fn add_code(&self, c: &CodeRow) -> rusqlite::Result<bool> {
        Ok(self.c().execute(
            "INSERT OR IGNORE INTO pro_codes(code, kind, days, percent_off, applies_period, max_uses, valid_until, active,
               note, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?10)",
            params![
                c.code,
                c.kind,
                c.days,
                c.percent_off,
                c.applies_period,
                c.max_uses,
                c.valid_until,
                c.note,
                c.created_by,
                c.created_at
            ],
        )? > 0)
    }

    pub fn code(&self, code: &str, now: i64) -> rusqlite::Result<Option<CodeRow>> {
        read(&self.c(), code, now)
    }

    /// Every code, newest first.
    pub fn codes(&self, now: i64) -> rusqlite::Result<Vec<CodeRow>> {
        let c = self.c();
        let mut q =
            c.prepare(&format!("SELECT {COLS}, {USED} FROM pro_codes c ORDER BY c.created_at DESC LIMIT 1000"))?;
        let rows = q.query_map([unpaid_cutoff(now)], row)?;
        rows.collect()
    }

    /// Switches a code off; `None` if there's no such code.
    pub fn disable_code(&self, code: &str, now: i64) -> rusqlite::Result<Option<CodeRow>> {
        self.c().execute("UPDATE pro_codes SET active = 0 WHERE code = ?1", [code])?;
        self.code(code, now)
    }

    /// A checkout made with a discount code (a new checkout from the same install replaces it).
    pub fn note_code_use(&self, code: &str, install: &str, sub: &str, now: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO pro_code_uses(code, install_id, sub_id, at) VALUES (?1, ?2, ?3, ?4)",
            params![code, install, sub, now],
        )?;
        Ok(())
    }

    /// Codes this subscription's installs used: (code, kind, at).
    pub fn code_uses_for_sub(&self, sub: &str) -> rusqlite::Result<Vec<(String, String, i64)>> {
        let c = self.c();
        let mut q = c.prepare(
            "SELECT u.code, COALESCE(c.kind, ''), u.at FROM pro_code_uses u LEFT JOIN pro_codes c ON c.code = u.code
               WHERE u.sub_id = ?1 ORDER BY u.at DESC",
        )?;
        let rows = q.query_map([sub], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// Redeems a grant code on `install`, all in one transaction (so `max_uses` holds even
    /// when two installs redeem at once): checks it, gives or extends the install's comp
    /// Pro, and counts the use.
    pub fn redeem_grant_code(&self, code: &str, install: &str, now: i64) -> Result<(Granted, CodeRow), RedeemError> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some(row) = read(&tx, code, now)? else { return Err(RedeemError::Refused(CodeRefusal::NotFound)) };
        if let Some(r) = row.refusal(now) {
            return Err(RedeemError::Refused(r));
        }
        if row.kind != "grant" {
            return Err(RedeemError::Discount(Box::new(row)));
        }
        let used_here = tx
            .query_row(
                "SELECT 1 FROM pro_code_uses WHERE code = ?1 AND install_id = ?2",
                params![code, install],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if used_here {
            return Err(RedeemError::AlreadyRedeemed);
        }
        let days = row.days.unwrap_or(0);
        let by = GrantBy { source: "code", code: Some(code.to_string()), actor: None, note: row.note.clone() };
        let g = comp::grant_in(&tx, install, days, now, &by).map_err(RedeemError::Comp)?;
        tx.execute(
            "INSERT INTO pro_code_uses(code, install_id, sub_id, at) VALUES (?1, ?2, ?3, ?4)",
            params![code, install, g.sub_id, now],
        )?;
        tx.commit()?;
        let row = CodeRow { used: row.used + 1, ..row };
        Ok((g, row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::access;
    use crate::now_ms;

    fn db_with(installs: &[&str]) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("relay.db")).unwrap();
        for i in installs {
            db.add_install(i, "h").unwrap();
        }
        (dir, db)
    }

    fn code(code: &str, kind: &str, max_uses: Option<i64>, valid_until: Option<i64>) -> CodeRow {
        CodeRow {
            code: code.into(),
            kind: kind.into(),
            days: (kind == "grant").then_some(14),
            percent_off: (kind == "discount").then_some(20),
            applies_period: None,
            max_uses,
            valid_until,
            active: true,
            note: Some("streamer friends".into()),
            created_by: "o@x.co".into(),
            created_at: 1,
            used: 0,
        }
    }

    #[test]
    fn codes_are_normalised_and_generated_readably() {
        assert_eq!(normalize("  diwali-30 ").as_deref(), Some("DIWALI-30"));
        for bad in ["abc", "has space", "emoji🙂ok", &"A".repeat(33), "under_score"] {
            assert_eq!(normalize(bad), None, "{bad}");
        }
        let g = generate("fc-");
        assert!(g.starts_with("FC-") && g.len() == 11, "{g}");
        assert!(normalize(&g).is_some());
        assert!(!g[3..].contains(['0', 'O', '1', 'I', 'L']), "{g}");
    }

    #[test]
    fn a_grant_code_gives_pro_once_per_install_up_to_its_uses() {
        let (_d, db) = db_with(&["i1", "i2", "i3"]);
        let now = now_ms();
        assert!(db.add_code(&code("FRIENDS", "grant", Some(2), None)).unwrap());
        assert!(!db.add_code(&code("FRIENDS", "grant", None, None)).unwrap(), "names are unique");
        let (g, row) = db.redeem_grant_code("FRIENDS", "i1", now).unwrap();
        assert!(comp::is_comp(&g.sub_id));
        assert_eq!(row.used, 1);
        let s = db.pro_sub_for_install("i1").unwrap().unwrap();
        assert!(access::is_pro(&s.access(), now));
        assert!(matches!(db.redeem_grant_code("FRIENDS", "i1", now), Err(RedeemError::AlreadyRedeemed)));
        db.redeem_grant_code("FRIENDS", "i2", now).unwrap();
        assert!(matches!(db.redeem_grant_code("FRIENDS", "i3", now), Err(RedeemError::Refused(CodeRefusal::UsedUp))));
        assert_eq!(db.code("FRIENDS", now).unwrap().unwrap().used, 2);
        // The grant says where it came from.
        let src: String = db
            .c()
            .query_row("SELECT source || ':' || code FROM pro_grants WHERE sub_id = ?1", [&g.sub_id], |r| r.get(0))
            .unwrap();
        assert_eq!(src, "code:FRIENDS");
    }

    #[test]
    fn expired_disabled_unknown_and_discount_codes_are_refused() {
        let (_d, db) = db_with(&["i1"]);
        let now = now_ms();
        db.add_code(&code("OLD1", "grant", None, Some(now - 1))).unwrap();
        db.add_code(&code("OFF1", "grant", None, None)).unwrap();
        db.disable_code("OFF1", now).unwrap();
        db.add_code(&code("SAVE20", "discount", None, None)).unwrap();
        assert!(matches!(db.redeem_grant_code("OLD1", "i1", now), Err(RedeemError::Refused(CodeRefusal::Expired))));
        assert!(matches!(db.redeem_grant_code("OFF1", "i1", now), Err(RedeemError::Refused(CodeRefusal::NotFound))));
        assert!(matches!(db.redeem_grant_code("NOPE", "i1", now), Err(RedeemError::Refused(CodeRefusal::NotFound))));
        assert!(matches!(db.redeem_grant_code("SAVE20", "i1", now), Err(RedeemError::Discount(_))));
        assert!(db.pro_sub_for_install("i1").unwrap().is_none(), "nothing was granted");
    }

    #[test]
    fn a_real_subscription_blocks_a_grant_code_and_the_use_isnt_counted() {
        let (_d, db) = db_with(&["i1"]);
        let now = now_ms();
        db.add_code(&code("FRIENDS", "grant", Some(1), None)).unwrap();
        db.add_pro_sub("sub_Real000000001", "monthly", "eh", "active", "i1").unwrap();
        assert!(matches!(
            db.redeem_grant_code("FRIENDS", "i1", now),
            Err(RedeemError::Comp(CompError::RealSubscription { .. }))
        ));
        assert_eq!(db.code("FRIENDS", now).unwrap().unwrap().used, 0);
    }

    #[test]
    fn discount_uses_count_paid_and_recent_checkouts_only() {
        let (_d, db) = db_with(&["i1", "i2"]);
        let now = now_ms();
        db.add_code(&code("SAVE20", "discount", Some(5), None)).unwrap();
        db.add_pro_sub("sub_A", "monthly", "eh", "created", "i1").unwrap();
        db.note_code_use("SAVE20", "i1", "sub_A", now).unwrap();
        assert_eq!(db.code("SAVE20", now).unwrap().unwrap().used, 1, "a fresh checkout holds a use");
        let later = now + super::super::CHECKOUT_EXPIRES_S * 1000 + 1;
        assert_eq!(db.code("SAVE20", later).unwrap().unwrap().used, 0, "an abandoned one gives it back");
        db.add_pro_sub("sub_B", "monthly", "eh", "active", "i2").unwrap();
        db.note_code_use("SAVE20", "i2", "sub_B", now).unwrap();
        assert_eq!(db.code("SAVE20", later).unwrap().unwrap().used, 1, "a paid one keeps it");
        assert_eq!(db.code_uses_for_sub("sub_B").unwrap()[0].0, "SAVE20");
    }
}
