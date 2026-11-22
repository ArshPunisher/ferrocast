//! Pure domain types and validation rules for scoped local API keys (Phase 3 #19).

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Prefix shared by every generated local API key so callers, proxies, and auth middleware
/// can distinguish API keys from short-lived dashboard pairing tokens immediately.
pub const KEY_PREFIX: &str = "fc_key_";
/// Number of lowercase hex characters following [`KEY_PREFIX`] (32 random bytes = 256 bits).
pub const KEY_HEX_LEN: usize = 64;
/// Total length of a validly formatted plain API key.
pub const KEY_TOTAL_LEN: usize = KEY_PREFIX.len() + KEY_HEX_LEN;
/// Maximum allowed length for a user-supplied key name.
pub const MAX_KEY_NAME_LEN: usize = 60;
/// Maximum number of active (non-revoked) local API keys retained simultaneously.
pub const MAX_ACTIVE_KEYS: usize = 32;
/// Maximum allowed expiry horizon from creation (5 years in milliseconds).
pub const MAX_EXPIRY_HORIZON_MS: i64 = 5 * 365 * 24 * 60 * 60 * 1000;

/// Permission scopes assignable to a local API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyScope {
    ReadOnly,
    Chat,
    Alerts,
    StudioControl,
    GoLive,
    Settings,
}

impl ApiKeyScope {
    /// All valid scopes in canonical display order.
    pub const ALL: [Self; 6] =
        [Self::ReadOnly, Self::Chat, Self::Alerts, Self::StudioControl, Self::GoLive, Self::Settings];

    /// Canonical wire string matching JSON serialization.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Chat => "chat",
            Self::Alerts => "alerts",
            Self::StudioControl => "studio_control",
            Self::GoLive => "go_live",
            Self::Settings => "settings",
        }
    }

    /// Plain-language label suitable for UI display and documentation.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read only",
            Self::Chat => "Chat",
            Self::Alerts => "Alerts",
            Self::StudioControl => "Studio control",
            Self::GoLive => "Go live",
            Self::Settings => "Settings",
        }
    }
}

impl fmt::Display for ApiKeyScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ApiKeyScope {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "read_only" => Ok(Self::ReadOnly),
            "chat" => Ok(Self::Chat),
            "alerts" => Ok(Self::Alerts),
            "studio_control" => Ok(Self::StudioControl),
            "go_live" => Ok(Self::GoLive),
            "settings" => Ok(Self::Settings),
            _ => Err("unknown API key scope"),
        }
    }
}

/// Request payload to create a local API key.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiKeyCreate {
    pub name: String,
    pub scopes: Vec<ApiKeyScope>,
    /// Optional expiry timestamp in Unix milliseconds.
    pub expires_at: Option<i64>,
}

/// Public metadata for one stored local API key (never includes the secret or its hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeySummary {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub scopes: Vec<ApiKeyScope>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

impl ApiKeySummary {
    /// Returns `true` when the key has not been revoked and has not expired at `now_ms`.
    #[must_use]
    pub fn is_active_at(&self, now_ms: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|exp| exp > now_ms)
    }
}

/// Creation response containing the one-time plain key. Never stored or returned again.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyCreated {
    pub key: ApiKeySummary,
    pub token: String,
}

impl fmt::Debug for ApiKeyCreated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyCreated").field("key", &self.key).field("token", &"[redacted]").finish()
    }
}

/// One redacted API key usage log row (no raw key or hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyAuditEntry {
    pub id: String,
    pub key_id: String,
    pub key_name: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub at: i64,
}

/// Combined response for the API keys Settings panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeysView {
    pub keys: Vec<ApiKeySummary>,
    pub audit: Vec<ApiKeyAuditEntry>,
}

/// Validates and normalizes an API key name.
pub fn validate_name(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("API key name cannot be empty.");
    }
    if trimmed.chars().count() > MAX_KEY_NAME_LEN {
        return Err("API key name must be 60 characters or fewer.");
    }
    if trimmed.chars().any(char::is_control) {
        return Err("API key name cannot contain control characters.");
    }
    Ok(trimmed.to_string())
}

/// Deduplicates and canonically orders requested scopes, requiring at least one.
pub fn normalize_scopes(scopes: &[ApiKeyScope]) -> Result<Vec<ApiKeyScope>, &'static str> {
    if scopes.is_empty() {
        return Err("Pick at least one scope for the API key.");
    }
    let set: BTreeSet<ApiKeyScope> = scopes.iter().copied().collect();
    Ok(set.into_iter().collect())
}

/// Validates an optional expiry timestamp against `now_ms`.
pub fn validate_expiry(expires_at: Option<i64>, now_ms: i64) -> Result<Option<i64>, &'static str> {
    let Some(exp) = expires_at else {
        return Ok(None);
    };
    if exp <= now_ms {
        return Err("Expiry time must be in the future.");
    }
    let max_exp = now_ms.saturating_add(MAX_EXPIRY_HORIZON_MS);
    if exp > max_exp {
        return Err("Expiry time cannot be more than 5 years in the future.");
    }
    Ok(Some(exp))
}

/// Builds a short display prefix (`fc_key_<8 hex>...`) safe to show in the UI and audit lists.
#[must_use]
pub fn display_prefix(token: &str) -> String {
    let head: String = token.chars().take(KEY_PREFIX.len() + 8).collect();
    format!("{head}...")
}

/// Returns `true` if a bearer token has the `fc_key_` API key prefix (valid or malformed).
#[must_use]
pub fn has_api_key_prefix(token: &str) -> bool {
    token.starts_with(KEY_PREFIX)
}

/// Returns `true` if a token is syntactically well-formed (`fc_key_` + 64 lowercase hex digits).
#[must_use]
pub fn is_well_formed_token(token: &str) -> bool {
    let Some(rest) = token.strip_prefix(KEY_PREFIX) else {
        return false;
    };
    rest.len() == KEY_HEX_LEN && rest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_secret_token() {
        let created = ApiKeyCreated {
            key: ApiKeySummary {
                id: "k1".into(),
                name: "Stream Deck".into(),
                prefix: "fc_key_12345678...".into(),
                scopes: vec![ApiKeyScope::StudioControl],
                created_at: 1000,
                expires_at: None,
                last_used_at: None,
                revoked_at: None,
            },
            token: format!("{KEY_PREFIX}{}", "a".repeat(KEY_HEX_LEN)),
        };
        let rendered = format!("{created:?}");
        assert!(rendered.contains("[redacted]"));
        assert!(!rendered.contains(&"a".repeat(KEY_HEX_LEN)));
    }

    #[test]
    fn name_and_scope_validation_rejects_bad_inputs() {
        assert!(validate_name("   ").is_err());
        assert!(validate_name("a\nb").is_err());
        assert!(validate_name(&"x".repeat(MAX_KEY_NAME_LEN + 1)).is_err());
        assert_eq!(validate_name("  My Deck  ").unwrap(), "My Deck");

        assert!(normalize_scopes(&[]).is_err());
        assert_eq!(
            normalize_scopes(&[ApiKeyScope::Settings, ApiKeyScope::ReadOnly, ApiKeyScope::Settings]).unwrap(),
            vec![ApiKeyScope::ReadOnly, ApiKeyScope::Settings]
        );
    }

    #[test]
    fn expiry_and_token_shape_checks() {
        assert!(validate_expiry(Some(1000), 1000).is_err());
        assert!(validate_expiry(Some(999), 1000).is_err());
        assert!(validate_expiry(Some(1000 + MAX_EXPIRY_HORIZON_MS + 1), 1000).is_err());
        assert_eq!(validate_expiry(Some(2000), 1000).unwrap(), Some(2000));
        assert_eq!(validate_expiry(None, 1000).unwrap(), None);

        let valid = format!("{KEY_PREFIX}{}", "0123456789abcdef".repeat(4));
        assert!(is_well_formed_token(&valid));
        assert!(!is_well_formed_token("fc_key_short"));
        assert!(!is_well_formed_token(&valid.to_uppercase()));
    }
}
