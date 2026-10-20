//! Google sign-in: the authorization URL (code + PKCE S256, `state`, `nonce`), the code
//! exchange (with the client secret, server to server) and the `id_token` check against
//! Google's JWKS: RS256 only, the `kid`'s key, `iss`, `aud`, `exp`, `iat`, `nonce`,
//! `email_verified`. Keys are cached for an hour and fetched again (at most once a minute) for
//! an unknown `kid`, so Google's key rotation needs nothing from us.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use base64::Engine as _;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::sha2::Sha256;
use rsa::signature::Verifier;
use rsa::{BigUint, RsaPublicKey};
use serde_json::Value;

pub const ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];
/// Clock difference allowed on `exp` and `iat`.
pub const LEEWAY_S: i64 = 120;
const KEYS_FRESH: Duration = Duration::from_secs(3600);
const REFETCH_GAP: Duration = Duration::from_secs(60);

/// Google's endpoints and the client (`RELAY_GOOGLE_CLIENT_ID` / `_SECRET`). Debug output never
/// shows the secret.
#[derive(Clone)]
pub struct GoogleConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub auth_url: String,
    pub token_url: String,
    pub jwks_url: String,
}

impl Default for GoogleConfig {
    fn default() -> Self {
        GoogleConfig {
            client_id: None,
            client_secret: None,
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            jwks_url: "https://www.googleapis.com/oauth2/v3/certs".into(),
        }
    }
}

impl std::fmt::Debug for GoogleConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "<set>"))
            .field("auth_url", &self.auth_url)
            .field("token_url", &self.token_url)
            .field("jwks_url", &self.jwks_url)
            .finish()
    }
}

impl GoogleConfig {
    pub fn from_env() -> GoogleConfig {
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let d = GoogleConfig::default();
        GoogleConfig {
            client_id: env("RELAY_GOOGLE_CLIENT_ID"),
            client_secret: env("RELAY_GOOGLE_CLIENT_SECRET"),
            auth_url: env("RELAY_GOOGLE_AUTH_URL").unwrap_or(d.auth_url),
            token_url: env("RELAY_GOOGLE_TOKEN_URL").unwrap_or(d.token_url),
            jwks_url: env("RELAY_GOOGLE_JWKS_URL").unwrap_or(d.jwks_url),
        }
    }

    /// Both halves of the client are set.
    pub fn on(&self) -> bool {
        self.client_id.is_some() && self.client_secret.is_some()
    }
}

/// What a verified `id_token` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    pub sub: String,
    pub email: String,
    pub name: Option<String>,
    pub picture: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("the id_token isn't a well-formed JWT")]
    Malformed,
    #[error("the id_token uses {0}, not RS256")]
    Alg(String),
    #[error("no Google key with that kid")]
    UnknownKey,
    #[error("the id_token's signature is wrong")]
    Signature,
    #[error("the id_token's {0} is wrong")]
    Claim(&'static str),
    #[error("Google says this email isn't verified")]
    Unverified,
    #[error("couldn't get Google's keys: {0}")]
    Keys(String),
}

#[derive(Debug, Default)]
pub struct KeyCache {
    keys: RwLock<Option<(HashMap<String, RsaPublicKey>, Instant)>>,
}

fn b64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// A JWKS document → RSA keys by `kid` (other key types left out).
pub fn parse_jwks(v: &Value) -> HashMap<String, RsaPublicKey> {
    let mut out = HashMap::new();
    for k in v["keys"].as_array().into_iter().flatten() {
        if k["kty"].as_str() != Some("RSA") || k["alg"].as_str().is_some_and(|a| a != "RS256") {
            continue;
        }
        let (Some(kid), Some(n), Some(e)) = (k["kid"].as_str(), k["n"].as_str(), k["e"].as_str()) else { continue };
        let (Some(n), Some(e)) = (b64(n), b64(e)) else { continue };
        if let Ok(key) = RsaPublicKey::new(BigUint::from_bytes_be(&n), BigUint::from_bytes_be(&e)) {
            out.insert(kid.to_string(), key);
        }
    }
    out
}

impl KeyCache {
    async fn fetch(&self, http: &reqwest::Client, url: &str) -> Result<HashMap<String, RsaPublicKey>, TokenError> {
        let res = http.get(url).send().await.map_err(|e| TokenError::Keys(e.without_url().to_string()))?;
        if !res.status().is_success() {
            return Err(TokenError::Keys(format!("HTTP {}", res.status().as_u16())));
        }
        let v: Value = res.json().await.map_err(|e| TokenError::Keys(e.without_url().to_string()))?;
        let keys = parse_jwks(&v);
        if keys.is_empty() {
            return Err(TokenError::Keys("no RSA keys in the answer".into()));
        }
        *self.keys.write().unwrap_or_else(|p| p.into_inner()) = Some((keys.clone(), Instant::now()));
        Ok(keys)
    }

    async fn key(&self, http: &reqwest::Client, url: &str, kid: &str) -> Result<RsaPublicKey, TokenError> {
        let cached = self.keys.read().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some((keys, at)) = &cached {
            if let Some(k) = keys.get(kid)
                && at.elapsed() < KEYS_FRESH
            {
                return Ok(k.clone());
            }
            // Unknown kid: Google rotated. Ask again, but not more than once a minute.
            if at.elapsed() < REFETCH_GAP {
                return keys.get(kid).cloned().ok_or(TokenError::UnknownKey);
            }
        }
        self.fetch(http, url).await?.get(kid).cloned().ok_or(TokenError::UnknownKey)
    }
}

/// Checks an `id_token` and returns its claims.
pub async fn verify(
    cache: &KeyCache,
    http: &reqwest::Client,
    cfg: &GoogleConfig,
    token: &str,
    nonce: &str,
    now_ms: i64,
) -> Result<Claims, TokenError> {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(TokenError::Malformed);
    };
    let header: Value = b64(h).and_then(|b| serde_json::from_slice(&b).ok()).ok_or(TokenError::Malformed)?;
    let alg = header["alg"].as_str().unwrap_or("");
    if alg != "RS256" {
        return Err(TokenError::Alg(alg.chars().take(16).collect()));
    }
    let kid = header["kid"].as_str().filter(|k| !k.is_empty() && k.len() <= 128).ok_or(TokenError::Malformed)?;
    let key = cache.key(http, &cfg.jwks_url, kid).await?;
    let sig = b64(s).and_then(|b| Signature::try_from(b.as_slice()).ok()).ok_or(TokenError::Malformed)?;
    let signed = format!("{h}.{p}");
    VerifyingKey::<Sha256>::new(key).verify(signed.as_bytes(), &sig).map_err(|_| TokenError::Signature)?;
    let c: Value = b64(p).and_then(|b| serde_json::from_slice(&b).ok()).ok_or(TokenError::Malformed)?;
    claims(&c, cfg.client_id.as_deref().unwrap_or(""), nonce, now_ms)
}

/// The claim checks, on an already signature-checked payload.
pub fn claims(c: &Value, client_id: &str, nonce: &str, now_ms: i64) -> Result<Claims, TokenError> {
    let now = now_ms / 1000;
    if !c["iss"].as_str().is_some_and(|i| ISSUERS.contains(&i)) {
        return Err(TokenError::Claim("iss"));
    }
    let aud_ok = match &c["aud"] {
        Value::String(a) => a == client_id,
        Value::Array(a) => {
            a.iter().any(|x| x.as_str() == Some(client_id)) && c["azp"].as_str().is_some_and(|z| z == client_id)
        }
        _ => false,
    };
    if client_id.is_empty() || !aud_ok {
        return Err(TokenError::Claim("aud"));
    }
    if !c["exp"].as_i64().is_some_and(|e| e + LEEWAY_S > now) {
        return Err(TokenError::Claim("exp"));
    }
    if !c["iat"].as_i64().is_some_and(|i| i <= now + LEEWAY_S) {
        return Err(TokenError::Claim("iat"));
    }
    let n = c["nonce"].as_str().unwrap_or("");
    if nonce.is_empty() || !bool::from(subtle::ConstantTimeEq::ct_eq(n.as_bytes(), nonce.as_bytes())) {
        return Err(TokenError::Claim("nonce"));
    }
    let sub = c["sub"].as_str().filter(|s| !s.is_empty() && s.len() <= 255).ok_or(TokenError::Claim("sub"))?;
    let verified = match &c["email_verified"] {
        Value::Bool(b) => *b,
        Value::String(s) => s == "true",
        _ => false,
    };
    let email = c["email"].as_str().map(|e| e.trim().to_lowercase()).ok_or(TokenError::Claim("email"))?;
    if !crate::billing::valid_email(&email) {
        return Err(TokenError::Claim("email"));
    }
    if !verified {
        return Err(TokenError::Unverified);
    }
    let text = |k: &str, max: usize| c[k].as_str().map(|s| s.trim().chars().take(max).collect::<String>());
    let picture = text("picture", 512).filter(|p| p.starts_with("https://"));
    Ok(Claims {
        sub: sub.to_string(),
        email,
        name: text("name", 80).filter(|n| !n.is_empty() && !n.chars().any(char::is_control)),
        picture,
    })
}
