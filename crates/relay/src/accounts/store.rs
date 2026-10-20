//! The accounts tables (migration 15, docs/ACCOUNTS.md §8). Every secret (session token,
//! email link and code, OAuth state, app code) is kept only as a SHA-256 hash; the email
//! address is kept (lower-cased) because it is the account.

use rusqlite::{OptionalExtension, params};

use crate::billing::access;
use crate::billing::store::SubRow;
use crate::db::Db;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (id TEXT PRIMARY KEY, email TEXT NOT NULL UNIQUE, name TEXT, avatar_url TEXT, created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL, disabled_at INTEGER, disabled_by TEXT);
CREATE INDEX IF NOT EXISTS accounts_created ON accounts(created_at);
CREATE TABLE IF NOT EXISTS account_identities (provider TEXT NOT NULL, subject TEXT NOT NULL, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, email TEXT NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL, PRIMARY KEY (provider, subject)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS account_identities_account ON account_identities(account_id);
CREATE TABLE IF NOT EXISTS account_sessions (id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, token_hash TEXT NOT NULL, kind TEXT NOT NULL, label TEXT NOT NULL, install_id TEXT, created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, max_expires_at INTEGER NOT NULL, revoked_at INTEGER, revoked_by TEXT);
CREATE INDEX IF NOT EXISTS account_sessions_account ON account_sessions(account_id, last_used_at);
CREATE INDEX IF NOT EXISTS account_sessions_install ON account_sessions(install_id);
CREATE TABLE IF NOT EXISTS account_magic (id TEXT PRIMARY KEY, token_hash TEXT NOT NULL UNIQUE, code_hash TEXT NOT NULL, email TEXT NOT NULL, browser_hash TEXT NOT NULL, flow_id TEXT, next TEXT NOT NULL, tries INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, used_at INTEGER);
CREATE TABLE IF NOT EXISTS account_oauth (state_hash TEXT PRIMARY KEY, nonce TEXT NOT NULL, verifier TEXT NOT NULL, browser_hash TEXT NOT NULL, flow_id TEXT, next TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS account_app_flows (id TEXT PRIMARY KEY, install_id TEXT NOT NULL, redirect_uri TEXT NOT NULL, challenge TEXT NOT NULL, state TEXT NOT NULL, label TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, code_hash TEXT UNIQUE, account_id TEXT, code_expires_at INTEGER, used_at INTEGER);
CREATE TABLE IF NOT EXISTS account_subs (sub_id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, source TEXT NOT NULL, linked_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS account_subs_account ON account_subs(account_id);
CREATE TABLE IF NOT EXISTS account_installs (install_id TEXT NOT NULL, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, first_at INTEGER NOT NULL, last_at INTEGER NOT NULL, PRIMARY KEY (install_id, account_id)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS account_remote_devices (device_id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, at INTEGER NOT NULL);
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Account {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub disabled_at: Option<i64>,
}

const ACCOUNT_COLS: &str = "id, email, name, avatar_url, created_at, last_seen_at, disabled_at";

fn account(r: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: r.get(0)?,
        email: r.get(1)?,
        name: r.get(2)?,
        avatar_url: r.get(3)?,
        created_at: r.get(4)?,
        last_seen_at: r.get(5)?,
        disabled_at: r.get(6)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    pub id: String,
    pub account_id: String,
    pub token_hash: String,
    pub kind: String,
    pub label: String,
    pub install_id: Option<String>,
    pub created_at: i64,
    pub last_used_at: i64,
    pub expires_at: i64,
    pub max_expires_at: i64,
    pub revoked_at: Option<i64>,
}

const SESSION_COLS: &str = "id, account_id, token_hash, kind, label, install_id, created_at, last_used_at, expires_at, max_expires_at, revoked_at";

fn session(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        account_id: r.get(1)?,
        token_hash: r.get(2)?,
        kind: r.get(3)?,
        label: r.get(4)?,
        install_id: r.get(5)?,
        created_at: r.get(6)?,
        last_used_at: r.get(7)?,
        expires_at: r.get(8)?,
        max_expires_at: r.get(9)?,
        revoked_at: r.get(10)?,
    })
}

/// One email sign-in (link + code).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Magic {
    pub id: String,
    pub code_hash: String,
    pub email: String,
    pub browser_hash: String,
    pub flow_id: Option<String>,
    pub next: String,
    pub tries: i64,
    pub expires_at: i64,
    pub used_at: Option<i64>,
}

const MAGIC_COLS: &str = "id, code_hash, email, browser_hash, flow_id, next, tries, expires_at, used_at";

fn magic(r: &rusqlite::Row<'_>) -> rusqlite::Result<Magic> {
    Ok(Magic {
        id: r.get(0)?,
        code_hash: r.get(1)?,
        email: r.get(2)?,
        browser_hash: r.get(3)?,
        flow_id: r.get(4)?,
        next: r.get(5)?,
        tries: r.get(6)?,
        expires_at: r.get(7)?,
        used_at: r.get(8)?,
    })
}

/// A Google round trip in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OAuth {
    pub nonce: String,
    pub verifier: String,
    pub browser_hash: String,
    pub flow_id: Option<String>,
    pub next: String,
    pub expires_at: i64,
}

/// An app sign-in (§3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Flow {
    pub id: String,
    pub install_id: String,
    pub redirect_uri: String,
    pub challenge: String,
    pub state: String,
    pub label: String,
    pub expires_at: i64,
    pub code_hash: Option<String>,
    pub account_id: Option<String>,
    pub code_expires_at: Option<i64>,
    pub used_at: Option<i64>,
}

const FLOW_COLS: &str = "id, install_id, redirect_uri, challenge, state, label, expires_at, code_hash, account_id, code_expires_at, used_at";

fn flow(r: &rusqlite::Row<'_>) -> rusqlite::Result<Flow> {
    Ok(Flow {
        id: r.get(0)?,
        install_id: r.get(1)?,
        redirect_uri: r.get(2)?,
        challenge: r.get(3)?,
        state: r.get(4)?,
        label: r.get(5)?,
        expires_at: r.get(6)?,
        code_hash: r.get(7)?,
        account_id: r.get(8)?,
        code_expires_at: r.get(9)?,
        used_at: r.get(10)?,
    })
}

/// Google's side of a sign-in, when it was Google.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoogleId {
    pub subject: String,
    pub name: Option<String>,
    pub picture: Option<String>,
}

fn hex16() -> String {
    let mut b = [0u8; 8];
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn new_id(prefix: &str) -> String {
    format!("{prefix}{}{}", hex16(), hex16())
}

/// Sub rows the way `billing::store` reads them.
const SUB_COLS: &str =
    "s.id, s.plan, s.email_hash, s.status, s.current_end, s.cancel_at_end, s.refunded_at, s.synced_at";

fn sub_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SubRow> {
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

/// The SQL condition for a session that still works at `?now`.
const LIVE: &str = "s.revoked_at IS NULL AND s.expires_at > ?2 AND s.max_expires_at > ?2";

/// Picks the best of an account's subscriptions: the one granting Pro now with the latest
/// end, else the most recently synced one.
pub(crate) fn best_sub(subs: Vec<SubRow>, now: i64) -> Option<SubRow> {
    let mut pro: Vec<&SubRow> = subs.iter().filter(|s| access::is_pro(&s.access(), now)).collect();
    pro.sort_by_key(|s| std::cmp::Reverse(access::access_until(&s.access()).unwrap_or(0)));
    if let Some(s) = pro.first() {
        return Some((*s).clone());
    }
    subs.into_iter().max_by_key(|s| s.synced_at)
}

impl Db {
    pub(crate) fn account(&self, id: &str) -> rusqlite::Result<Option<Account>> {
        self.c().query_row(&format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE id = ?1"), [id], account).optional()
    }

    /// Finds or makes the account for a verified `email` (lower-cased by the caller), adding
    /// the Google identity when it was Google. `true` when it was just made.
    pub(crate) fn account_sign_in(
        &self,
        email: &str,
        google: Option<&GoogleId>,
        now: i64,
    ) -> rusqlite::Result<(Account, bool)> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let by_google: Option<String> = match google {
            Some(g) => tx
                .query_row(
                    "SELECT account_id FROM account_identities WHERE provider = 'google' AND subject = ?1",
                    [&g.subject],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
        let by_email: Option<String> =
            tx.query_row("SELECT id FROM accounts WHERE email = ?1", [email], |r| r.get(0)).optional()?;
        let (id, made) = match by_google.or(by_email) {
            Some(id) => (id, false),
            None => {
                let id = new_id("ac_");
                tx.execute(
                    "INSERT INTO accounts(id, email, created_at, last_seen_at) VALUES (?1, ?2, ?3, ?3)",
                    params![id, email, now],
                )?;
                (id, true)
            }
        };
        if let Some(g) = google {
            tx.execute(
                "INSERT INTO account_identities(provider, subject, account_id, email, created_at, last_used_at)
                 VALUES ('google', ?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(provider, subject) DO UPDATE SET email = excluded.email, last_used_at = excluded.last_used_at",
                params![g.subject, id, email, now],
            )?;
            tx.execute(
                "UPDATE accounts SET name = COALESCE(?2, name), avatar_url = COALESCE(?3, avatar_url) WHERE id = ?1",
                params![id, g.name, g.picture],
            )?;
        } else {
            tx.execute(
                "INSERT INTO account_identities(provider, subject, account_id, email, created_at, last_used_at)
                 VALUES ('email', ?1, ?2, ?1, ?3, ?3)
                 ON CONFLICT(provider, subject) DO UPDATE SET last_used_at = excluded.last_used_at",
                params![email, id, now],
            )?;
        }
        tx.execute("UPDATE accounts SET last_seen_at = ?2 WHERE id = ?1", params![id, now])?;
        let a = tx.query_row(&format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE id = ?1"), [&id], account)?;
        tx.commit()?;
        Ok((a, made))
    }

    pub(crate) fn account_providers(&self, account_id: &str) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut q =
            c.prepare("SELECT DISTINCT provider FROM account_identities WHERE account_id = ?1 ORDER BY provider DESC")?;
        let rows = q.query_map([account_id], |r| r.get(0))?;
        rows.collect()
    }

    /// Attaches the Razorpay subscriptions bought with this email (keyed hash) that no account
    /// owns yet. How many were attached.
    pub(crate) fn account_attach_email_subs(
        &self,
        account_id: &str,
        email_hash: &str,
        now: i64,
    ) -> rusqlite::Result<usize> {
        if email_hash.is_empty() {
            return Ok(0);
        }
        self.c().execute(
            "INSERT OR IGNORE INTO account_subs(sub_id, account_id, source, linked_at)
             SELECT id, ?1, 'email_match', ?3 FROM pro_subs
              WHERE email_hash = ?2 AND id NOT LIKE 'comp\\_%' ESCAPE '\\'",
            params![account_id, email_hash, now],
        )
    }

    /// Makes `sub` belong to the account (no-op when another account already owns it).
    pub(crate) fn account_attach_sub(
        &self,
        account_id: &str,
        sub: &str,
        source: &str,
        now: i64,
    ) -> rusqlite::Result<bool> {
        Ok(self.c().execute(
            "INSERT OR IGNORE INTO account_subs(sub_id, account_id, source, linked_at) VALUES (?1, ?2, ?3, ?4)",
            params![sub, account_id, source, now],
        )? > 0)
    }

    pub(crate) fn account_subs(&self, account_id: &str) -> rusqlite::Result<Vec<SubRow>> {
        let c = self.c();
        let mut q = c.prepare(&format!(
            "SELECT {SUB_COLS} FROM account_subs a JOIN pro_subs s ON s.id = a.sub_id WHERE a.account_id = ?1
              ORDER BY a.linked_at DESC"
        ))?;
        let rows = q.query_map([account_id], sub_row)?;
        rows.collect()
    }

    /// The account's subscription for Pro: [`best_sub`].
    pub(crate) fn account_sub(&self, account_id: &str, now: i64) -> rusqlite::Result<Option<SubRow>> {
        Ok(best_sub(self.account_subs(account_id)?, now))
    }

    /// The (active) account an install is signed in to, if the install is among the account's
    /// `MAX_INSTALLS` most recently signed-in PCs.
    pub(crate) fn account_for_install(&self, install: &str, now: i64) -> rusqlite::Result<Option<String>> {
        let c = self.c();
        let acct: Option<String> = c
            .query_row(
                &format!(
                    "SELECT s.account_id FROM account_sessions s JOIN accounts a ON a.id = s.account_id
                      WHERE s.install_id = ?1 AND s.kind = 'app' AND a.disabled_at IS NULL AND {LIVE}
                      ORDER BY s.last_used_at DESC LIMIT 1"
                ),
                params![install, now],
                |r| r.get(0),
            )
            .optional()?;
        let Some(acct) = acct else { return Ok(None) };
        let ok: bool = c.query_row(
            &format!(
                "SELECT ?3 IN (SELECT s.install_id FROM account_sessions s WHERE s.account_id = ?1 AND s.kind = 'app'
                   AND s.install_id IS NOT NULL AND {LIVE} GROUP BY s.install_id ORDER BY MAX(s.created_at) DESC LIMIT ?4)"
            ),
            params![acct, now, install, crate::billing::store::MAX_INSTALLS],
            |r| r.get(0),
        )?;
        Ok(ok.then_some(acct))
    }

    /// The account's signed-in PCs: (install id, label, last use), newest first.
    pub(crate) fn account_pcs(&self, account_id: &str, now: i64) -> rusqlite::Result<Vec<(String, String, i64)>> {
        let c = self.c();
        let mut q = c.prepare(&format!(
            "SELECT s.install_id, s.label, MAX(s.last_used_at) FROM account_sessions s
              WHERE s.account_id = ?1 AND s.kind = 'app' AND s.install_id IS NOT NULL AND {LIVE}
              GROUP BY s.install_id ORDER BY MAX(s.last_used_at) DESC LIMIT 50"
        ))?;
        let rows = q.query_map(params![account_id, now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    // ── Sessions ──

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn account_add_session(
        &self,
        id: &str,
        account_id: &str,
        token_hash: &str,
        kind: &str,
        label: &str,
        install: Option<&str>,
        now: i64,
        ttl: i64,
        max_ttl: i64,
    ) -> rusqlite::Result<()> {
        let c = self.c();
        c.execute(
            &format!(
                "INSERT INTO account_sessions({SESSION_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, NULL)"
            ),
            params![id, account_id, token_hash, kind, label, install, now, now + ttl.min(max_ttl), now + max_ttl],
        )?;
        if let Some(i) = install {
            c.execute(
                "INSERT INTO account_installs(install_id, account_id, first_at, last_at) VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT(install_id, account_id) DO UPDATE SET last_at = excluded.last_at",
                params![i, account_id, now],
            )?;
        }
        c.execute("UPDATE accounts SET last_seen_at = ?2 WHERE id = ?1", params![account_id, now])?;
        Ok(())
    }

    pub(crate) fn account_session(&self, id: &str) -> rusqlite::Result<Option<Session>> {
        self.c()
            .query_row(&format!("SELECT {SESSION_COLS} FROM account_sessions WHERE id = ?1"), [id], session)
            .optional()
    }

    /// Slides a session's expiry (never past its cap) and the account's last-seen time.
    pub(crate) fn account_touch(&self, s: &Session, now: i64, ttl: i64) -> rusqlite::Result<()> {
        let c = self.c();
        c.execute(
            "UPDATE account_sessions SET last_used_at = ?2, expires_at = MIN(?3, max_expires_at) WHERE id = ?1",
            params![s.id, now, now + ttl],
        )?;
        c.execute("UPDATE accounts SET last_seen_at = ?2 WHERE id = ?1", params![s.account_id, now])?;
        if let Some(i) = &s.install_id {
            c.execute(
                "UPDATE account_installs SET last_at = ?3 WHERE install_id = ?1 AND account_id = ?2",
                params![i, s.account_id, now],
            )?;
        }
        Ok(())
    }

    /// The account's sessions that still work, newest use first.
    pub(crate) fn account_sessions(&self, account_id: &str, now: i64) -> rusqlite::Result<Vec<Session>> {
        let c = self.c();
        let cols = SESSION_COLS.split(", ").map(|x| format!("s.{x}")).collect::<Vec<_>>().join(", ");
        let mut q = c.prepare(&format!(
            "SELECT {cols} FROM account_sessions s WHERE s.account_id = ?1 AND {LIVE}
              ORDER BY s.last_used_at DESC LIMIT 100"
        ))?;
        let rows = q.query_map(params![account_id, now], session)?;
        rows.collect()
    }

    /// Revokes one session of the account; `false` if it isn't the account's or already gone.
    pub(crate) fn account_revoke(&self, account_id: &str, id: &str, by: &str, now: i64) -> rusqlite::Result<bool> {
        Ok(self.c().execute(
            "UPDATE account_sessions SET revoked_at = ?3, revoked_by = ?4
              WHERE id = ?1 AND account_id = ?2 AND revoked_at IS NULL",
            params![id, account_id, now, by],
        )? > 0)
    }

    /// Revokes every session of the account. How many.
    pub(crate) fn account_revoke_all(&self, account_id: &str, by: &str, now: i64) -> rusqlite::Result<usize> {
        self.c().execute(
            "UPDATE account_sessions SET revoked_at = ?2, revoked_by = ?3 WHERE account_id = ?1 AND revoked_at IS NULL",
            params![account_id, now, by],
        )
    }

    /// Disables (`Some(actor)`) or enables (`None`) an account. Disabling signs it out everywhere.
    pub(crate) fn account_set_disabled(&self, id: &str, by: Option<&str>, now: i64) -> rusqlite::Result<bool> {
        let n = match by {
            Some(actor) => self.c().execute(
                "UPDATE accounts SET disabled_at = COALESCE(disabled_at, ?2), disabled_by = ?3 WHERE id = ?1",
                params![id, now, actor],
            )?,
            None => self
                .c()
                .execute("UPDATE accounts SET disabled_at = NULL, disabled_by = NULL WHERE id = ?1", params![id])?,
        };
        if n > 0
            && let Some(actor) = by
        {
            self.account_revoke_all(id, &format!("admin:{actor}"), now)?;
        }
        Ok(n > 0)
    }

    // ── Email sign-in ──

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn account_put_magic(
        &self,
        id: &str,
        token_hash: &str,
        code_hash: &str,
        email: &str,
        browser_hash: &str,
        flow: Option<&str>,
        next: &str,
        now: i64,
        expires_at: i64,
    ) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO account_magic(id, token_hash, code_hash, email, browser_hash, flow_id, next, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![id, token_hash, code_hash, email, browser_hash, flow, next, now, expires_at],
        )?;
        Ok(())
    }

    pub(crate) fn account_magic_by_token(&self, token_hash: &str) -> rusqlite::Result<Option<Magic>> {
        self.c()
            .query_row(&format!("SELECT {MAGIC_COLS} FROM account_magic WHERE token_hash = ?1"), [token_hash], magic)
            .optional()
    }

    pub(crate) fn account_magic(&self, id: &str) -> rusqlite::Result<Option<Magic>> {
        self.c().query_row(&format!("SELECT {MAGIC_COLS} FROM account_magic WHERE id = ?1"), [id], magic).optional()
    }

    /// Counts a wrong code; the tries made so far.
    pub(crate) fn account_magic_miss(&self, id: &str) -> rusqlite::Result<i64> {
        self.c()
            .query_row("UPDATE account_magic SET tries = tries + 1 WHERE id = ?1 RETURNING tries", [id], |r| r.get(0))
    }

    /// Uses the sign-in up, once: `false` if it was already used (a replay) or expired.
    pub(crate) fn account_magic_use(&self, id: &str, now: i64) -> rusqlite::Result<bool> {
        Ok(self.c().execute(
            "UPDATE account_magic SET used_at = ?2 WHERE id = ?1 AND used_at IS NULL AND expires_at > ?2",
            params![id, now],
        )? > 0)
    }

    // ── Google ──

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn account_put_oauth(
        &self,
        state_hash: &str,
        nonce: &str,
        verifier: &str,
        browser_hash: &str,
        flow: Option<&str>,
        next: &str,
        now: i64,
        expires_at: i64,
    ) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO account_oauth(state_hash, nonce, verifier, browser_hash, flow_id, next, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![state_hash, nonce, verifier, browser_hash, flow, next, now, expires_at],
        )?;
        Ok(())
    }

    /// Takes a Google round trip once (deleted whether or not it was still valid).
    pub(crate) fn account_take_oauth(&self, state_hash: &str, now: i64) -> rusqlite::Result<Option<OAuth>> {
        let row = self
            .c()
            .query_row(
                "DELETE FROM account_oauth WHERE state_hash = ?1
                 RETURNING nonce, verifier, browser_hash, flow_id, next, expires_at",
                [state_hash],
                |r| {
                    Ok(OAuth {
                        nonce: r.get(0)?,
                        verifier: r.get(1)?,
                        browser_hash: r.get(2)?,
                        flow_id: r.get(3)?,
                        next: r.get(4)?,
                        expires_at: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row.filter(|o| o.expires_at > now))
    }

    // ── App sign-in ──

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn account_put_flow(
        &self,
        id: &str,
        install: &str,
        redirect_uri: &str,
        challenge: &str,
        state: &str,
        label: &str,
        now: i64,
        expires_at: i64,
    ) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO account_app_flows(id, install_id, redirect_uri, challenge, state, label, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, install, redirect_uri, challenge, state, label, now, expires_at],
        )?;
        Ok(())
    }

    pub(crate) fn account_flow(&self, id: &str) -> rusqlite::Result<Option<Flow>> {
        self.c().query_row(&format!("SELECT {FLOW_COLS} FROM account_app_flows WHERE id = ?1"), [id], flow).optional()
    }

    /// Gives a waiting flow its one-time code (once: `false` if it already has one).
    pub(crate) fn account_flow_code(
        &self,
        id: &str,
        code_hash: &str,
        account_id: &str,
        now: i64,
        code_expires_at: i64,
    ) -> rusqlite::Result<bool> {
        Ok(self.c().execute(
            "UPDATE account_app_flows SET code_hash = ?2, account_id = ?3, code_expires_at = ?5
              WHERE id = ?1 AND code_hash IS NULL AND used_at IS NULL AND expires_at > ?4",
            params![id, code_hash, account_id, now, code_expires_at],
        )? > 0)
    }

    pub(crate) fn account_flow_by_code(&self, code_hash: &str) -> rusqlite::Result<Option<Flow>> {
        self.c()
            .query_row(&format!("SELECT {FLOW_COLS} FROM account_app_flows WHERE code_hash = ?1"), [code_hash], flow)
            .optional()
    }

    /// Uses the code up, once.
    pub(crate) fn account_flow_use(&self, id: &str, now: i64) -> rusqlite::Result<bool> {
        Ok(self.c().execute(
            "UPDATE account_app_flows SET used_at = ?2 WHERE id = ?1 AND used_at IS NULL AND code_expires_at > ?2",
            params![id, now],
        )? > 0)
    }

    // ── Web dashboard devices ──

    /// Binds a device to an account unless it's bound already. The owner afterwards.
    pub(crate) fn account_bind_device(&self, device: &str, account_id: &str, now: i64) -> rusqlite::Result<String> {
        let c = self.c();
        c.execute(
            "INSERT OR IGNORE INTO account_remote_devices(device_id, account_id, at) VALUES (?1, ?2, ?3)",
            params![device, account_id, now],
        )?;
        c.query_row("SELECT account_id FROM account_remote_devices WHERE device_id = ?1", [device], |r| r.get(0))
    }

    /// Housekeeping: finished sign-ins after a day, sessions a month after they ended, links
    /// to subscriptions and devices that are gone.
    pub(crate) fn prune_accounts(&self, now: i64) -> rusqlite::Result<()> {
        let day = 86_400_000;
        let c = self.c();
        c.execute("DELETE FROM account_magic WHERE expires_at < ?1", [now - day])?;
        c.execute("DELETE FROM account_oauth WHERE expires_at < ?1", [now])?;
        c.execute("DELETE FROM account_app_flows WHERE expires_at < ?1", [now - day])?;
        c.execute(
            "DELETE FROM account_sessions WHERE COALESCE(revoked_at, MIN(expires_at, max_expires_at)) < ?1",
            [now - 30 * day],
        )?;
        c.execute("DELETE FROM account_subs WHERE sub_id NOT IN (SELECT id FROM pro_subs)", [])?;
        c.execute("DELETE FROM account_remote_devices WHERE device_id NOT IN (SELECT id FROM remote_devices)", [])?;
        Ok(())
    }
}
