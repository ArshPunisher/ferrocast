//! Ferrocast Bond (Pro): the relay vouches for a Pro subscription with a short-lived token
//! the Bond service checks (HMAC-SHA256 with the secret both have, `BOND_SECRET`), and says
//! where the service is. The relay never sees the stream or its keys.
//!
//! Settings: `BOND_SECRET` (32+ characters, the same on the Bond service), `BOND_API_URL`
//! (its HTTPS address), `BOND_SRTLA_HOST` and `BOND_SRTLA_PORT` (where the stream goes, UDP;
//! 5000 by default). Off unless the first three are set.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use hmac::{Hmac, Mac};
use rsa::sha2::{Digest, Sha256};
use serde_json::{Value, json};

use crate::billing::gate::require_pro;
use crate::{ApiError, AppState, now_ms};

/// A token is good for this long: a stream that runs longer asks again.
const TOKEN_FOR_MS: i64 = 6 * 3_600_000;

/// Where the Bond service is, and the secret its tokens are signed with.
#[derive(Clone, Default)]
pub struct Runtime {
    secret: Option<Vec<u8>>,
    api: Option<String>,
    host: Option<String>,
    port: u16,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").field("api", &self.api).field("host", &self.host).finish_non_exhaustive()
    }
}

impl Runtime {
    pub fn new(secret: Option<String>, api: Option<String>, host: Option<String>, port: Option<u16>) -> Self {
        Runtime {
            secret: secret.filter(|s| s.len() >= 32).map(String::into_bytes),
            api: api.map(|a| a.trim_end_matches('/').to_string()),
            host,
            port: port.unwrap_or(5000),
        }
    }

    pub fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Runtime::new(
            env("BOND_SECRET"),
            env("BOND_API_URL"),
            env("BOND_SRTLA_HOST"),
            env("BOND_SRTLA_PORT").and_then(|p| p.parse().ok()),
        )
    }
}

/// `features.bond`: set up on this relay.
pub(crate) fn available(st: &AppState) -> bool {
    let b = &st.bond;
    b.secret.is_some() && b.api.is_some() && b.host.is_some()
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/v1/pro/bond/token", post(token))
}

/// A token's account: the subscription, unreadable (a hash), in the characters a token allows.
fn account_of(sub_id: &str) -> String {
    let h = Sha256::digest(format!("ferrocast-bond:{sub_id}").as_bytes());
    h[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// `v1.<account>.<expires ms>.<hex HMAC-SHA256 of "v1.<account>.<expires ms>">` (the Bond
/// service's `token::verify` reads it).
pub fn sign(secret: &[u8], account: &str, expires_ms: i64) -> Option<String> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret).ok()?;
    m.update(format!("v1.{account}.{expires_ms}").as_bytes());
    let sig: String = m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("v1.{account}.{expires_ms}.{sig}"))
}

/// `POST /v1/pro/bond/token` → `{ token, expires_ms, api, srtla_host, srtla_port }`.
async fn token(State(st): State<AppState>, headers: HeaderMap) -> Result<axum::Json<Value>, ApiError> {
    let ctx = require_pro(&st, &headers).await?;
    let b = &st.bond;
    let (Some(secret), Some(api), Some(host)) = (&b.secret, &b.api, &b.host) else {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "bond_off", "Ferrocast Bond isn't running yet.")
            .hint("Stream from your computer as usual; Bond comes in an update."));
    };
    let expires_ms = now_ms() + TOKEN_FOR_MS;
    let token = sign(secret, &account_of(&ctx.sub_id), expires_ms).ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "bond_off", "Ferrocast Bond isn't set up right.")
    })?;
    Ok(axum::Json(
        json!({ "token": token, "expires_ms": expires_ms, "api": api, "srtla_host": host, "srtla_port": b.port }),
    ))
}

#[cfg(test)]
mod tests;
