//! Owner-only test Pro ("comp" grants): Pro for one install without Razorpay, so the owner
//! can try Pro end to end on their own desktop. Only reachable from the relay's command line
//! (`ferrocast-relay pro-grant …`, run on the server through `deploy/pro-grant.sh`); there's
//! no HTTP route. A grant is an ordinary `pro_subs` row with a `comp_` id and plan `comp`,
//! so every Pro feature sees it as Pro; everything that would ask Razorpay about a
//! subscription skips it (see [`is_comp`]).

use rusqlite::{OptionalExtension, params};

use super::access;
use crate::db::Db;
use crate::now_ms;

/// Every comp grant's id starts with this; Razorpay's start with `sub_`.
pub const PREFIX: &str = "comp_";
/// The plan stored (and shown by `/v1/pro/status`) for a comp grant.
pub const PLAN: &str = "comp";
/// The longest a grant reaches past now, however often it's extended.
pub const MAX_DAYS: i64 = 90;
const DAY_MS: i64 = 86_400_000;

pub fn is_comp(sub_id: &str) -> bool {
    sub_id.starts_with(PREFIX)
}

/// One grant in `--list`: (sub id, install or "", status, current_end).
pub type CompRow = (String, String, String, Option<i64>);

/// What a grant or revoke did, for the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Granted {
    pub sub_id: String,
    pub until: i64,
    pub extended: bool,
}

impl Db {
    /// Grants (or extends) test Pro on `install` for `days` (1..=[`MAX_DAYS`]). Ends with the
    /// period, no renewal grace (`cancel_at_end`). Refuses an unknown install, and one whose
    /// real subscription still grants or renews Pro.
    pub fn comp_grant(&self, install: &str, days: i64) -> Result<Granted, String> {
        if !(1..=MAX_DAYS).contains(&days) {
            return Err(format!("Days must be 1 to {MAX_DAYS}."));
        }
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(db)?;
        let known = tx
            .query_row("SELECT 1 FROM installs WHERE id = ?1", [install], |_| Ok(()))
            .optional()
            .map_err(db)?
            .is_some();
        if !known {
            return Err(format!(
                "No install '{install}' on this relay. Check the id (the part before the dot in the app's relay-install secret)."
            ));
        }
        let now = now_ms();
        let linked: Option<(String, String, Option<i64>, bool, bool)> = tx
            .query_row(
                "SELECT s.id, s.status, s.current_end, s.cancel_at_end, s.refunded_at IS NOT NULL
                   FROM pro_installs p JOIN pro_subs s ON s.id = p.sub_id WHERE p.install_id = ?1",
                [install],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0, r.get(4)?)),
            )
            .optional()
            .map_err(db)?;
        let cap = now + MAX_DAYS * DAY_MS;
        let granted = match linked {
            Some((id, status, end, _, _)) if is_comp(&id) => {
                // Extend from what's left (or from now once it ran out).
                let from = end.filter(|e| status == "active" && *e > now).unwrap_or(now);
                let until = (from + days * DAY_MS).min(cap);
                tx.execute(
                    "UPDATE pro_subs SET status = 'active', current_end = ?2, cancel_at_end = 1, refunded_at = NULL,
                       synced_at = ?3 WHERE id = ?1",
                    params![id, until, now],
                )
                .map_err(db)?;
                Granted { sub_id: id, until, extended: true }
            }
            Some((id, status, current_end, cancel_at_end, refunded)) => {
                let a = access::SubState { status: status.clone(), current_end, cancel_at_end, refunded };
                if (access::BLOCKING.contains(&status.as_str()) && !refunded) || access::is_pro(&a, now) {
                    return Err(format!(
                        "Install '{install}' has a real Razorpay subscription ({id}, {status}). Nothing was changed: \
                         cancel it and let it end first, or test on another install."
                    ));
                }
                new_grant(&tx, install, days, now).map_err(db)?
            }
            None => new_grant(&tx, install, days, now).map_err(db)?,
        };
        tx.commit().map_err(db)?;
        Ok(granted)
    }

    /// Ends the install's test Pro now. The row stays (as `cancelled`, ended now), so the
    /// usual "kept 30 days after Pro ended" rules clean up its data like any lapsed Pro,
    /// and a later grant extends the same id.
    pub fn comp_revoke(&self, install: &str) -> Result<Granted, String> {
        let c = self.c();
        let id: Option<String> = c
            .query_row("SELECT sub_id FROM pro_installs WHERE install_id = ?1", [install], |r| r.get(0))
            .optional()
            .map_err(db)?;
        let Some(id) = id.filter(|id| is_comp(id)) else {
            return Err(format!("Install '{install}' has no test Pro grant. Nothing was changed."));
        };
        let now = now_ms();
        c.execute(
            "UPDATE pro_subs SET status = 'cancelled', current_end = MIN(COALESCE(current_end, ?2), ?2),
               cancel_at_end = 1, synced_at = ?2 WHERE id = ?1",
            params![id, now],
        )
        .map_err(db)?;
        Ok(Granted { sub_id: id, until: now, extended: false })
    }

    /// Every comp grant: (sub id, install or "", status, current_end).
    pub fn comp_list(&self) -> rusqlite::Result<Vec<CompRow>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT s.id, COALESCE(p.install_id, ''), s.status, s.current_end FROM pro_subs s
               LEFT JOIN pro_installs p ON p.sub_id = s.id WHERE s.id LIKE 'comp\\_%' ESCAPE '\\' ORDER BY s.created_at",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect()
    }
}

fn new_grant(tx: &rusqlite::Transaction<'_>, install: &str, days: i64, now: i64) -> rusqlite::Result<Granted> {
    let id = format!("{PREFIX}{}", crate::random(12));
    let until = now + days * DAY_MS;
    tx.execute(
        "INSERT INTO pro_subs(id, plan, email_hash, status, current_end, cancel_at_end, created_at, synced_at)
         VALUES (?1, ?2, '', 'active', ?3, 1, ?4, ?4)",
        params![id, PLAN, until, now],
    )?;
    super::store::link(tx, install, &id, now)?;
    Ok(Granted { sub_id: id, until, extended: false })
}

fn db(e: rusqlite::Error) -> String {
    format!("The relay database refused: {e}")
}

const USAGE: &str = "Usage:\n  ferrocast-relay pro-grant <install-id> <days>   grant or extend test Pro (1-90 days)\n  \
                     ferrocast-relay pro-grant --revoke <install-id>  end it now\n  \
                     ferrocast-relay pro-grant --list                 show every grant";

/// `ferrocast-relay pro-grant …` (the arguments after `pro-grant`): what to print, or the error.
pub fn cli(db: &Db, args: &[String]) -> Result<String, String> {
    let date = |ms: i64| super::date(ms);
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["--list"] => {
            let rows = db.comp_list().map_err(self::db)?;
            if rows.is_empty() {
                return Ok("No test Pro grants.".into());
            }
            let now = now_ms();
            Ok(rows
                .iter()
                .map(|(id, install, status, end)| {
                    let state = match end {
                        Some(e) if status == "active" && *e > now => format!("Pro until {}", date(*e)),
                        Some(e) => format!("ended {}", date(*e)),
                        None => "ended".into(),
                    };
                    let install = if install.is_empty() { "(no install)" } else { install };
                    format!("{id}  {install}  {state}")
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        ["--revoke", install] => {
            let g = db.comp_revoke(install)?;
            Ok(format!("Test Pro on {install} ended now ({}).", g.sub_id))
        }
        [install, days] if !install.starts_with('-') => {
            let days: i64 = days.parse().map_err(|_| format!("'{days}' isn't a number of days.\n{USAGE}"))?;
            let g = db.comp_grant(install, days)?;
            let verb = if g.extended { "extended" } else { "granted" };
            Ok(format!(
                "Test Pro {verb} on {install} until {} ({}). In the app, press Check again in Settings › Pro.",
                date(g.until),
                g.sub_id
            ))
        }
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with(installs: &[&str]) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("relay.db")).unwrap();
        for i in installs {
            db.add_install(i, "h").unwrap();
        }
        (dir, db)
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_grant_is_pro_with_no_renewal_grace() {
        let (_d, db) = db_with(&["i1"]);
        let before = now_ms();
        let g = db.comp_grant("i1", 7).unwrap();
        assert!(is_comp(&g.sub_id) && !g.extended);
        let s = db.pro_sub_for_install("i1").unwrap().unwrap();
        assert_eq!((s.plan.as_str(), s.status.as_str(), s.email_hash.as_str()), (PLAN, "active", ""));
        assert!(access::is_pro(&s.access(), now_ms()));
        assert_eq!(access::access_until(&s.access()), s.current_end, "ends with the grant");
        assert!(s.current_end.unwrap() >= before + 7 * DAY_MS);
        assert!(db.open_pro_subs().unwrap().is_empty(), "never sent to Razorpay's daily re-check");
    }

    #[test]
    fn granting_again_extends_up_to_the_cap() {
        let (_d, db) = db_with(&["i1"]);
        let g1 = db.comp_grant("i1", 10).unwrap();
        let g2 = db.comp_grant("i1", 5).unwrap();
        assert_eq!(g2.sub_id, g1.sub_id);
        assert!(g2.extended);
        assert_eq!(g2.until, g1.until + 5 * DAY_MS);
        let g3 = db.comp_grant("i1", 90).unwrap();
        assert!(g3.until <= now_ms() + MAX_DAYS * DAY_MS, "capped at 90 days from now");
        assert_eq!(db.comp_list().unwrap().len(), 1);
        for bad in [0, -1, 91] {
            assert!(db.comp_grant("i1", bad).is_err(), "{bad} days");
        }
    }

    #[test]
    fn revoke_ends_it_now_and_a_new_grant_starts_from_now() {
        let (_d, db) = db_with(&["i1", "i2"]);
        let g = db.comp_grant("i1", 30).unwrap();
        db.comp_revoke("i1").unwrap();
        let s = db.pro_sub_for_install("i1").unwrap().unwrap();
        assert_eq!(s.status, "cancelled");
        assert!(!access::is_pro(&s.access(), now_ms() + 1));
        assert!(s.stopped_at() <= now_ms(), "the 30-day lapse rules count from now");
        assert!(db.comp_revoke("i2").unwrap_err().contains("no test Pro"));
        let again = db.comp_grant("i1", 2).unwrap();
        assert_eq!(again.sub_id, g.sub_id);
        assert!(again.until <= now_ms() + 2 * DAY_MS);
        assert!(access::is_pro(&db.pro_sub_for_install("i1").unwrap().unwrap().access(), now_ms()));
    }

    #[test]
    fn unknown_installs_and_real_subscriptions_are_refused() {
        let (_d, db) = db_with(&["i1", "i2"]);
        assert!(db.comp_grant("nope", 7).unwrap_err().contains("No install 'nope'"));
        db.add_pro_sub("sub_Real000000001", "monthly", "eh", "active", "i1").unwrap();
        let e = db.comp_grant("i1", 7).unwrap_err();
        assert!(e.contains("real Razorpay subscription"), "{e}");
        assert_eq!(db.pro_sub_for_install("i1").unwrap().unwrap().id, "sub_Real000000001", "left alone");
        // An ended one (no access left) no longer blocks a test grant.
        db.add_pro_sub("sub_Old0000000001", "monthly", "eh", "expired", "i2").unwrap();
        assert!(db.comp_grant("i2", 7).is_ok());
    }

    #[test]
    fn the_command_line_answers_every_form() {
        let (_d, db) = db_with(&["i1"]);
        assert_eq!(cli(&db, &args(&["--list"])).unwrap(), "No test Pro grants.");
        assert!(cli(&db, &args(&["i1", "7"])).unwrap().contains("granted on i1"));
        assert!(cli(&db, &args(&["i1", "7"])).unwrap().contains("extended on i1"));
        assert!(cli(&db, &args(&["--list"])).unwrap().contains("i1  Pro until"));
        assert!(cli(&db, &args(&["i1", "seven"])).unwrap_err().contains("isn't a number"));
        assert!(cli(&db, &args(&["--revoke", "i1"])).unwrap().contains("ended now"));
        assert!(cli(&db, &args(&["--list"])).unwrap().contains("ended"));
        for bad in [&[][..], &["i1"], &["--revoke"], &["--nope", "1"], &["a", "b", "c"]] {
            assert!(cli(&db, &args(bad)).unwrap_err().starts_with("Usage:"), "{bad:?}");
        }
    }
}
