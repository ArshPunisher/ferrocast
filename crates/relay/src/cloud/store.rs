//! Cloud mode's tables (docs/CLOUD.md §2): which accounts use it, their documents
//! (overlays, alert rules, designs, settings) with per-account versions for the desktop's
//! sync, overlay URL hashes, connected channels, uploaded media and connect finishes.

use rusqlite::{OptionalExtension, params};

use crate::db::Db;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cloud_accounts (account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE, enabled_at INTEGER NOT NULL, last_active_at INTEGER NOT NULL, sleeping_at INTEGER, version INTEGER NOT NULL DEFAULT 0, install_id TEXT);
CREATE TABLE IF NOT EXISTS cloud_docs (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, kind TEXT NOT NULL, id TEXT NOT NULL, version INTEGER NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, deleted_at INTEGER, json TEXT, PRIMARY KEY (account_id, kind, id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS cloud_docs_version ON cloud_docs(account_id, version);
CREATE TABLE IF NOT EXISTS cloud_overlay_tokens (token_hash TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, overlay_id TEXT NOT NULL, generation INTEGER NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS cloud_overlay_tokens_overlay ON cloud_overlay_tokens(account_id, overlay_id);
CREATE TABLE IF NOT EXISTS cloud_channels (platform TEXT NOT NULL, channel_id TEXT NOT NULL, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, login TEXT NOT NULL, name TEXT, avatar_url TEXT, scopes TEXT NOT NULL, event_subs TEXT NOT NULL DEFAULT '[]', connected_at INTEGER NOT NULL, error TEXT, PRIMARY KEY (platform, channel_id)) WITHOUT ROWID;
CREATE UNIQUE INDEX IF NOT EXISTS cloud_channels_account ON cloud_channels(account_id, platform);
CREATE TABLE IF NOT EXISTS cloud_media (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, id TEXT NOT NULL, sha256 TEXT NOT NULL, kind TEXT NOT NULL, mime TEXT NOT NULL, bytes INTEGER NOT NULL, name TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (account_id, id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS cloud_media_sha ON cloud_media(account_id, sha256);
CREATE TABLE IF NOT EXISTS cloud_pending (code_hash TEXT PRIMARY KEY, account_id TEXT NOT NULL, platform TEXT NOT NULL, payload TEXT NOT NULL, expires_at INTEGER NOT NULL);
";

/// The document kinds (docs/CLOUD.md §2).
pub(crate) const KINDS: [&str; 4] = ["overlay", "alert_rule", "design", "settings"];

/// One account's use of cloud mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloudAccount {
    pub account_id: String,
    pub enabled_at: i64,
    pub last_active_at: i64,
    pub sleeping_at: Option<i64>,
    pub version: i64,
    pub install_id: Option<String>,
}

fn cloud_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<CloudAccount> {
    Ok(CloudAccount {
        account_id: r.get(0)?,
        enabled_at: r.get(1)?,
        last_active_at: r.get(2)?,
        sleeping_at: r.get(3)?,
        version: r.get(4)?,
        install_id: r.get(5)?,
    })
}

const ACCOUNT_COLS: &str = "account_id, enabled_at, last_active_at, sleeping_at, version, install_id";

/// One stored document (a tombstone has `json: None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Doc {
    pub kind: String,
    pub id: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
    pub json: Option<String>,
}

fn doc(r: &rusqlite::Row<'_>) -> rusqlite::Result<Doc> {
    Ok(Doc {
        kind: r.get(0)?,
        id: r.get(1)?,
        version: r.get(2)?,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
        deleted_at: r.get(5)?,
        json: r.get(6)?,
    })
}

const DOC_COLS: &str = "kind, id, version, created_at, updated_at, deleted_at, json";

/// How a write with a base version went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Write {
    /// Saved at this version.
    Saved(i64),
    /// The stored copy moved on: what it is now (`None`: there is none).
    Conflict(Option<Doc>),
}

/// A connected channel (docs/CLOUD.md §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Channel {
    pub platform: String,
    pub channel_id: String,
    pub account_id: String,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub scopes: Vec<String>,
    pub event_subs: Vec<String>,
    pub connected_at: i64,
    pub error: Option<String>,
}

fn channel(r: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
    let scopes: String = r.get(6)?;
    let subs: String = r.get(7)?;
    Ok(Channel {
        platform: r.get(0)?,
        channel_id: r.get(1)?,
        account_id: r.get(2)?,
        login: r.get(3)?,
        name: r.get(4)?,
        avatar_url: r.get(5)?,
        scopes: scopes.split(' ').filter(|s| !s.is_empty()).map(String::from).collect(),
        event_subs: serde_json::from_str(&subs).unwrap_or_default(),
        connected_at: r.get(8)?,
        error: r.get(9)?,
    })
}

const CHANNEL_COLS: &str =
    "platform, channel_id, account_id, login, name, avatar_url, scopes, event_subs, connected_at, error";

/// An uploaded file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Media {
    pub id: String,
    pub sha256: String,
    pub kind: String,
    pub mime: String,
    pub bytes: i64,
    pub name: String,
    pub created_at: i64,
}

fn media(r: &rusqlite::Row<'_>) -> rusqlite::Result<Media> {
    Ok(Media {
        id: r.get(0)?,
        sha256: r.get(1)?,
        kind: r.get(2)?,
        mime: r.get(3)?,
        bytes: r.get(4)?,
        name: r.get(5)?,
        created_at: r.get(6)?,
    })
}

const MEDIA_COLS: &str = "id, sha256, kind, mime, bytes, name, created_at";

/// Tombstones are kept this long, so a PC that syncs within a month sees the delete.
pub(crate) const TOMBSTONE_KEEP_MS: i64 = 30 * 86_400_000;

impl Db {
    pub(crate) fn cloud_account(&self, account: &str) -> rusqlite::Result<Option<CloudAccount>> {
        self.c()
            .query_row(
                &format!("SELECT {ACCOUNT_COLS} FROM cloud_accounts WHERE account_id = ?1"),
                [account],
                cloud_account,
            )
            .optional()
    }

    /// Turns cloud mode on (idempotent) and wakes the account; `true` when it was just made.
    pub(crate) fn cloud_enable(&self, account: &str, now: i64) -> rusqlite::Result<bool> {
        let n = self.c().execute(
            "INSERT INTO cloud_accounts(account_id, enabled_at, last_active_at) VALUES (?1, ?2, ?2)
             ON CONFLICT(account_id) DO NOTHING",
            params![account, now],
        )?;
        if n == 0 {
            self.cloud_touch(account, now)?;
        }
        Ok(n == 1)
    }

    /// Use of any kind (sign-in, dashboard, overlay): wakes a sleeping account.
    pub(crate) fn cloud_touch(&self, account: &str, now: i64) -> rusqlite::Result<bool> {
        let woke = self.c().execute(
            "UPDATE cloud_accounts SET last_active_at = ?2, sleeping_at = NULL WHERE account_id = ?1 AND sleeping_at IS NOT NULL",
            params![account, now],
        )?;
        self.c().execute(
            "UPDATE cloud_accounts SET last_active_at = MAX(last_active_at, ?2) WHERE account_id = ?1",
            params![account, now],
        )?;
        Ok(woke == 1)
    }

    /// Accounts with cloud mode on and awake, not used since `before`: put to sleep.
    pub(crate) fn cloud_sleep_idle(&self, before: i64, now: i64) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut s = c.prepare(
            "UPDATE cloud_accounts SET sleeping_at = ?2 WHERE sleeping_at IS NULL AND last_active_at < ?1 RETURNING account_id",
        )?;
        s.query_map(params![before, now], |r| r.get(0))?.collect()
    }

    pub(crate) fn cloud_accounts(&self) -> rusqlite::Result<Vec<CloudAccount>> {
        let c = self.c();
        let mut s = c.prepare(&format!("SELECT {ACCOUNT_COLS} FROM cloud_accounts"))?;
        s.query_map([], cloud_account)?.collect()
    }

    pub(crate) fn cloud_set_install(&self, account: &str, install: &str) -> rusqlite::Result<()> {
        self.c()
            .execute("UPDATE cloud_accounts SET install_id = ?2 WHERE account_id = ?1", params![account, install])?;
        Ok(())
    }

    /// The account a cloud install belongs to.
    pub(crate) fn cloud_account_by_install(&self, install: &str) -> rusqlite::Result<Option<String>> {
        self.c()
            .query_row("SELECT account_id FROM cloud_accounts WHERE install_id = ?1", [install], |r| r.get(0))
            .optional()
    }

    // ── Documents ────────────────────────────────────────────────────────────────────

    pub(crate) fn cloud_docs(&self, account: &str, kind: &str) -> rusqlite::Result<Vec<Doc>> {
        let c = self.c();
        let mut s = c.prepare(&format!(
            "SELECT {DOC_COLS} FROM cloud_docs WHERE account_id = ?1 AND kind = ?2 AND deleted_at IS NULL
             ORDER BY created_at, id"
        ))?;
        s.query_map(params![account, kind], doc)?.collect()
    }

    pub(crate) fn cloud_doc_count(&self, account: &str, kind: &str) -> rusqlite::Result<i64> {
        self.c().query_row(
            "SELECT COUNT(*) FROM cloud_docs WHERE account_id = ?1 AND kind = ?2 AND deleted_at IS NULL",
            params![account, kind],
            |r| r.get(0),
        )
    }

    /// One document, tombstones included.
    pub(crate) fn cloud_doc(&self, account: &str, kind: &str, id: &str) -> rusqlite::Result<Option<Doc>> {
        self.c()
            .query_row(
                &format!("SELECT {DOC_COLS} FROM cloud_docs WHERE account_id = ?1 AND kind = ?2 AND id = ?3"),
                params![account, kind, id],
                doc,
            )
            .optional()
    }

    /// Everything changed after `since` (tombstones included), oldest change first.
    pub(crate) fn cloud_changes(&self, account: &str, since: i64, limit: i64) -> rusqlite::Result<Vec<Doc>> {
        let c = self.c();
        let mut s = c.prepare(&format!(
            "SELECT {DOC_COLS} FROM cloud_docs WHERE account_id = ?1 AND version > ?2 ORDER BY version LIMIT ?3"
        ))?;
        s.query_map(params![account, since, limit], doc)?.collect()
    }

    /// Writes (`json: Some`) or deletes (`None`) a document. With `base`, only while the
    /// stored copy is still at that version (0 = there is none yet, or it's a tombstone).
    pub(crate) fn cloud_write(
        &self,
        account: &str,
        kind: &str,
        id: &str,
        json: Option<&str>,
        base: Option<i64>,
        now: i64,
    ) -> rusqlite::Result<Write> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = tx
            .query_row(
                &format!("SELECT {DOC_COLS} FROM cloud_docs WHERE account_id = ?1 AND kind = ?2 AND id = ?3"),
                params![account, kind, id],
                doc,
            )
            .optional()?;
        if let Some(base) = base {
            let at = current.as_ref().filter(|d| d.deleted_at.is_none()).map_or(0, |d| d.version);
            // A delete of something that's already gone, or a write over a tombstone the PC
            // saw: fine. Anything else must start where the PC last looked.
            let gone_ok = current.as_ref().is_some_and(|d| d.deleted_at.is_some() && d.version == base);
            if at != base && !gone_ok {
                return Ok(Write::Conflict(current));
            }
        }
        if json.is_none() && current.as_ref().is_none_or(|d| d.deleted_at.is_some()) {
            let v = current.map_or(0, |d| d.version);
            tx.commit()?;
            return Ok(Write::Saved(v));
        }
        let version: i64 = tx.query_row(
            "UPDATE cloud_accounts SET version = version + 1 WHERE account_id = ?1 RETURNING version",
            [account],
            |r| r.get(0),
        )?;
        let created = current.as_ref().filter(|d| d.deleted_at.is_none()).map_or(now, |d| d.created_at);
        tx.execute(
            "INSERT INTO cloud_docs(account_id, kind, id, version, created_at, updated_at, deleted_at, json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(account_id, kind, id) DO UPDATE SET version = excluded.version, created_at = excluded.created_at,
               updated_at = excluded.updated_at, deleted_at = excluded.deleted_at, json = excluded.json",
            params![account, kind, id, version, created, now, json.is_none().then_some(now), json],
        )?;
        tx.commit()?;
        Ok(Write::Saved(version))
    }

    /// Old tombstones are forgotten.
    pub(crate) fn cloud_prune_tombstones(&self, before: i64) -> rusqlite::Result<usize> {
        self.c().execute("DELETE FROM cloud_docs WHERE deleted_at IS NOT NULL AND deleted_at < ?1", [before])
    }

    // ── Overlay URLs ─────────────────────────────────────────────────────────────────

    /// The URL generation of an overlay (0 when it has none yet).
    pub(crate) fn cloud_token_generation(&self, account: &str, overlay: &str) -> rusqlite::Result<Option<i64>> {
        self.c()
            .query_row(
                "SELECT generation FROM cloud_overlay_tokens WHERE account_id = ?1 AND overlay_id = ?2",
                params![account, overlay],
                |r| r.get(0),
            )
            .optional()
    }

    /// Sets an overlay's URL hash (replacing the old one, so the old URL stops at once).
    pub(crate) fn cloud_set_token(
        &self,
        account: &str,
        overlay: &str,
        hash: &str,
        generation: i64,
    ) -> rusqlite::Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute(
            "DELETE FROM cloud_overlay_tokens WHERE account_id = ?1 AND overlay_id = ?2",
            params![account, overlay],
        )?;
        tx.execute(
            "INSERT INTO cloud_overlay_tokens(token_hash, account_id, overlay_id, generation) VALUES (?1, ?2, ?3, ?4)",
            params![hash, account, overlay, generation],
        )?;
        tx.commit()
    }

    /// `(account, overlay)` for a URL's hash.
    pub(crate) fn cloud_token(&self, hash: &str) -> rusqlite::Result<Option<(String, String)>> {
        self.c()
            .query_row("SELECT account_id, overlay_id FROM cloud_overlay_tokens WHERE token_hash = ?1", [hash], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
    }

    // ── Channels ─────────────────────────────────────────────────────────────────────

    pub(crate) fn cloud_channels(&self, account: &str) -> rusqlite::Result<Vec<Channel>> {
        let c = self.c();
        let mut s =
            c.prepare(&format!("SELECT {CHANNEL_COLS} FROM cloud_channels WHERE account_id = ?1 ORDER BY platform"))?;
        s.query_map([account], channel)?.collect()
    }

    pub(crate) fn cloud_all_channels(&self) -> rusqlite::Result<Vec<Channel>> {
        let c = self.c();
        let mut s = c.prepare(&format!("SELECT {CHANNEL_COLS} FROM cloud_channels"))?;
        s.query_map([], channel)?.collect()
    }

    pub(crate) fn cloud_channel(&self, platform: &str, channel_id: &str) -> rusqlite::Result<Option<Channel>> {
        self.c()
            .query_row(
                &format!("SELECT {CHANNEL_COLS} FROM cloud_channels WHERE platform = ?1 AND channel_id = ?2"),
                params![platform, channel_id],
                channel,
            )
            .optional()
    }

    pub(crate) fn cloud_account_channel(&self, account: &str, platform: &str) -> rusqlite::Result<Option<Channel>> {
        self.c()
            .query_row(
                &format!("SELECT {CHANNEL_COLS} FROM cloud_channels WHERE account_id = ?1 AND platform = ?2"),
                params![account, platform],
                channel,
            )
            .optional()
    }

    /// Binds a channel to an account: the account's earlier channel on that platform and
    /// another account's binding of this channel are replaced. The replaced rows.
    pub(crate) fn cloud_bind_channel(&self, ch: &Channel) -> rusqlite::Result<Vec<Channel>> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let replaced: Vec<Channel> = {
            let mut s = tx.prepare(&format!(
                "SELECT {CHANNEL_COLS} FROM cloud_channels WHERE (platform = ?1 AND channel_id = ?2) OR (platform = ?1 AND account_id = ?3)"
            ))?;
            s.query_map(params![ch.platform, ch.channel_id, ch.account_id], channel)?
                .collect::<rusqlite::Result<_>>()?
        };
        tx.execute(
            "DELETE FROM cloud_channels WHERE (platform = ?1 AND channel_id = ?2) OR (platform = ?1 AND account_id = ?3)",
            params![ch.platform, ch.channel_id, ch.account_id],
        )?;
        tx.execute(
            "INSERT INTO cloud_channels(platform, channel_id, account_id, login, name, avatar_url, scopes, event_subs, connected_at, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
            params![
                ch.platform,
                ch.channel_id,
                ch.account_id,
                ch.login,
                ch.name,
                ch.avatar_url,
                ch.scopes.join(" "),
                serde_json::to_string(&ch.event_subs).unwrap_or_else(|_| "[]".into()),
                ch.connected_at
            ],
        )?;
        tx.commit()?;
        Ok(replaced)
    }

    pub(crate) fn cloud_set_event_subs(
        &self,
        platform: &str,
        channel_id: &str,
        ids: &[String],
    ) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE cloud_channels SET event_subs = ?3 WHERE platform = ?1 AND channel_id = ?2",
            params![platform, channel_id, serde_json::to_string(ids).unwrap_or_else(|_| "[]".into())],
        )?;
        Ok(())
    }

    pub(crate) fn cloud_set_channel_error(
        &self,
        platform: &str,
        channel_id: &str,
        error: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE cloud_channels SET error = ?3 WHERE platform = ?1 AND channel_id = ?2",
            params![platform, channel_id, error],
        )?;
        Ok(())
    }

    pub(crate) fn cloud_unbind_channel(&self, account: &str, platform: &str) -> rusqlite::Result<Option<Channel>> {
        let ch = self.cloud_account_channel(account, platform)?;
        self.c().execute(
            "DELETE FROM cloud_channels WHERE account_id = ?1 AND platform = ?2",
            params![account, platform],
        )?;
        Ok(ch)
    }

    // ── Media ────────────────────────────────────────────────────────────────────────

    pub(crate) fn cloud_media(&self, account: &str) -> rusqlite::Result<Vec<Media>> {
        let c = self.c();
        let mut s = c.prepare(&format!(
            "SELECT {MEDIA_COLS} FROM cloud_media WHERE account_id = ?1 ORDER BY created_at DESC, id"
        ))?;
        s.query_map([account], media)?.collect()
    }

    pub(crate) fn cloud_media_one(&self, account: &str, id: &str) -> rusqlite::Result<Option<Media>> {
        self.c()
            .query_row(
                &format!("SELECT {MEDIA_COLS} FROM cloud_media WHERE account_id = ?1 AND id = ?2"),
                params![account, id],
                media,
            )
            .optional()
    }

    pub(crate) fn cloud_media_by_sha(&self, account: &str, sha: &str) -> rusqlite::Result<Option<Media>> {
        self.c()
            .query_row(
                &format!("SELECT {MEDIA_COLS} FROM cloud_media WHERE account_id = ?1 AND sha256 = ?2 LIMIT 1"),
                params![account, sha],
                media,
            )
            .optional()
    }

    pub(crate) fn cloud_media_bytes(&self, account: &str) -> rusqlite::Result<i64> {
        self.c()
            .query_row("SELECT COALESCE(SUM(bytes), 0) FROM cloud_media WHERE account_id = ?1", [account], |r| r.get(0))
    }

    /// Adds a file unless it would take the account over `limit` bytes; `false` when it would.
    pub(crate) fn cloud_add_media(&self, account: &str, m: &Media, limit: i64) -> rusqlite::Result<bool> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let used: i64 =
            tx.query_row("SELECT COALESCE(SUM(bytes), 0) FROM cloud_media WHERE account_id = ?1", [account], |r| {
                r.get(0)
            })?;
        if used + m.bytes > limit {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO cloud_media(account_id, id, sha256, kind, mime, bytes, name, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![account, m.id, m.sha256, m.kind, m.mime, m.bytes, m.name, m.created_at],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Deletes a file's row; whether another row still uses its bytes.
    pub(crate) fn cloud_delete_media(&self, account: &str, id: &str) -> rusqlite::Result<Option<(Media, bool)>> {
        let Some(m) = self.cloud_media_one(account, id)? else { return Ok(None) };
        self.c().execute("DELETE FROM cloud_media WHERE account_id = ?1 AND id = ?2", params![account, id])?;
        let shared = self.cloud_media_by_sha(account, &m.sha256)?.is_some();
        Ok(Some((m, shared)))
    }

    /// Relay-wide: files and bytes kept.
    pub(crate) fn cloud_media_totals(&self) -> rusqlite::Result<(i64, i64)> {
        self.c()
            .query_row("SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM cloud_media", [], |r| Ok((r.get(0)?, r.get(1)?)))
    }

    // ── Connect finishes ─────────────────────────────────────────────────────────────

    pub(crate) fn cloud_put_pending(
        &self,
        code_hash: &str,
        account: &str,
        platform: &str,
        payload: &str,
        expires_at: i64,
    ) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO cloud_pending(code_hash, account_id, platform, payload, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![code_hash, account, platform, payload, expires_at],
        )?;
        Ok(())
    }

    /// Takes a finish once: `(account, platform, payload)` while it hasn't expired.
    pub(crate) fn cloud_take_pending(
        &self,
        code_hash: &str,
        now: i64,
    ) -> rusqlite::Result<Option<(String, String, String)>> {
        let row: Option<(String, String, String, i64)> = self
            .c()
            .query_row(
                "DELETE FROM cloud_pending WHERE code_hash = ?1 RETURNING account_id, platform, payload, expires_at",
                [code_hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(row.filter(|r| r.3 > now).map(|(a, p, j, _)| (a, p, j)))
    }

    /// Expired finishes (their sealed sign-ins with them).
    pub(crate) fn cloud_prune_pending(&self, now: i64) -> rusqlite::Result<Vec<(String, String, String)>> {
        let c = self.c();
        let mut s =
            c.prepare("DELETE FROM cloud_pending WHERE expires_at <= ?1 RETURNING account_id, platform, payload")?;
        s.query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
    }
}
