//! Local API keys (Phase 3 #19): CSPRNG generation, SHA-256 hashing, verification, and audit.

use ferrocast_core::api_keys::{
    ApiKeyAuditEntry, ApiKeyCreate, ApiKeyCreated, ApiKeyScope, ApiKeySummary, ApiKeysView, KEY_PREFIX, display_prefix,
    is_well_formed_token, normalize_scopes, validate_expiry, validate_name,
};
use ferrocast_store::repo;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{Engine, EngineError, now};

const DEFAULT_AUDIT_LIMIT: usize = 100;

/// Ids of keys just revoked: a live socket held by one listens and closes at once.
static REVOKED: std::sync::LazyLock<tokio::sync::broadcast::Sender<String>> =
    std::sync::LazyLock::new(|| tokio::sync::broadcast::channel(64).0);

/// Listens for revoked key ids (subscribe before checking the key, so none falls in the gap).
#[must_use]
pub fn subscribe_key_revocations() -> tokio::sync::broadcast::Receiver<String> {
    REVOKED.subscribe()
}

/// Outcome of validating a presented `fc_key_*` token.
#[derive(Debug, Clone)]
pub enum ApiKeyAuth {
    /// Token matched an active, non-revoked, non-expired key.
    Valid(ApiKeySummary),
    /// Token was malformed, unknown, revoked, or expired.
    Invalid,
}

impl Engine {
    /// Lists all local API keys (without secrets or hashes) and recent redacted audit rows.
    pub async fn api_keys_view(&self) -> Result<ApiKeysView, EngineError> {
        self.db(|s| {
            let keys = repo::api_keys::list(s)?;
            let audit = repo::api_keys::list_audit(s, DEFAULT_AUDIT_LIMIT)?;
            Ok(ApiKeysView { keys, audit })
        })
        .await
    }

    /// Lists only the stored API keys (metadata only).
    pub async fn list_api_keys(&self) -> Result<Vec<ApiKeySummary>, EngineError> {
        self.db(repo::api_keys::list).await
    }

    /// Lists recent redacted API key audit events.
    pub async fn list_api_key_audit(&self, limit: usize) -> Result<Vec<ApiKeyAuditEntry>, EngineError> {
        self.db(move |s| repo::api_keys::list_audit(s, limit)).await
    }

    /// Generates a new scoped local API key, hashes it with SHA-256, persists metadata + hash,
    /// and returns the one-time plain key. The plain key is never stored or logged.
    pub async fn create_api_key(&self, req: ApiKeyCreate) -> Result<ApiKeyCreated, EngineError> {
        let name = validate_name(&req.name).map_err(|m| EngineError::Invalid(m.to_string()))?;
        let scopes: Vec<ApiKeyScope> =
            normalize_scopes(&req.scopes).map_err(|m| EngineError::Invalid(m.to_string()))?;
        let expires_at = validate_expiry(req.expires_at, now().0).map_err(|m| EngineError::Invalid(m.to_string()))?;

        let token = generate_token()?;
        let prefix = display_prefix(&token);
        let key_hash = sha256_hex(&token);
        let id = ferrocast_store::new_id();

        let summary =
            self.db(move |s| repo::api_keys::insert(s, &id, &name, &prefix, &key_hash, &scopes, expires_at)).await?;

        Ok(ApiKeyCreated { key: summary, token })
    }

    /// Revokes a local API key by id so future requests are immediately rejected.
    pub async fn revoke_api_key(&self, id: &str) -> Result<(), EngineError> {
        let id = id.trim().to_string();
        if id.is_empty() {
            return Err(EngineError::NotFound("API key"));
        }
        let key_id = id.clone();
        self.db(move |s| repo::api_keys::revoke(s, &id)).await?;
        // Its open sockets close now (nobody listening is fine).
        let _ = REVOKED.send(key_id);
        Ok(())
    }

    /// Authenticates a presented `fc_key_*` token by hashing it with SHA-256 and checking active
    /// status (neither revoked nor expired).
    pub async fn authenticate_api_key(&self, token: &str) -> Result<ApiKeyAuth, EngineError> {
        if !is_well_formed_token(token) {
            return Ok(ApiKeyAuth::Invalid);
        }
        let digest = sha256_hex(token);
        let digest_cmp = digest.clone();
        let rec = self.db(move |s| repo::api_keys::find_by_hash(s, &digest)).await?;
        let Some(rec) = rec else {
            return Ok(ApiKeyAuth::Invalid);
        };
        let hash_matches = bool::from(rec.key_hash.as_bytes().ct_eq(digest_cmp.as_bytes()));
        if !hash_matches || !rec.summary.is_active_at(now().0) {
            return Ok(ApiKeyAuth::Invalid);
        }
        Ok(ApiKeyAuth::Valid(rec.summary))
    }

    /// Logs API key usage to tracing and SQLite audit history without the raw token or hash.
    pub async fn record_api_key_usage(&self, key_id: &str, key_name: &str, method: &str, path: &str, status: u16) {
        tracing::info!(
            key_id = %key_id,
            key_name = %key_name,
            method = %method,
            path = %path,
            status = status,
            "api_key_use"
        );
        let key_id = key_id.to_string();
        let key_name = key_name.to_string();
        let method = method.to_string();
        let path = path.to_string();
        let _ = self.db(move |s| repo::api_keys::record_usage(s, &key_id, &key_name, &method, &path, status)).await;
    }
}

/// Computes the lowercase hex SHA-256 digest of a raw key token.
#[must_use]
pub fn sha256_hex(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let out = hasher.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn generate_token() -> Result<String, EngineError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| EngineError::Internal(format!("OS randomness failed: {e}")))?;
    let mut s = String::with_capacity(KEY_PREFIX.len() + 64);
    s.push_str(KEY_PREFIX);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use ferrocast_core::api_keys::is_well_formed_token;

    use super::*;

    #[tokio::test]
    async fn hash_is_stored_plain_key_never_stored_and_revoked_or_expired_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::start_with_memory_secrets(dir.path()).await.unwrap();

        let created = engine
            .create_api_key(ApiKeyCreate {
                name: "Follow Listener".into(),
                scopes: vec![ApiKeyScope::ReadOnly, ApiKeyScope::Alerts],
                expires_at: None,
            })
            .await
            .unwrap();
        assert!(is_well_formed_token(&created.token));
        assert_eq!(created.key.scopes, vec![ApiKeyScope::ReadOnly, ApiKeyScope::Alerts]);

        // Plain key must never appear anywhere in the SQLite database file or summary list.
        let stored_rec = repo::api_keys::find_by_hash(&engine.store(), &sha256_hex(&created.token)).unwrap().unwrap();
        assert_ne!(stored_rec.key_hash, created.token);
        assert_eq!(stored_rec.key_hash, sha256_hex(&created.token));
        let listed = engine.list_api_keys().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].prefix, ferrocast_core::api_keys::display_prefix(&created.token));
        assert!(!serde_json::to_string(&listed).unwrap().contains(&created.token));

        // Active key authenticates cleanly.
        assert!(matches!(engine.authenticate_api_key(&created.token).await.unwrap(), ApiKeyAuth::Valid(_)));

        // Wrong token fails even with valid prefix.
        let wrong = format!("{KEY_PREFIX}{}", "f".repeat(64));
        assert!(matches!(engine.authenticate_api_key(&wrong).await.unwrap(), ApiKeyAuth::Invalid));

        // Usage logs without plain token or hash.
        engine.record_api_key_usage(&created.key.id, &created.key.name, "GET", "/status", 200).await;
        let audit = engine.list_api_key_audit(10).await.unwrap();
        assert_eq!(audit.len(), 1);
        let audit_json = serde_json::to_string(&audit).unwrap();
        assert!(!audit_json.contains(&created.token));
        assert!(!audit_json.contains(&stored_rec.key_hash));

        // Revoked key fails authentication immediately.
        engine.revoke_api_key(&created.key.id).await.unwrap();
        assert!(matches!(engine.authenticate_api_key(&created.token).await.unwrap(), ApiKeyAuth::Invalid));

        // Expired key (inserted directly in past) also fails authentication.
        let exp_token = generate_token().unwrap();
        let exp_hash = sha256_hex(&exp_token);
        repo::api_keys::insert(
            &engine.store(),
            "expired_key",
            "Old Key",
            &display_prefix(&exp_token),
            &exp_hash,
            &[ApiKeyScope::ReadOnly],
            Some(1),
        )
        .unwrap();
        assert!(matches!(engine.authenticate_api_key(&exp_token).await.unwrap(), ApiKeyAuth::Invalid));

        engine.shutdown();
    }
}
