//! The cloud bot's tables (migration 10). Chat is never stored (R-51): only the channel
//! bindings, the sealed refresh tokens, the pushed config, counter changes waiting for a PC
//! and a little relay-wide state (the Twitch conduit's id).

use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::db::Db;
use crate::vault::Sealed;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS bot_channels (sub_id TEXT NOT NULL, platform TEXT NOT NULL, bot_id TEXT, bot_login TEXT, channel_id TEXT, channel_login TEXT, scopes TEXT NOT NULL DEFAULT '', moderation INTEGER NOT NULL DEFAULT 0, subscriptions TEXT NOT NULL DEFAULT '[]', connected_by TEXT NOT NULL, connected_at INTEGER NOT NULL, last_reply_at INTEGER, error TEXT, last_drop TEXT, live_since INTEGER, PRIMARY KEY (sub_id, platform));
CREATE UNIQUE INDEX IF NOT EXISTS bot_channels_channel ON bot_channels(platform, channel_id) WHERE channel_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS bot_tokens (sub_id TEXT NOT NULL, platform TEXT NOT NULL, purpose TEXT NOT NULL, key_v INTEGER NOT NULL, nonce BLOB NOT NULL, ct BLOB NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY (sub_id, platform, purpose));
CREATE TABLE IF NOT EXISTS bot_config (sub_id TEXT PRIMARY KEY, version INTEGER NOT NULL, json TEXT NOT NULL, updated_at INTEGER NOT NULL, enabled INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS bot_counter_changes (id INTEGER PRIMARY KEY AUTOINCREMENT, sub_id TEXT NOT NULL, name TEXT NOT NULL, op TEXT NOT NULL DEFAULT 'set', delta INTEGER NOT NULL DEFAULT 0, value INTEGER NOT NULL, at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS bot_counter_changes_sub ON bot_counter_changes(sub_id, id);
CREATE TABLE IF NOT EXISTS bot_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL);
";

/// The kv key of the Twitch conduit this relay last set up.
pub(crate) const KV_CONDUIT: &str = "twitch_conduit";

/// Error codes that stop the cloud from answering on a channel until it's connected again.
/// Everything else (a refused or dropped message, Twitch being unreachable) is shown but
/// never blocks: the next message tries again, and a good one clears it.
pub(crate) fn blocking(code: &str) -> bool {
    code.ends_with("_signed_out") || code == "twitch_revoked"
}

/// One platform's binding for a subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Channel {
    pub sub_id: String,
    pub platform: String,
    pub bot_id: Option<String>,
    pub bot_login: Option<String>,
    pub channel_id: Option<String>,
    pub channel_login: Option<String>,
    pub scopes: String,
    pub moderation: bool,
    /// Twitch EventSub subscription ids.
    pub subscriptions: Vec<String>,
    pub connected_by: String,
    pub connected_at: i64,
    pub last_reply_at: Option<i64>,
    /// `{code, message, hint, at}`.
    pub error: Option<Value>,
    /// The last message the platform took but didn't show: `{reason, at}`.
    pub last_drop: Option<Value>,
    /// The channel went live then (ms); `None` while offline.
    pub live_since: Option<i64>,
}

impl Channel {
    /// The last error's code, if any.
    pub fn error_code(&self) -> Option<&str> {
        self.error.as_ref().and_then(|e| e["code"].as_str())
    }

    /// The error stops the cloud here (signed out, revoked).
    pub fn blocked(&self) -> bool {
        self.error_code().is_some_and(blocking)
    }

    /// Both accounts are known and nothing blocks: the cloud can answer here.
    pub fn ready(&self) -> bool {
        self.bot_id.is_some() && self.channel_id.is_some() && !self.blocked()
    }
}

const COLS: &str = "sub_id, platform, bot_id, bot_login, channel_id, channel_login, scopes, moderation, subscriptions, connected_by, connected_at, last_reply_at, error, last_drop, live_since";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
    let subs: String = r.get(8)?;
    let err: Option<String> = r.get(12)?;
    let drop: Option<String> = r.get(13)?;
    Ok(Channel {
        sub_id: r.get(0)?,
        platform: r.get(1)?,
        bot_id: r.get(2)?,
        bot_login: r.get(3)?,
        channel_id: r.get(4)?,
        channel_login: r.get(5)?,
        scopes: r.get(6)?,
        moderation: r.get::<_, i64>(7)? != 0,
        subscriptions: serde_json::from_str(&subs).unwrap_or_default(),
        connected_by: r.get(9)?,
        connected_at: r.get(10)?,
        last_reply_at: r.get(11)?,
        error: err.and_then(|e| serde_json::from_str(&e).ok()),
        last_drop: drop.and_then(|e| serde_json::from_str(&e).ok()),
        live_since: r.get(14)?,
    })
}

/// The stored config row: version, JSON, when, and the cloud bot switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigRow {
    pub version: i64,
    pub json: String,
    pub updated_at: i64,
    pub enabled: bool,
}

/// A counter the cloud changed, waiting for the PCs: `add` (by `delta`) or `set` (to `value`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CounterChange {
    pub id: i64,
    pub name: String,
    pub op: String,
    pub delta: i64,
    pub value: i64,
    pub at: i64,
}

/// Why a config push didn't save.
#[derive(Debug)]
pub(crate) enum PushErr {
    /// Someone pushed since: the current version.
    Stale(i64),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for PushErr {
    fn from(e: rusqlite::Error) -> Self {
        PushErr::Db(e)
    }
}

impl Db {
    pub(crate) fn bot_channels(&self, sub: &str) -> rusqlite::Result<Vec<Channel>> {
        let c = self.c();
        let mut q = c.prepare(&format!("SELECT {COLS} FROM bot_channels WHERE sub_id = ?1 ORDER BY platform"))?;
        q.query_map([sub], row)?.collect()
    }

    pub(crate) fn bot_channel(&self, sub: &str, platform: &str) -> rusqlite::Result<Option<Channel>> {
        self.c()
            .query_row(
                &format!("SELECT {COLS} FROM bot_channels WHERE sub_id = ?1 AND platform = ?2"),
                params![sub, platform],
                row,
            )
            .optional()
    }

    /// The binding for a platform channel (whoever's subscription it is).
    pub(crate) fn bot_channel_by_channel(&self, platform: &str, channel: &str) -> rusqlite::Result<Option<Channel>> {
        self.c()
            .query_row(
                &format!("SELECT {COLS} FROM bot_channels WHERE platform = ?1 AND channel_id = ?2"),
                params![platform, channel],
                row,
            )
            .optional()
    }

    /// Every binding (the hourly sweep and the timer loop).
    pub(crate) fn bot_all_channels(&self) -> rusqlite::Result<Vec<Channel>> {
        let c = self.c();
        let mut q = c.prepare(&format!("SELECT {COLS} FROM bot_channels ORDER BY sub_id, platform"))?;
        q.query_map([], row)?.collect()
    }

    /// Sets the speaking account (Twitch's first step, or Kick's only one). A new row starts
    /// without a channel unless one is given.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bot_set_account(
        &self,
        sub: &str,
        platform: &str,
        bot: (&str, &str),
        channel: Option<(&str, &str)>,
        scopes: &str,
        moderation: bool,
        install: &str,
        now: i64,
    ) -> rusqlite::Result<()> {
        let (cid, clogin) = channel.map_or((None, None), |(i, l)| (Some(i), Some(l)));
        self.c().execute(
            "INSERT INTO bot_channels(sub_id, platform, bot_id, bot_login, channel_id, channel_login, scopes, moderation, connected_by, connected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(sub_id, platform) DO UPDATE SET bot_id = excluded.bot_id, bot_login = excluded.bot_login,
               channel_id = COALESCE(excluded.channel_id, bot_channels.channel_id),
               channel_login = COALESCE(excluded.channel_login, bot_channels.channel_login),
               scopes = excluded.scopes, moderation = excluded.moderation, error = NULL, last_drop = NULL,
               connected_by = excluded.connected_by, connected_at = excluded.connected_at",
            params![sub, platform, bot.0, bot.1, cid, clogin, scopes, i64::from(moderation), install, now],
        )?;
        Ok(())
    }

    /// Sets whose chat it is (Twitch's second step). `false` when the row is gone.
    pub(crate) fn bot_set_channel(&self, sub: &str, platform: &str, id: &str, login: &str) -> rusqlite::Result<bool> {
        Ok(self.c().execute(
            "UPDATE bot_channels SET channel_id = ?3, channel_login = ?4, error = NULL, live_since = NULL WHERE sub_id = ?1 AND platform = ?2",
            params![sub, platform, id, login],
        )? > 0)
    }

    /// Unbinds the channel (the bot account stays): no channel, no EventSub ids, no
    /// channel sign-in, and the error that says why.
    pub(crate) fn bot_unbind_channel(&self, sub: &str, platform: &str, error: Option<&Value>) -> rusqlite::Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE bot_channels SET channel_id = NULL, channel_login = NULL, subscriptions = '[]', live_since = NULL, error = ?3
             WHERE sub_id = ?1 AND platform = ?2",
            params![sub, platform, error.map(Value::to_string)],
        )?;
        tx.execute(
            "DELETE FROM bot_tokens WHERE sub_id = ?1 AND platform = ?2 AND purpose = 'channel'",
            params![sub, platform],
        )?;
        tx.commit()
    }

    pub(crate) fn bot_set_subscriptions(&self, sub: &str, platform: &str, ids: &[String]) -> rusqlite::Result<()> {
        let json = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
        self.c().execute(
            "UPDATE bot_channels SET subscriptions = ?3 WHERE sub_id = ?1 AND platform = ?2",
            params![sub, platform, json],
        )?;
        Ok(())
    }

    pub(crate) fn bot_set_error(&self, sub: &str, platform: &str, error: Option<&Value>) -> rusqlite::Result<()> {
        let json = error.map(Value::to_string);
        self.c().execute(
            "UPDATE bot_channels SET error = ?3 WHERE sub_id = ?1 AND platform = ?2",
            params![sub, platform, json],
        )?;
        Ok(())
    }

    /// Clears an error that doesn't block (a refusal, Twitch unreachable); a blocking one stays.
    pub(crate) fn bot_clear_transient_error(&self, sub: &str, platform: &str) -> rusqlite::Result<()> {
        if let Some(ch) = self.bot_channel(sub, platform)?
            && ch.error.is_some()
            && !ch.blocked()
        {
            self.bot_set_error(sub, platform, None)?;
        }
        Ok(())
    }

    /// The platform took a message but didn't show it (AutoMod, slow mode, too long).
    pub(crate) fn bot_set_drop(&self, sub: &str, platform: &str, drop: &Value) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE bot_channels SET last_drop = ?3 WHERE sub_id = ?1 AND platform = ?2",
            params![sub, platform, drop.to_string()],
        )?;
        Ok(())
    }

    /// Live since `at` (ms), or offline with `None`. Kept so timers and `{uptime}` survive a
    /// relay restart mid-stream.
    pub(crate) fn bot_set_live(&self, sub: &str, platform: &str, at: Option<i64>) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE bot_channels SET live_since = ?3 WHERE sub_id = ?1 AND platform = ?2",
            params![sub, platform, at],
        )?;
        Ok(())
    }

    pub(crate) fn bot_replied(&self, sub: &str, platform: &str, at: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE bot_channels SET last_reply_at = ?3 WHERE sub_id = ?1 AND platform = ?2",
            params![sub, platform, at],
        )?;
        Ok(())
    }

    /// Deletes a platform's binding and its tokens.
    pub(crate) fn bot_forget_channel(&self, sub: &str, platform: &str) -> rusqlite::Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM bot_channels WHERE sub_id = ?1 AND platform = ?2", params![sub, platform])?;
        tx.execute("DELETE FROM bot_tokens WHERE sub_id = ?1 AND platform = ?2", params![sub, platform])?;
        tx.commit()
    }

    /// Everything the cloud bot keeps for a subscription.
    pub(crate) fn bot_forget(&self, sub: &str) -> rusqlite::Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for t in ["bot_channels", "bot_tokens", "bot_config", "bot_counter_changes"] {
            tx.execute(&format!("DELETE FROM {t} WHERE sub_id = ?1"), [sub])?;
        }
        tx.commit()
    }

    pub(crate) fn bot_put_token(&self, sub: &str, platform: &str, purpose: &str, s: &Sealed) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO bot_tokens(sub_id, platform, purpose, key_v, nonce, ct, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(sub_id, platform, purpose) DO UPDATE SET key_v = excluded.key_v, nonce = excluded.nonce, ct = excluded.ct, updated_at = excluded.updated_at",
            params![sub, platform, purpose, s.key_v, s.nonce, s.ct, crate::now_ms()],
        )?;
        Ok(())
    }

    pub(crate) fn bot_token(&self, sub: &str, platform: &str, purpose: &str) -> rusqlite::Result<Option<Sealed>> {
        self.c()
            .query_row(
                "SELECT key_v, nonce, ct FROM bot_tokens WHERE sub_id = ?1 AND platform = ?2 AND purpose = ?3",
                params![sub, platform, purpose],
                |r| Ok(Sealed { key_v: r.get(0)?, nonce: r.get(1)?, ct: r.get(2)? }),
            )
            .optional()
    }

    /// The token purposes kept for a platform (`bot`, `channel`).
    pub(crate) fn bot_token_purposes(&self, sub: &str, platform: &str) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut q = c.prepare("SELECT purpose FROM bot_tokens WHERE sub_id = ?1 AND platform = ?2 ORDER BY purpose")?;
        q.query_map(params![sub, platform], |r| r.get(0))?.collect()
    }

    pub(crate) fn bot_config(&self, sub: &str) -> rusqlite::Result<Option<ConfigRow>> {
        self.c()
            .query_row("SELECT version, json, updated_at, enabled FROM bot_config WHERE sub_id = ?1", [sub], |r| {
                Ok(ConfigRow {
                    version: r.get(0)?,
                    json: r.get(1)?,
                    updated_at: r.get(2)?,
                    enabled: r.get::<_, i64>(3)? != 0,
                })
            })
            .optional()
    }

    /// Saves a pushed config if `base` is the stored version (0 = none yet), as `base + 1`,
    /// and drops the counter changes the PC already applied, in one transaction.
    pub(crate) fn bot_push_config(
        &self,
        sub: &str,
        base: i64,
        json: &str,
        counters_seen: i64,
        now: i64,
    ) -> Result<i64, PushErr> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: i64 = tx
            .query_row("SELECT version FROM bot_config WHERE sub_id = ?1", [sub], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        if current != base {
            return Err(PushErr::Stale(current));
        }
        let next = base + 1;
        tx.execute(
            "INSERT INTO bot_config(sub_id, version, json, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(sub_id) DO UPDATE SET version = excluded.version, json = excluded.json, updated_at = excluded.updated_at",
            params![sub, next, json, now],
        )?;
        if counters_seen > 0 {
            tx.execute("DELETE FROM bot_counter_changes WHERE sub_id = ?1 AND id <= ?2", params![sub, counters_seen])?;
        }
        tx.commit()?;
        Ok(next)
    }

    pub(crate) fn bot_set_enabled(&self, sub: &str, enabled: bool, now: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO bot_config(sub_id, version, json, updated_at, enabled) VALUES (?1, 0, '{}', ?2, ?3)
             ON CONFLICT(sub_id) DO UPDATE SET enabled = excluded.enabled",
            params![sub, now, i64::from(enabled)],
        )?;
        Ok(())
    }

    /// Subscriptions with the cloud bot switched on.
    pub(crate) fn bot_enabled_subs(&self) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut q = c.prepare("SELECT sub_id FROM bot_config WHERE enabled = 1 ORDER BY sub_id")?;
        q.query_map([], |r| r.get(0))?.collect()
    }

    /// Records a counter the cloud changed (`op` `add` with `delta`, or `set`; `value` is the
    /// result); its id.
    pub(crate) fn bot_add_counter_change(
        &self,
        sub: &str,
        name: &str,
        op: &str,
        delta: i64,
        value: i64,
        at: i64,
    ) -> rusqlite::Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO bot_counter_changes(sub_id, name, op, delta, value, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![sub, name, op, delta, value, at],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// Changes not yet applied by a PC, oldest first.
    pub(crate) fn bot_counter_changes(&self, sub: &str) -> rusqlite::Result<Vec<CounterChange>> {
        let c = self.c();
        let mut q =
            c.prepare("SELECT id, name, op, delta, value, at FROM bot_counter_changes WHERE sub_id = ?1 ORDER BY id")?;
        q.query_map([sub], |r| {
            Ok(CounterChange {
                id: r.get(0)?,
                name: r.get(1)?,
                op: r.get(2)?,
                delta: r.get(3)?,
                value: r.get(4)?,
                at: r.get(5)?,
            })
        })?
        .collect()
    }

    /// The installs sharing a subscription (where counter changes go).
    pub(crate) fn bot_installs(&self, sub: &str) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut q = c.prepare("SELECT install_id FROM pro_installs WHERE sub_id = ?1 ORDER BY linked_at")?;
        q.query_map([sub], |r| r.get(0))?.collect()
    }

    /// The Kick channel this install connected in Accounts (its webhooks reach the relay).
    /// The same one every time when an install has more than one.
    pub(crate) fn bot_kick_channel_of(&self, install: &str) -> rusqlite::Result<Option<String>> {
        self.c()
            .query_row(
                "SELECT broadcaster_id FROM kick_owners WHERE install_id = ?1 ORDER BY broadcaster_id LIMIT 1",
                [install],
                |r| r.get(0),
            )
            .optional()
    }

    /// Counter changes older than this are dropped (no PC came back for a month).
    pub(crate) fn bot_prune(&self, before: i64) -> rusqlite::Result<()> {
        self.c().execute("DELETE FROM bot_counter_changes WHERE at < ?1", [before])?;
        Ok(())
    }

    /// Every subscription with anything kept (for the lapse sweep).
    pub(crate) fn bot_subs(&self) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut q = c.prepare(
            "SELECT sub_id FROM bot_channels UNION SELECT sub_id FROM bot_config UNION SELECT sub_id FROM bot_counter_changes ORDER BY 1",
        )?;
        q.query_map([], |r| r.get(0))?.collect()
    }

    pub(crate) fn bot_kv(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.c().query_row("SELECT value FROM bot_kv WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    pub(crate) fn bot_put_kv(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO bot_kv(key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, crate::now_ms()],
        )?;
        Ok(())
    }
}
