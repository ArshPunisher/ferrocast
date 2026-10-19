//! The relay database's numbered migrations, tracked in `PRAGMA user_version` (R-34).
//!
//! Migration 1 is every table the relay had before migrations existed, written with
//! `CREATE … IF NOT EXISTS`, so a server that already has them (at `user_version` 0) just
//! moves to 1. Each later feature adds its own `SCHEMA` here as the next number; a migration
//! never changes or drops what an older one made, so an older relay (a rollback) still runs
//! on a newer database. Each runs in its own transaction with its version bump, and a copy
//! of the database is taken first (`<db>.before-v<N>.from-v<M>.<ms>`, never overwritten).

use std::path::Path;

use rusqlite::Connection;

pub(crate) struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static [&'static str],
}

/// In order; append only.
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "installs, Kick, queue, tips, Pro billing",
        sql: &[crate::db::SCHEMA, crate::billing::store::SCHEMA],
    },
    Migration { version: 2, name: "Pro quota counters", sql: &[crate::quota::SCHEMA] },
    Migration {
        version: 3,
        name: "Pro tip page branding and goals",
        sql: &[crate::tips::brand::SCHEMA, crate::tips::goal::SCHEMA],
    },
    Migration { version: 4, name: "Pro stream history", sql: &[crate::history::SCHEMA] },
    Migration { version: 5, name: "Pro cloud backups", sql: &[crate::backups::SCHEMA] },
    Migration { version: 6, name: "Pro voices: relay-wide character count", sql: &[crate::tts::SCHEMA] },
    Migration { version: 7, name: "Pro dashboard from anywhere", sql: &[crate::remote::SCHEMA] },
    Migration { version: 8, name: "Pro Discord bot: servers, rules, links, grants", sql: &[crate::discord::SCHEMA] },
    Migration { version: 9, name: "Pro weekly email: sealed addresses, sends", sql: &[crate::mail::SCHEMA] },
    Migration {
        version: 10,
        name: "Pro cloud bot: channels, sealed sign-ins, config, counter changes",
        sql: &[crate::bot::SCHEMA],
    },
];

pub(crate) fn latest() -> i64 {
    MIGRATIONS.last().map_or(0, |m| m.version)
}

pub(crate) fn version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
}

/// Brings the database at `path` up to [`latest`].
pub(crate) fn run(conn: &mut Connection, path: &Path) -> rusqlite::Result<()> {
    let current = version(conn)?;
    if current > latest() {
        // A rollback to an older relay: the newer tables are left alone (migrations only add).
        tracing::warn!(current, latest = latest(), "the relay database is newer than this relay; running anyway");
        return Ok(());
    }
    let pending: Vec<&Migration> = MIGRATIONS.iter().filter(|m| m.version > current).collect();
    if pending.is_empty() {
        return Ok(());
    }
    let has_tables: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table')", [], |r| r.get(0))?;
    if has_tables {
        snapshot(conn, path)?;
    }
    for m in pending {
        let tx = conn.transaction()?;
        for sql in m.sql {
            tx.execute_batch(sql)?;
        }
        tx.pragma_update(None, "user_version", m.version)?;
        tx.commit()?;
        tracing::info!(version = m.version, name = m.name, "relay database migrated");
    }
    Ok(())
}

/// A consistent copy of the database before it changes, next to it:
/// `<db>.before-v<latest>.from-v<current>.<ms>`. Never overwritten, so if a run stops halfway
/// (1 done, 2 failed) the next start adds a copy instead of replacing the original one.
fn snapshot(conn: &Connection, path: &Path) -> rusqlite::Result<String> {
    let base = format!("{}.before-v{}.from-v{}.{}", path.display(), latest(), version(conn)?, crate::now_ms());
    let mut to = base.clone();
    let mut n = 1;
    while Path::new(&to).exists() {
        to = format!("{base}-{n}");
        n += 1;
    }
    conn.execute("VACUUM INTO ?1", [&to])?;
    tracing::info!(copy = %to, "copied the relay database before migrating");
    Ok(to)
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;
    use crate::Db;

    #[test]
    fn numbers_only_go_up() {
        let mut last = 0;
        for m in MIGRATIONS {
            assert_eq!(m.version, last + 1, "{}", m.name);
            last = m.version;
        }
    }

    #[test]
    fn a_new_database_gets_every_table_and_no_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.db");
        let db = Db::open(&path).unwrap();
        assert_eq!(version(&db.c()).unwrap(), latest());
        for t in [
            "installs",
            "queue",
            "tip_pages",
            "pro_subs",
            "pro_installs",
            "quota_usage",
            "streams",
            "stream_samples",
            "cloud_backups",
            "cloud_backup_parts",
            "tts_usage",
            "remote_devices",
            "remote_codes",
            "remote_audit",
            "discord_guilds",
            "discord_rules",
            "discord_links",
            "discord_grants",
            "discord_posts",
            "bot_channels",
            "bot_tokens",
            "bot_config",
            "bot_counter_changes",
            "mail_addresses",
            "mail_sent",
            "mail_budget",
        ] {
            let n: i64 =
                db.c().query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = ?1", [t], |r| r.get(0)).unwrap();
            assert_eq!(n, 1, "{t}");
        }
        assert!(copies(dir.path()).is_empty(), "nothing to copy in a new database");
    }

    #[test]
    fn a_server_from_before_migrations_keeps_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.db");
        {
            // What the live relay has today: the tables, user_version 0.
            let c = Connection::open(&path).unwrap();
            c.execute_batch(crate::db::SCHEMA).unwrap();
            c.execute_batch(crate::billing::store::SCHEMA).unwrap();
            c.execute("INSERT INTO installs(id, secret_hash, created_at, last_seen) VALUES ('i1', 'h', 1, 2)", [])
                .unwrap();
            c.execute(
                "INSERT INTO pro_subs(id, plan, email_hash, status, created_at, synced_at) VALUES ('sub_1', 'monthly', 'e', 'active', 1, 1)",
                [],
            )
            .unwrap();
            assert_eq!(version(&c).unwrap(), 0);
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(version(&db.c()).unwrap(), latest());
        assert_eq!(db.install_hash("i1").unwrap().as_deref(), Some("h"));
        assert_eq!(db.pro_sub("sub_1").unwrap().unwrap().status, "active");
        db.try_take("sub_1", "n", crate::quota::Period::Day, 1, 5).unwrap();
        // The copy taken first has the old data and no new tables.
        let made = copies(dir.path());
        assert_eq!(made.len(), 1, "{made:?}");
        assert!(made[0].contains(&format!(".before-v{}.from-v0.", latest())), "{made:?}");
        let copy = Connection::open(dir.path().join(&made[0])).unwrap();
        let n: i64 = copy.query_row("SELECT COUNT(*) FROM installs", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert_eq!(version(&copy).unwrap(), 0);
        drop(db);
        // Opening again changes nothing.
        let db = Db::open(&path).unwrap();
        assert_eq!(version(&db.c()).unwrap(), latest());
        assert_eq!(db.quota_used("sub_1", "n", crate::quota::Period::Day).unwrap(), 1);
        assert_eq!(copies(dir.path()).len(), 1, "nothing pending, no new copy");
    }

    #[test]
    fn a_run_that_stopped_halfway_keeps_the_first_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.db");
        let mut c = Connection::open(&path).unwrap();
        c.execute_batch(crate::db::SCHEMA).unwrap();
        c.execute("INSERT INTO installs(id, secret_hash, created_at, last_seen) VALUES ('i1', 'h', 1, 2)", []).unwrap();
        let first = snapshot(&c, &path).unwrap();
        // Migration 1 went through, then the relay stopped before the rest; data changed meanwhile.
        c.pragma_update(None, "user_version", 1).unwrap();
        c.execute("DELETE FROM installs", []).unwrap();
        run(&mut c, &path).unwrap();
        let made = copies(dir.path());
        assert_eq!(made.len(), 2, "a second copy, the first kept: {made:?}");
        assert!(made.iter().any(|m| m.contains(".from-v1.")), "{made:?}");
        let original = Connection::open(&first).unwrap();
        let n: i64 = original.query_row("SELECT COUNT(*) FROM installs", [], |r| r.get(0)).unwrap();
        assert_eq!((n, version(&original).unwrap()), (1, 0), "the original copy is untouched");
        // Two copies in the same millisecond still get their own names.
        let a = snapshot(&c, &path).unwrap();
        let b = snapshot(&c, &path).unwrap();
        assert_ne!(a, b);
    }

    /// The database copies next to `relay.db`, by file name.
    fn copies(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with("relay.db.before-v"))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_newer_database_still_opens_for_a_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.db");
        drop(Db::open(&path).unwrap());
        Connection::open(&path).unwrap().pragma_update(None, "user_version", latest() + 5).unwrap();
        let db = Db::open(&path).unwrap();
        assert_eq!(version(&db.c()).unwrap(), latest() + 5, "left as it is");
        db.add_install("i2", "h").unwrap();
    }

    #[test]
    fn a_failed_migration_leaves_the_version_where_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Connection::open(dir.path().join("x.db")).unwrap();
        let tx = c.transaction().unwrap();
        tx.execute_batch("CREATE TABLE a (x INTEGER);").unwrap();
        tx.pragma_update(None, "user_version", 7).unwrap();
        drop(tx); // rolled back
        assert_eq!(version(&c).unwrap(), 0);
        c.execute("CREATE TABLE b (x INTEGER)", params![]).unwrap();
    }
}
