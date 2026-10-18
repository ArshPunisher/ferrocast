//! Encryption for the few secrets Pro keeps on the relay (platform refresh tokens, a mail
//! address): AES-256-GCM with the server's `RELAY_VAULT_KEY` (32 bytes, base64), made on the
//! server by `deploy/vault-key.sh` and never printed or logged.
//!
//! Each sealed value is bound to its install and purpose (the AAD is `install|purpose`), so
//! a row copied to another install or another column doesn't open. Rows keep
//! `(key_v, nonce, ct)`; `key_v` names the key, so a new key can be added later without
//! losing the old rows. Without a key, features that need the vault answer 503
//! `vault_unavailable`; the relay itself runs as before.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use axum::http::StatusCode;
use base64::Engine as _;

use crate::ApiError;

/// The key version every new seal uses.
pub const KEY_V: i64 = 1;
const NONCE_LEN: usize = 12;

/// One sealed value, as stored: `(key_v, nonce, ct)`. `ct` ends with the 16-byte tag.
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    pub key_v: i64,
    pub nonce: Vec<u8>,
    pub ct: Vec<u8>,
}

impl std::fmt::Debug for Sealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealed").field("key_v", &self.key_v).field("ct_len", &self.ct.len()).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VaultError {
    /// `RELAY_VAULT_KEY` is set but isn't 32 bytes of base64.
    #[error("RELAY_VAULT_KEY must be 32 bytes, base64-encoded")]
    BadKey,
    /// A sealed row made with a key this relay doesn't have.
    #[error("sealed with key version {0}, which this relay doesn't have")]
    UnknownKey(i64),
    /// Wrong install or purpose, a damaged row, or a different key.
    #[error("the sealed value doesn't open")]
    Damaged,
    /// The install or purpose has a `|`, which would make the AAD ambiguous.
    #[error("install and purpose can't contain '|'")]
    BadLabel,
}

/// The relay's vault. `Debug` never shows the key.
pub struct Vault {
    cipher: Aes256Gcm,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").field("key_v", &KEY_V).field("key", &"<redacted>").finish()
    }
}

fn aad(install: &str, purpose: &str) -> Result<Vec<u8>, VaultError> {
    if install.contains('|') || purpose.contains('|') {
        return Err(VaultError::BadLabel);
    }
    Ok(format!("{install}|{purpose}").into_bytes())
}

impl Vault {
    /// A vault from the base64 text of a 32-byte key.
    pub fn from_base64(key: &str) -> Result<Vault, VaultError> {
        let raw = base64::engine::general_purpose::STANDARD.decode(key.trim()).map_err(|_| VaultError::BadKey)?;
        let cipher = Aes256Gcm::new_from_slice(&raw).map_err(|_| VaultError::BadKey)?;
        Ok(Vault { cipher })
    }

    /// `RELAY_VAULT_KEY`: `Ok(None)` when unset, an error when set but unusable.
    pub fn from_env() -> Result<Option<Vault>, VaultError> {
        match std::env::var("RELAY_VAULT_KEY").ok().filter(|v| !v.trim().is_empty()) {
            Some(k) => Vault::from_base64(&k).map(Some),
            None => Ok(None),
        }
    }

    /// Encrypts `plaintext` for one install and purpose, with a fresh random nonce.
    pub fn seal(&self, install: &str, purpose: &str, plaintext: &[u8]) -> Result<Sealed, VaultError> {
        let aad = aad(install, purpose)?;
        let mut nonce = [0u8; NONCE_LEN];
        // No OS randomness means no safe nonce; refuse loudly (as `crate::random` does).
        #[allow(clippy::expect_used)]
        getrandom::fill(&mut nonce).expect("the OS random number generator is unavailable");
        let ct = self
            .cipher
            .encrypt(&Nonce::from(nonce), Payload { msg: plaintext, aad: &aad })
            .map_err(|_| VaultError::Damaged)?;
        Ok(Sealed { key_v: KEY_V, nonce: nonce.to_vec(), ct })
    }

    /// Decrypts a value sealed for this install and purpose.
    pub fn open(&self, install: &str, purpose: &str, sealed: &Sealed) -> Result<Vec<u8>, VaultError> {
        if sealed.key_v != KEY_V {
            return Err(VaultError::UnknownKey(sealed.key_v));
        }
        let aad = aad(install, purpose)?;
        let nonce: [u8; NONCE_LEN] = sealed.nonce.as_slice().try_into().map_err(|_| VaultError::Damaged)?;
        self.cipher
            .decrypt(&Nonce::from(nonce), Payload { msg: &sealed.ct, aad: &aad })
            .map_err(|_| VaultError::Damaged)
    }
}

/// What a feature that needs the vault answers when the server has no key.
pub fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "vault_unavailable",
        "This Pro feature isn't switched on on Ferrocast's servers yet.",
    )
    .hint("Nothing to do on your side; try again later.")
}

/// A sealed row that doesn't open is our fault, never the streamer's: they reconnect.
pub fn damaged(e: &VaultError) -> ApiError {
    tracing::error!(error = %e, "a sealed value didn't open");
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "vault_unavailable", "Ferrocast couldn't read a saved sign-in.")
        .hint("Connect that account again in Ferrocast.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> String {
        base64::engine::general_purpose::STANDARD.encode([7u8; 32])
    }

    #[test]
    fn a_sealed_value_opens_only_for_its_install_and_purpose() {
        let v = Vault::from_base64(&key()).unwrap();
        let s = v.seal("inst1", "twitch_refresh", b"secret-token").unwrap();
        assert_eq!(s.key_v, KEY_V);
        assert_eq!(s.nonce.len(), NONCE_LEN);
        assert!(!s.ct.windows(12).any(|w| w == b"secret-token"), "the plaintext isn't in the row");
        assert_eq!(v.open("inst1", "twitch_refresh", &s).unwrap(), b"secret-token");
        assert_eq!(v.open("inst2", "twitch_refresh", &s), Err(VaultError::Damaged), "another install");
        assert_eq!(v.open("inst1", "kick_refresh", &s), Err(VaultError::Damaged), "another purpose");
        // "a|b" + "c" and "a" + "b|c" must never share an AAD.
        assert_eq!(v.seal("a|b", "c", b"x"), Err(VaultError::BadLabel));
        assert_eq!(v.open("a", "b|c", &s), Err(VaultError::BadLabel));
    }

    #[test]
    fn a_changed_row_or_another_key_doesnt_open() {
        let v = Vault::from_base64(&key()).unwrap();
        let s = v.seal("i", "p", b"hello").unwrap();
        let mut flipped = s.clone();
        flipped.ct[0] ^= 1;
        assert_eq!(v.open("i", "p", &flipped), Err(VaultError::Damaged));
        let short = Sealed { nonce: vec![0; 3], ..s.clone() };
        assert_eq!(v.open("i", "p", &short), Err(VaultError::Damaged));
        let other = Vault::from_base64(&base64::engine::general_purpose::STANDARD.encode([8u8; 32])).unwrap();
        assert_eq!(other.open("i", "p", &s), Err(VaultError::Damaged));
        assert_eq!(v.open("i", "p", &Sealed { key_v: 2, ..s }), Err(VaultError::UnknownKey(2)));
        // Two seals of the same text differ (fresh nonces).
        assert_ne!(v.seal("i", "p", b"hello").unwrap(), v.seal("i", "p", b"hello").unwrap());
    }

    #[test]
    fn only_a_32_byte_base64_key_is_accepted() {
        assert!(Vault::from_base64(&key()).is_ok());
        assert!(Vault::from_base64(&format!(" {}\n", key())).is_ok(), "whitespace around it is fine");
        assert_eq!(Vault::from_base64("not base64!").unwrap_err(), VaultError::BadKey);
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        assert_eq!(Vault::from_base64(&short).unwrap_err(), VaultError::BadKey);
    }

    #[test]
    fn debug_never_shows_the_key_or_the_ciphertext() {
        let v = Vault::from_base64(&key()).unwrap();
        let d = format!("{v:?}");
        assert!(d.contains("<redacted>") && !d.contains(&key()), "{d}");
        let s = v.seal("i", "p", b"hello").unwrap();
        let d = format!("{s:?}");
        assert!(d.contains("ct_len") && !d.contains("nonce"), "{d}");
    }
}
