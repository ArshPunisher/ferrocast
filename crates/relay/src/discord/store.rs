//! What the Discord bot keeps (R-51), per subscription:
//! - `discord_guilds`: the one connected server (id, name, icon hash), the go-live channel,
//!   switch and line, the viewer link code, which install connected it, and the last
//!   "a role couldn't be given" problem (rule id, Discord's reason, when);
//! - `discord_rules`: up to 10 perk rules;
//! - `discord_links`: per viewer who linked themselves, their Discord user id and the ids of
//!   the Twitch / YouTube accounts connected to it (no names, no tokens);
//! - `discord_grants`: which role went to which Discord user and when it ends (and, once it
//!   has ended, how often taking it back failed and when to try again);
//! - `discord_posts`: the last go-live message per platform (to edit it at the end).
//!
//! A grant whose role was taken off because the subscription's Pro ended is kept with
//! `paused_at` set, so the role goes back on if Pro comes back before it would have ended.

use rusqlite::{OptionalExtension, Row, params};

use crate::db::Db;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS discord_guilds (sub_id TEXT PRIMARY KEY, guild_id TEXT NOT NULL UNIQUE, guild_name TEXT NOT NULL, icon TEXT, link_code TEXT NOT NULL UNIQUE, channel_id TEXT, go_live INTEGER NOT NULL DEFAULT 0, go_live_message TEXT, connected_by TEXT NOT NULL, connected_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, perk_issue TEXT);
CREATE TABLE IF NOT EXISTS discord_rules (id TEXT PRIMARY KEY, sub_id TEXT NOT NULL, kind TEXT NOT NULL, platform TEXT, tier TEXT, min_minor INTEGER, currency TEXT, role_id TEXT NOT NULL, keep_days INTEGER NOT NULL, created_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS discord_rules_sub ON discord_rules(sub_id, created_at);
CREATE TABLE IF NOT EXISTS discord_links (guild_id TEXT NOT NULL, discord_user_id TEXT NOT NULL, platform TEXT NOT NULL, platform_user_id TEXT NOT NULL, linked_at INTEGER NOT NULL, PRIMARY KEY (guild_id, discord_user_id, platform));
CREATE INDEX IF NOT EXISTS discord_links_account ON discord_links(guild_id, platform, platform_user_id);
CREATE TABLE IF NOT EXISTS discord_grants (guild_id TEXT NOT NULL, discord_user_id TEXT NOT NULL, role_id TEXT NOT NULL, rule_id TEXT NOT NULL, granted_at INTEGER NOT NULL, expires_at INTEGER, attempts INTEGER NOT NULL DEFAULT 0, retry_at INTEGER, PRIMARY KEY (guild_id, discord_user_id, role_id));
CREATE INDEX IF NOT EXISTS discord_grants_expiry ON discord_grants(expires_at);
CREATE TABLE IF NOT EXISTS discord_posts (sub_id TEXT NOT NULL, platform TEXT NOT NULL, channel_id TEXT NOT NULL, message_id TEXT NOT NULL, started_at INTEGER NOT NULL, posted_at INTEGER NOT NULL, ended_at INTEGER, post TEXT NOT NULL, PRIMARY KEY (sub_id, platform));
";

/// Migration 13: when a grant's role was taken off because Pro ended (`NULL`: it's on).
pub(crate) const SCHEMA_PAUSED: &str = "
ALTER TABLE discord_grants ADD COLUMN paused_at INTEGER;
";

/// A subscription's connected server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuildRow {
    pub sub_id: String,
    pub guild_id: String,
    pub name: String,
    pub icon: Option<String>,
    pub link_code: String,
    pub channel_id: Option<String>,
    pub go_live: bool,
    pub go_live_message: Option<String>,
    /// The install that added the bot (shown as its PC code, never as the id).
    pub connected_by: String,
    pub connected_at: i64,
    /// The last perk problem, JSON `{rule_id, code, at}`; `None` when roles go out fine.
    pub perk_issue: Option<String>,
}

const GUILD_COLS: &str = "sub_id, guild_id, guild_name, icon, link_code, channel_id, go_live, go_live_message, connected_by, connected_at, perk_issue";

fn guild_row(r: &Row<'_>) -> rusqlite::Result<GuildRow> {
    Ok(GuildRow {
        sub_id: r.get(0)?,
        guild_id: r.get(1)?,
        name: r.get(2)?,
        icon: r.get(3)?,
        link_code: r.get(4)?,
        channel_id: r.get(5)?,
        go_live: r.get::<_, i64>(6)? != 0,
        go_live_message: r.get(7)?,
        connected_by: r.get(8)?,
        connected_at: r.get(9)?,
        perk_issue: r.get(10)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RuleRow {
    pub id: String,
    pub kind: String,
    pub platform: Option<String>,
    pub tier: Option<String>,
    pub min_minor: Option<i64>,
    pub currency: Option<String>,
    pub role_id: String,
    pub keep_days: i64,
    pub created_at: i64,
}

const RULE_COLS: &str = "id, kind, platform, tier, min_minor, currency, role_id, keep_days, created_at";

fn rule_row(r: &Row<'_>) -> rusqlite::Result<RuleRow> {
    Ok(RuleRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        platform: r.get(2)?,
        tier: r.get(3)?,
        min_minor: r.get(4)?,
        currency: r.get(5)?,
        role_id: r.get(6)?,
        keep_days: r.get(7)?,
        created_at: r.get(8)?,
    })
}

/// A role given out, as the hourly sweep sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrantRow {
    pub guild_id: String,
    pub discord_user_id: String,
    pub role_id: String,
    pub expires_at: Option<i64>,
    /// Failed removals so far (the sweep backs off, then gives up).
    pub attempts: i64,
}

/// What the sweep's bookkeeping found after a removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ungranted {
    /// The grant is gone from the list.
    Forgotten,
    /// A new perk renewed it while the role was being taken back: it must go back on.
    Renewed,
}

const GRANT_COLS: &str = "guild_id, discord_user_id, role_id, expires_at, attempts";

fn grant_row(r: &Row<'_>) -> rusqlite::Result<GrantRow> {
    Ok(GrantRow {
        guild_id: r.get(0)?,
        discord_user_id: r.get(1)?,
        role_id: r.get(2)?,
        expires_at: r.get(3)?,
        attempts: r.get(4)?,
    })
}

/// The last go-live message on one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PostRow {
    pub channel_id: String,
    pub message_id: String,
    pub started_at: i64,
    pub posted_at: i64,
    pub ended_at: Option<i64>,
    /// What the message showed (JSON), to rebuild it when the stream ends.
    pub post: String,
}

/// How saving a newly connected server went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Claimed {
    Yes,
    /// Another subscription has this server.
    Taken,
    /// This subscription already has a different server (one per subscription).
    HasOther,
}

impl Db {
    pub(crate) fn discord_guild(&self, sub: &str) -> rusqlite::Result<Option<GuildRow>> {
        self.c()
            .query_row(&format!("SELECT {GUILD_COLS} FROM discord_guilds WHERE sub_id = ?1"), [sub], guild_row)
            .optional()
    }

    pub(crate) fn discord_guild_by_code(&self, code: &str) -> rusqlite::Result<Option<GuildRow>> {
        self.c()
            .query_row(&format!("SELECT {GUILD_COLS} FROM discord_guilds WHERE link_code = ?1"), [code], guild_row)
            .optional()
    }

    /// The subscription that has this server, if any.
    pub(crate) fn discord_guild_owner(&self, guild: &str) -> rusqlite::Result<Option<String>> {
        self.c().query_row("SELECT sub_id FROM discord_guilds WHERE guild_id = ?1", [guild], |r| r.get(0)).optional()
    }

    pub(crate) fn discord_guilds(&self) -> rusqlite::Result<Vec<GuildRow>> {
        let c = self.c();
        let mut q = c.prepare(&format!("SELECT {GUILD_COLS} FROM discord_guilds"))?;
        q.query_map([], guild_row)?.collect()
    }

    /// Records the server a subscription's bot was added to (same server again: refreshes
    /// its name and icon, keeps everything else).
    pub(crate) fn discord_claim(
        &self,
        sub: &str,
        install: &str,
        guild: &super::api::Guild,
        link_code: &str,
        now: i64,
    ) -> rusqlite::Result<Claimed> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let owner: Option<String> = tx
            .query_row("SELECT sub_id FROM discord_guilds WHERE guild_id = ?1", [&guild.id], |r| r.get(0))
            .optional()?;
        if owner.as_deref().is_some_and(|o| o != sub) {
            return Ok(Claimed::Taken);
        }
        let mine: Option<String> =
            tx.query_row("SELECT guild_id FROM discord_guilds WHERE sub_id = ?1", [sub], |r| r.get(0)).optional()?;
        match mine {
            Some(g) if g != guild.id => return Ok(Claimed::HasOther),
            Some(_) => {
                tx.execute(
                    "UPDATE discord_guilds SET guild_name = ?2, icon = ?3, updated_at = ?4, perk_issue = NULL WHERE sub_id = ?1",
                    params![sub, guild.name, guild.icon, now],
                )?;
            }
            None => {
                tx.execute(
                    "INSERT INTO discord_guilds(sub_id, guild_id, guild_name, icon, link_code, connected_by, connected_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    params![sub, guild.id, guild.name, guild.icon, link_code, install, now],
                )?;
            }
        }
        tx.commit()?;
        Ok(Claimed::Yes)
    }

    pub(crate) fn discord_set_channel(&self, sub: &str, channel: &str, now: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE discord_guilds SET channel_id = ?2, updated_at = ?3 WHERE sub_id = ?1",
            params![sub, channel, now],
        )?;
        Ok(())
    }

    pub(crate) fn discord_set_go_live(
        &self,
        sub: &str,
        on: bool,
        message: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE discord_guilds SET go_live = ?2, go_live_message = ?3, updated_at = ?4 WHERE sub_id = ?1",
            params![sub, i64::from(on), message, now],
        )?;
        Ok(())
    }

    /// Everything the subscription's server had: the server, rules, links, grants, posts.
    pub(crate) fn discord_forget(&self, sub: &str) -> rusqlite::Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let guild: Option<String> =
            tx.query_row("SELECT guild_id FROM discord_guilds WHERE sub_id = ?1", [sub], |r| r.get(0)).optional()?;
        if let Some(g) = guild {
            tx.execute("DELETE FROM discord_links WHERE guild_id = ?1", [&g])?;
            tx.execute("DELETE FROM discord_grants WHERE guild_id = ?1", [&g])?;
        }
        tx.execute("DELETE FROM discord_guilds WHERE sub_id = ?1", [sub])?;
        tx.execute("DELETE FROM discord_rules WHERE sub_id = ?1", [sub])?;
        tx.execute("DELETE FROM discord_posts WHERE sub_id = ?1", [sub])?;
        tx.commit()
    }

    /// Records (or, with `None`, clears) the "a role couldn't be given" problem.
    pub(crate) fn discord_set_perk_issue(&self, sub: &str, issue: Option<&str>) -> rusqlite::Result<()> {
        self.c().execute("UPDATE discord_guilds SET perk_issue = ?2 WHERE sub_id = ?1", params![sub, issue])?;
        Ok(())
    }

    /// Clears the perk problem if it's about this rule (edited, deleted, or now working).
    pub(crate) fn discord_clear_perk_issue_for(&self, sub: &str, rule: &str) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE discord_guilds SET perk_issue = NULL WHERE sub_id = ?1 AND json_extract(perk_issue, '$.rule_id') = ?2",
            params![sub, rule],
        )?;
        Ok(())
    }

    // ── Rules ────────────────────────────────────────────────────────────────────────

    pub(crate) fn discord_rules(&self, sub: &str) -> rusqlite::Result<Vec<RuleRow>> {
        let c = self.c();
        let mut q =
            c.prepare(&format!("SELECT {RULE_COLS} FROM discord_rules WHERE sub_id = ?1 ORDER BY created_at, id"))?;
        q.query_map([sub], rule_row)?.collect()
    }

    /// Adds a rule if the subscription has fewer than `max`; false when full.
    pub(crate) fn discord_add_rule(&self, sub: &str, r: &RuleRow, max: i64) -> rusqlite::Result<bool> {
        let n = self.c().execute(
            "INSERT INTO discord_rules(id, sub_id, kind, platform, tier, min_minor, currency, role_id, keep_days, created_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
             WHERE (SELECT COUNT(*) FROM discord_rules WHERE sub_id = ?2) < ?11",
            params![
                r.id,
                sub,
                r.kind,
                r.platform,
                r.tier,
                r.min_minor,
                r.currency,
                r.role_id,
                r.keep_days,
                r.created_at,
                max
            ],
        )?;
        Ok(n > 0)
    }

    /// Replaces a rule's fields (keeps its id and creation time); false when it isn't there.
    pub(crate) fn discord_update_rule(&self, sub: &str, r: &RuleRow) -> rusqlite::Result<bool> {
        let n = self.c().execute(
            "UPDATE discord_rules SET kind = ?3, platform = ?4, tier = ?5, min_minor = ?6, currency = ?7, role_id = ?8,
               keep_days = ?9 WHERE sub_id = ?1 AND id = ?2",
            params![sub, r.id, r.kind, r.platform, r.tier, r.min_minor, r.currency, r.role_id, r.keep_days],
        )?;
        Ok(n > 0)
    }

    pub(crate) fn discord_delete_rule(&self, sub: &str, id: &str) -> rusqlite::Result<bool> {
        Ok(self.c().execute("DELETE FROM discord_rules WHERE sub_id = ?1 AND id = ?2", params![sub, id])? > 0)
    }

    // ── Links ────────────────────────────────────────────────────────────────────────

    /// Discord users with at least one linked account.
    pub(crate) fn discord_linked(&self, guild: &str) -> rusqlite::Result<i64> {
        self.c().query_row(
            "SELECT COUNT(DISTINCT discord_user_id) FROM discord_links WHERE guild_id = ?1",
            [guild],
            |r| r.get(0),
        )
    }

    /// Replaces a viewer's links with `accounts` (`(platform, platform user id)`); an account
    /// linked to another Discord user before moves to this one. False (nothing changed) when
    /// the server already has `max` other linked users.
    pub(crate) fn discord_link(
        &self,
        guild: &str,
        user: &str,
        accounts: &[(String, String)],
        max: i64,
        now: i64,
    ) -> rusqlite::Result<bool> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let others: i64 = tx.query_row(
            "SELECT COUNT(DISTINCT discord_user_id) FROM discord_links WHERE guild_id = ?1 AND discord_user_id != ?2",
            params![guild, user],
            |r| r.get(0),
        )?;
        if others >= max && !accounts.is_empty() {
            return Ok(false);
        }
        tx.execute("DELETE FROM discord_links WHERE guild_id = ?1 AND discord_user_id = ?2", params![guild, user])?;
        for (platform, id) in accounts {
            tx.execute(
                "DELETE FROM discord_links WHERE guild_id = ?1 AND platform = ?2 AND platform_user_id = ?3",
                params![guild, platform, id],
            )?;
            tx.execute(
                "INSERT INTO discord_links(guild_id, discord_user_id, platform, platform_user_id, linked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![guild, user, platform, id, now],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// The Discord users linked to a platform account in a server.
    pub(crate) fn discord_users_for(
        &self,
        guild: &str,
        platform: &str,
        platform_user: &str,
    ) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut q = c.prepare(
            "SELECT discord_user_id FROM discord_links WHERE guild_id = ?1 AND platform = ?2 AND platform_user_id = ?3",
        )?;
        q.query_map(params![guild, platform, platform_user], |r| r.get(0))?.collect()
    }

    // ── Grants ───────────────────────────────────────────────────────────────────────

    /// Records a role given out. The later end wins; `None` (kept for good) beats any date. A
    /// renewal starts the sweep's failure count over.
    pub(crate) fn discord_grant(
        &self,
        guild: &str,
        user: &str,
        role: &str,
        rule: &str,
        expires_at: Option<i64>,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO discord_grants(guild_id, discord_user_id, role_id, rule_id, granted_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(guild_id, discord_user_id, role_id) DO UPDATE SET rule_id = excluded.rule_id,
               granted_at = excluded.granted_at, attempts = 0, retry_at = NULL, paused_at = NULL,
               expires_at = CASE WHEN discord_grants.expires_at IS NULL OR excluded.expires_at IS NULL THEN NULL
                                 ELSE MAX(discord_grants.expires_at, excluded.expires_at) END",
            params![guild, user, role, rule, now, expires_at],
        )?;
        Ok(())
    }

    /// Ended grants whose next try is due, the longest-waiting first (a retry counts from its
    /// retry time, so ones failing for good can't keep new ones waiting).
    pub(crate) fn discord_grants_due(&self, now: i64, limit: i64) -> rusqlite::Result<Vec<GrantRow>> {
        let c = self.c();
        let mut q = c.prepare(&format!(
            "SELECT {GRANT_COLS} FROM discord_grants
             WHERE expires_at IS NOT NULL AND expires_at <= ?1 AND (retry_at IS NULL OR retry_at <= ?1)
             ORDER BY COALESCE(retry_at, expires_at), expires_at LIMIT ?2"
        ))?;
        q.query_map(params![now, limit], grant_row)?.collect()
    }

    #[cfg(test)]
    pub(crate) fn discord_grant_row(&self, guild: &str, user: &str, role: &str) -> rusqlite::Result<Option<GrantRow>> {
        self.c()
            .query_row(
                &format!(
                    "SELECT {GRANT_COLS} FROM discord_grants WHERE guild_id = ?1 AND discord_user_id = ?2 AND role_id = ?3"
                ),
                params![guild, user, role],
                grant_row,
            )
            .optional()
    }

    /// Drops a grant the sweep removed, unless it was renewed meanwhile (a later end, or kept
    /// for good): then the role has to go back on.
    pub(crate) fn discord_ungrant(&self, g: &GrantRow) -> rusqlite::Result<Ungranted> {
        let c = self.c();
        let n = c.execute(
            "DELETE FROM discord_grants WHERE guild_id = ?1 AND discord_user_id = ?2 AND role_id = ?3
               AND expires_at IS NOT NULL AND expires_at <= ?4",
            params![g.guild_id, g.discord_user_id, g.role_id, g.expires_at.unwrap_or(i64::MAX)],
        )?;
        if n > 0 {
            return Ok(Ungranted::Forgotten);
        }
        let still: Option<i64> = c
            .query_row(
                "SELECT 1 FROM discord_grants WHERE guild_id = ?1 AND discord_user_id = ?2 AND role_id = ?3",
                params![g.guild_id, g.discord_user_id, g.role_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(if still.is_some() { Ungranted::Renewed } else { Ungranted::Forgotten })
    }

    /// A removal failed: try again at `retry_at`, or forget the grant after `max` tries (the
    /// role then stays; the streamer can take it off by hand). True when it was forgotten.
    /// A grant renewed meanwhile isn't touched.
    pub(crate) fn discord_grant_failed(&self, g: &GrantRow, retry_at: i64, max: i64) -> rusqlite::Result<bool> {
        let c = self.c();
        let same =
            "guild_id = ?1 AND discord_user_id = ?2 AND role_id = ?3 AND expires_at IS NOT NULL AND expires_at <= ?4";
        let end = g.expires_at.unwrap_or(i64::MAX);
        if g.attempts + 1 >= max {
            let n = c.execute(
                &format!("DELETE FROM discord_grants WHERE {same}"),
                params![g.guild_id, g.discord_user_id, g.role_id, end],
            )?;
            return Ok(n > 0);
        }
        c.execute(
            &format!("UPDATE discord_grants SET attempts = attempts + 1, retry_at = ?5 WHERE {same}"),
            params![g.guild_id, g.discord_user_id, g.role_id, end, retry_at],
        )?;
        Ok(false)
    }

    // ── Pausing grants while Pro is off ──────────────────────────────────────────────

    /// Roles still on in `guild` that haven't ended, whose next try at taking them off (Pro
    /// ended) is due.
    pub(crate) fn discord_grants_to_pause(&self, guild: &str, now: i64, limit: i64) -> rusqlite::Result<Vec<GrantRow>> {
        let c = self.c();
        let mut q = c.prepare(&format!(
            "SELECT {GRANT_COLS} FROM discord_grants
             WHERE guild_id = ?1 AND paused_at IS NULL AND (expires_at IS NULL OR expires_at > ?2)
               AND (retry_at IS NULL OR retry_at <= ?2)
             ORDER BY granted_at, discord_user_id, role_id LIMIT ?3"
        ))?;
        q.query_map(params![guild, now, limit], grant_row)?.collect()
    }

    /// The role came off because Pro ended (or, `gave_up`, it can't be taken off): the grant
    /// waits for Pro to come back.
    pub(crate) fn discord_pause_grant(&self, g: &GrantRow, now: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE discord_grants SET paused_at = ?4, attempts = 0, retry_at = NULL
             WHERE guild_id = ?1 AND discord_user_id = ?2 AND role_id = ?3 AND paused_at IS NULL",
            params![g.guild_id, g.discord_user_id, g.role_id, now],
        )?;
        Ok(())
    }

    /// Taking the role off failed: try again at `retry_at`, or after `max` tries leave it on
    /// and count the grant as paused (so it isn't tried every hour). True when it gave up.
    pub(crate) fn discord_pause_failed(
        &self,
        g: &GrantRow,
        now: i64,
        retry_at: i64,
        max: i64,
    ) -> rusqlite::Result<bool> {
        if g.attempts + 1 >= max {
            self.discord_pause_grant(g, now)?;
            return Ok(true);
        }
        self.c().execute(
            "UPDATE discord_grants SET attempts = attempts + 1, retry_at = ?4
             WHERE guild_id = ?1 AND discord_user_id = ?2 AND role_id = ?3 AND paused_at IS NULL",
            params![g.guild_id, g.discord_user_id, g.role_id, retry_at],
        )?;
        Ok(false)
    }

    /// Paused grants in `guild` that haven't ended: their roles go back on with Pro.
    pub(crate) fn discord_paused_grants(&self, guild: &str, now: i64, limit: i64) -> rusqlite::Result<Vec<GrantRow>> {
        let c = self.c();
        let mut q = c.prepare(&format!(
            "SELECT {GRANT_COLS} FROM discord_grants
             WHERE guild_id = ?1 AND paused_at IS NOT NULL AND (expires_at IS NULL OR expires_at > ?2)
               AND (retry_at IS NULL OR retry_at <= ?2)
             ORDER BY COALESCE(retry_at, granted_at), discord_user_id, role_id LIMIT ?3"
        ))?;
        q.query_map(params![guild, now, limit], grant_row)?.collect()
    }

    /// Giving a paused role back failed: try again at `retry_at`, or after `max` tries forget
    /// the grant (the role stays off). True when it was forgotten.
    pub(crate) fn discord_resume_failed(&self, g: &GrantRow, retry_at: i64, max: i64) -> rusqlite::Result<bool> {
        if g.attempts + 1 >= max {
            self.discord_drop_grant(g)?;
            return Ok(true);
        }
        self.c().execute(
            "UPDATE discord_grants SET attempts = attempts + 1, retry_at = ?4
             WHERE guild_id = ?1 AND discord_user_id = ?2 AND role_id = ?3 AND paused_at IS NOT NULL",
            params![g.guild_id, g.discord_user_id, g.role_id, retry_at],
        )?;
        Ok(false)
    }

    /// The role is back on (Pro is back).
    pub(crate) fn discord_unpause_grant(&self, g: &GrantRow) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE discord_grants SET paused_at = NULL, attempts = 0, retry_at = NULL
             WHERE guild_id = ?1 AND discord_user_id = ?2 AND role_id = ?3",
            params![g.guild_id, g.discord_user_id, g.role_id],
        )?;
        Ok(())
    }

    /// A paused grant that can't come back (the member left, the role was deleted).
    pub(crate) fn discord_drop_grant(&self, g: &GrantRow) -> rusqlite::Result<()> {
        self.c().execute(
            "DELETE FROM discord_grants WHERE guild_id = ?1 AND discord_user_id = ?2 AND role_id = ?3
               AND paused_at IS NOT NULL",
            params![g.guild_id, g.discord_user_id, g.role_id],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn discord_grant_paused(&self, guild: &str, user: &str, role: &str) -> rusqlite::Result<Option<bool>> {
        self.c()
            .query_row(
                "SELECT paused_at IS NOT NULL FROM discord_grants WHERE guild_id = ?1 AND discord_user_id = ?2 AND role_id = ?3",
                params![guild, user, role],
                |r| r.get(0),
            )
            .optional()
    }

    // ── Go-live posts ────────────────────────────────────────────────────────────────

    pub(crate) fn discord_post(&self, sub: &str, platform: &str) -> rusqlite::Result<Option<PostRow>> {
        self.c()
            .query_row(
                "SELECT channel_id, message_id, started_at, posted_at, ended_at, post FROM discord_posts
                 WHERE sub_id = ?1 AND platform = ?2",
                params![sub, platform],
                |r| {
                    Ok(PostRow {
                        channel_id: r.get(0)?,
                        message_id: r.get(1)?,
                        started_at: r.get(2)?,
                        posted_at: r.get(3)?,
                        ended_at: r.get(4)?,
                        post: r.get(5)?,
                    })
                },
            )
            .optional()
    }

    pub(crate) fn discord_save_post(&self, sub: &str, platform: &str, p: &PostRow) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO discord_posts(sub_id, platform, channel_id, message_id, started_at, posted_at, ended_at, post)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(sub_id, platform) DO UPDATE SET channel_id = excluded.channel_id,
               message_id = excluded.message_id, started_at = excluded.started_at, posted_at = excluded.posted_at,
               ended_at = excluded.ended_at, post = excluded.post",
            params![sub, platform, p.channel_id, p.message_id, p.started_at, p.posted_at, p.ended_at, p.post],
        )?;
        Ok(())
    }

    /// Go-live posts made before `before` (their streams are long over).
    pub(crate) fn discord_prune_posts(&self, before: i64) -> rusqlite::Result<()> {
        self.c().execute("DELETE FROM discord_posts WHERE posted_at < ?1", [before])?;
        Ok(())
    }
}
