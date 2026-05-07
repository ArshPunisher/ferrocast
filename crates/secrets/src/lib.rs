//! Secrets (platform tokens, the OBS password) live in the OS keychain only (R-45):
//! Keychain on macOS, Credential Manager on Windows, Secret Service on Linux. Never in the
//! database, settings, exports or logs.
//!
//! Callers use the [`Secrets`] trait so tests (and the headless server, if the OS has no
//! keychain) can swap in [`MemorySecrets`].

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

/// The keychain service name every Ferrocast secret is filed under.
pub const SERVICE: &str = "Ferrocast";

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("this computer's keychain isn't available: {0}")]
    Unavailable(String),
    #[error("the keychain refused: {0}")]
    Keychain(String),
}

pub trait Secrets: Send + Sync + fmt::Debug {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError>;
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;
    /// Removes a secret; removing one that isn't there is fine.
    fn delete(&self, key: &str) -> Result<(), SecretError>;
}

/// The OS keychain.
#[derive(Debug, Default)]
pub struct Keychain;

impl Keychain {
    fn entry(key: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(SERVICE, key).map_err(|e| match e {
            keyring::Error::NoDefaultStore => SecretError::Unavailable(e.to_string()),
            other => SecretError::Keychain(other.to_string()),
        })
    }

    /// Whether the keychain works here (headless Linux servers often have none).
    pub fn available() -> bool {
        keyring::Entry::store_status().is_ok()
    }
}

impl Secrets for Keychain {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        match Self::entry(key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError::Keychain(e.to_string())),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        Self::entry(key)?.set_password(value).map_err(|e| SecretError::Keychain(e.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError::Keychain(e.to_string())),
        }
    }
}

/// Secrets kept in memory for one run (tests, or a machine without a keychain, where the
/// streamer signs in again after a restart rather than us writing tokens to disk).
#[derive(Default)]
pub struct MemorySecrets {
    map: Mutex<HashMap<String, String>>,
}

impl fmt::Debug for MemorySecrets {
    // Never print the values.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.map.lock().map(|m| m.len()).unwrap_or(0);
        write!(f, "MemorySecrets({n} entries)")
    }
}

impl Secrets for MemorySecrets {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        Ok(self.map.lock().unwrap_or_else(|p| p.into_inner()).get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).insert(key.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).remove(key);
        Ok(())
    }
}

/// The keychain when it works, otherwise memory (with a warning in the log).
pub fn best_available() -> Box<dyn Secrets> {
    if Keychain::available() {
        Box::new(Keychain)
    } else {
        tracing_warn("no OS keychain; secrets will be kept in memory for this run only");
        Box::new(MemorySecrets::default())
    }
}

fn tracing_warn(msg: &str) {
    eprintln!("ferrocast: {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip_and_never_prints_values() {
        let s = MemorySecrets::default();
        assert_eq!(s.get("twitch").unwrap(), None);
        s.set("twitch", "super-secret-token").unwrap();
        assert_eq!(s.get("twitch").unwrap().as_deref(), Some("super-secret-token"));
        assert!(!format!("{s:?}").contains("super-secret"));
        s.delete("twitch").unwrap();
        s.delete("twitch").unwrap();
        assert_eq!(s.get("twitch").unwrap(), None);
    }
}
