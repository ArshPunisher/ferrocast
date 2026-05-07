//! Bot data: commands, timers, counters, quotes, and the shared ban list.

use ferrocast_core::command::{Command, Timer};
use ferrocast_core::moderation::BanList;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};
use crate::{Store, now_ms};

pub fn commands(store: &Store) -> Result<Vec<Command>> {
    store.read(|c| {
        let mut st = c.prepare("SELECT command FROM commands ORDER BY rowid")?;
        Ok(st.query_map([], |r| super::from_json(0, r.get(0)?))?.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

/// Validates against the other commands (names and aliases must be unique) and saves.
pub fn save_command(store: &Store, cmd: &Command) -> Result<()> {
    let others = commands(store)?;
    cmd.validate(&others).map_err(|e| StoreError::Conflict(e.to_string()))?;
    let json = serde_json::to_string(cmd)?;
    store.write(|tx| {
        tx.execute(
            "INSERT INTO commands(id, command) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET command = excluded.command",
            params![cmd.id, json],
        )?;
        Ok(())
    })
}

pub fn delete_command(store: &Store, id: &str) -> Result<()> {
    let n = store.write(|tx| Ok(tx.execute("DELETE FROM commands WHERE id = ?1", [id])?))?;
    if n == 0 { Err(StoreError::NotFound("command")) } else { Ok(()) }
}

pub fn timers(store: &Store) -> Result<Vec<Timer>> {
    store.read(|c| {
        let mut st = c.prepare("SELECT timer FROM timers ORDER BY rowid")?;
        Ok(st.query_map([], |r| super::from_json(0, r.get(0)?))?.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn save_timer(store: &Store, t: &Timer) -> Result<()> {
    t.validate().map_err(|e| StoreError::Conflict(e.to_string()))?;
    let json = serde_json::to_string(t)?;
    store.write(|tx| {
        tx.execute(
            "INSERT INTO timers(id, timer) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET timer = excluded.timer",
            params![t.id, json],
        )?;
        Ok(())
    })
}

pub fn delete_timer(store: &Store, id: &str) -> Result<()> {
    let n = store.write(|tx| Ok(tx.execute("DELETE FROM timers WHERE id = ?1", [id])?))?;
    if n == 0 { Err(StoreError::NotFound("timer")) } else { Ok(()) }
}

/// Adds `delta` to a counter (creating it at 0) and returns the new value.
pub fn bump_counter(store: &Store, name: &str, delta: i64) -> Result<i64> {
    let name = name.trim().to_lowercase();
    if name.is_empty() || name.len() > 40 {
        return Err(StoreError::Conflict("counter names are 1 to 40 characters".into()));
    }
    store.write(|tx| {
        tx.execute(
            "INSERT INTO counters(name, value) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET value = counters.value + excluded.value",
            params![name, delta],
        )?;
        Ok(tx.query_row("SELECT value FROM counters WHERE name = ?1", [&name], |r| r.get(0))?)
    })
}

pub fn counter(store: &Store, name: &str) -> Result<i64> {
    let name = name.trim().to_lowercase();
    Ok(store
        .read(|c| Ok(c.query_row("SELECT value FROM counters WHERE name = ?1", [name], |r| r.get(0)).optional()?))?
        .unwrap_or(0))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub id: i64,
    pub text: String,
    pub added_by: String,
    pub at: i64,
}

pub fn add_quote(store: &Store, text: &str, added_by: &str) -> Result<Quote> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > 400 {
        return Err(StoreError::Conflict("quotes are 1 to 400 characters".into()));
    }
    let at = now_ms();
    let id = store.write(|tx| {
        tx.execute("INSERT INTO quotes(text, added_by, at) VALUES (?1, ?2, ?3)", params![text, added_by, at])?;
        Ok(tx.last_insert_rowid())
    })?;
    Ok(Quote { id, text: text.into(), added_by: added_by.into(), at })
}

/// A quote by number, or a random one.
pub fn quote(store: &Store, id: Option<i64>) -> Result<Option<Quote>> {
    store.read(|c| {
        let sql = match id {
            Some(_) => "SELECT id, text, added_by, at FROM quotes WHERE id = ?1",
            None => "SELECT id, text, added_by, at FROM quotes WHERE ?1 IS NULL ORDER BY RANDOM() LIMIT 1",
        };
        Ok(c.query_row(sql, [id], |r| Ok(Quote { id: r.get(0)?, text: r.get(1)?, added_by: r.get(2)?, at: r.get(3)? }))
            .optional()?)
    })
}

pub fn delete_quote(store: &Store, id: i64) -> Result<()> {
    let n = store.write(|tx| Ok(tx.execute("DELETE FROM quotes WHERE id = ?1", [id])?))?;
    if n == 0 { Err(StoreError::NotFound("quote")) } else { Ok(()) }
}

pub fn ban_list(store: &Store) -> Result<BanList> {
    super::settings::get_or_default(store, "ban_list")
}

pub fn save_ban_list(store: &Store, list: &BanList) -> Result<()> {
    super::settings::set(store, "ban_list", list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_core::command::Access;

    fn cmd(id: &str, name: &str) -> Command {
        Command {
            id: id.into(),
            name: name.into(),
            aliases: vec![],
            response: "hi".into(),
            access: Access::Everyone,
            cooldown_s: 5,
            user_cooldown_s: 0,
            platforms: vec![],
            enabled: true,
        }
    }

    #[test]
    fn commands_must_have_unique_names_across_updates() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        save_command(&s, &cmd("a", "!discord")).unwrap();
        save_command(&s, &cmd("a", "!discord")).unwrap(); // updating itself is fine
        assert!(matches!(save_command(&s, &cmd("b", "!DISCORD")), Err(StoreError::Conflict(_))));
        assert_eq!(commands(&s).unwrap().len(), 1);
        delete_command(&s, "a").unwrap();
        assert!(delete_command(&s, "a").is_err());
    }

    #[test]
    fn counters_quotes_and_bans() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(bump_counter(&s, "Deaths", 1).unwrap(), 1);
        assert_eq!(bump_counter(&s, "deaths ", 2).unwrap(), 3);
        assert_eq!(counter(&s, "DEATHS").unwrap(), 3);
        assert_eq!(counter(&s, "none").unwrap(), 0);
        assert!(bump_counter(&s, "  ", 1).is_err());
        let q = add_quote(&s, "  gg no re  ", "mod").unwrap();
        assert_eq!(q.text, "gg no re");
        assert_eq!(quote(&s, Some(q.id)).unwrap(), Some(q.clone()));
        assert_eq!(quote(&s, None).unwrap(), Some(q.clone()));
        assert!(add_quote(&s, "", "mod").is_err());
        delete_quote(&s, q.id).unwrap();
        assert_eq!(quote(&s, None).unwrap(), None);
        let mut bans = ban_list(&s).unwrap();
        bans.match_names = true;
        save_ban_list(&s, &bans).unwrap();
        assert!(ban_list(&s).unwrap().match_names);
    }
}
