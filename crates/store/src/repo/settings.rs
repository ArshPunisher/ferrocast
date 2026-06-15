//! Typed key-value settings and connected accounts (no secrets: those live in the keychain).

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::Result;
use crate::{Store, now_ms};

/// Reads a setting; `None` when it was never saved.
pub fn get<T: DeserializeOwned>(store: &Store, key: &str) -> Result<Option<T>> {
    let raw: Option<String> = store
        .read(|c| Ok(c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?))?;
    match raw {
        Some(s) => Ok(Some(serde_json::from_str(&s)?)),
        None => Ok(None),
    }
}

/// Reads a setting or its default. A stored value that no longer parses (renamed field)
/// is an error, not a silent reset to defaults (R-34).
pub fn get_or_default<T: DeserializeOwned + Default>(store: &Store, key: &str) -> Result<T> {
    Ok(get(store, key)?.unwrap_or_default())
}

pub fn set<T: Serialize>(store: &Store, key: &str, value: &T) -> Result<()> {
    let json = serde_json::to_string(value)?;
    store.write(|tx| {
        tx.execute(
            "INSERT INTO settings(key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, json, now_ms()],
        )?;
        Ok(())
    })
}

/// Removes a setting; the next `get` returns `None`.
pub fn delete(store: &Store, key: &str) -> Result<()> {
    store.write(|tx| {
        tx.execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub platform: String,
    pub user_id: String,
    pub login: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub scopes: Vec<String>,
    pub connected_at: i64,
}

pub fn accounts(store: &Store) -> Result<Vec<Account>> {
    store.read(|c| {
        let mut st = c.prepare(
            "SELECT platform, user_id, login, display_name, avatar_url, scopes, connected_at FROM accounts ORDER BY platform",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Account {
                platform: r.get(0)?,
                user_id: r.get(1)?,
                login: r.get(2)?,
                display_name: r.get(3)?,
                avatar_url: r.get(4)?,
                scopes: super::from_json(5, r.get(5)?)?,
                connected_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn save_account(store: &Store, a: &Account) -> Result<()> {
    let scopes = serde_json::to_string(&a.scopes)?;
    store.write(|tx| {
        tx.execute(
            "INSERT INTO accounts(platform, user_id, login, display_name, avatar_url, scopes, connected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(platform) DO UPDATE SET user_id = excluded.user_id, login = excluded.login,
               display_name = excluded.display_name, avatar_url = excluded.avatar_url, scopes = excluded.scopes,
               connected_at = excluded.connected_at",
            params![a.platform, a.user_id, a.login, a.display_name, a.avatar_url, scopes, a.connected_at],
        )?;
        Ok(())
    })
}

pub fn remove_account(store: &Store, platform: &str) -> Result<bool> {
    store.write(|tx| Ok(tx.execute("DELETE FROM accounts WHERE platform = ?1", [platform])? > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Prefs {
        volume: u8,
    }

    #[test]
    fn typed_settings_round_trip_and_default() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(get_or_default::<Prefs>(&s, "prefs").unwrap(), Prefs::default());
        set(&s, "prefs", &Prefs { volume: 40 }).unwrap();
        set(&s, "prefs", &Prefs { volume: 60 }).unwrap();
        assert_eq!(get::<Prefs>(&s, "prefs").unwrap(), Some(Prefs { volume: 60 }));
    }

    #[test]
    fn a_setting_that_no_longer_parses_is_an_error_not_a_reset() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        set(&s, "prefs", &"not prefs").unwrap();
        assert!(get_or_default::<Prefs>(&s, "prefs").is_err());
    }

    #[test]
    fn accounts_upsert_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let mut a = Account {
            platform: "twitch".into(),
            user_id: "1".into(),
            login: "me".into(),
            display_name: "Me".into(),
            avatar_url: None,
            scopes: vec!["chat:read".into()],
            connected_at: 1,
        };
        save_account(&s, &a).unwrap();
        a.display_name = "Me2".into();
        save_account(&s, &a).unwrap();
        assert_eq!(accounts(&s).unwrap(), vec![a]);
        assert!(remove_account(&s, "twitch").unwrap());
        assert!(!remove_account(&s, "twitch").unwrap());
    }
}
