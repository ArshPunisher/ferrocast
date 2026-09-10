//! Twitch sign-in with the Device Code flow for a public client (R-45, R-46).
//!
//! No client secret ships in the app and no redirect server is needed: the streamer opens
//! twitch.tv/activate, types a short code, and we poll until Twitch hands us tokens. Refresh
//! tokens are one-time use, so a new one is saved before the old one is forgotten.

use serde::{Deserialize, Serialize};

use crate::{Endpoints, TwitchError, now_ms};

/// Every scope Ferrocast needs this year, asked for at the first sign-in so streamers aren't
/// sent back to Twitch for each new feature (R-22).
pub const SCOPES: &[&str] = &[
    "user:read:chat",
    "user:write:chat",
    "user:bot",
    "channel:bot",
    "moderator:read:followers",
    "moderator:manage:banned_users",
    "moderator:manage:chat_messages",
    "moderator:manage:chat_settings",
    "channel:read:subscriptions",
    "channel:read:redemptions",
    "channel:read:ads",
    "channel:moderate",
    "bits:read",
    // The stream-info editor and stream markers (Modify Channel Information).
    "channel:manage:broadcast",
];

/// What the streamer sees while signing in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// Saved in the secret store as JSON. `expires_at` lets us refresh before Twitch says no.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub scopes: Vec<String>,
}

impl Tokens {
    /// Refresh 5 minutes early so a request never races the expiry.
    pub fn needs_refresh(&self, now: i64) -> bool {
        now >= self.expires_at - 300_000
    }

    /// Scopes from [`SCOPES`] the token doesn't have.
    pub fn missing_scopes(&self) -> Vec<String> {
        SCOPES.iter().filter(|s| !self.scopes.iter().any(|g| g == *s)).map(|s| s.to_string()).collect()
    }
}

/// Who the token belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Identity {
    pub user_id: String,
    pub login: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    #[serde(default)]
    scope: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: String,
}

/// Outcome of one poll while the streamer is typing the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    Pending,
    /// Twitch asked us to poll less often.
    SlowDown,
    Done(Tokens),
}

/// Starts sign-in.
pub async fn start(http: &reqwest::Client, ep: &Endpoints, client_id: &str) -> Result<DeviceCode, TwitchError> {
    let res = http
        .post(format!("{}/oauth2/device", ep.id))
        .form(&[("client_id", client_id), ("scopes", &SCOPES.join(" "))])
        .send()
        .await
        .map_err(TwitchError::network)?;
    if !res.status().is_success() {
        return Err(TwitchError::from_response(res).await);
    }
    res.json().await.map_err(TwitchError::network)
}

/// Polls once. Call every `interval` seconds until `Done` or an error.
pub async fn poll(
    http: &reqwest::Client,
    ep: &Endpoints,
    client_id: &str,
    device_code: &str,
) -> Result<Poll, TwitchError> {
    let res = http
        .post(format!("{}/oauth2/token", ep.id))
        .form(&[
            ("client_id", client_id),
            ("scopes", &SCOPES.join(" ")),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .send()
        .await
        .map_err(TwitchError::network)?;
    if res.status().is_success() {
        let t: TokenResponse = res.json().await.map_err(TwitchError::network)?;
        return Ok(Poll::Done(tokens(t)));
    }
    let body: ErrorBody = res.json().await.unwrap_or(ErrorBody { message: String::new() });
    match body.message.as_str() {
        "authorization_pending" => Ok(Poll::Pending),
        "slow_down" => Ok(Poll::SlowDown),
        "expired_token" => Err(TwitchError::SignIn("The code expired. Start signing in again.".into())),
        "access_denied" => Err(TwitchError::SignIn("Sign-in was cancelled on Twitch.".into())),
        other => Err(TwitchError::SignIn(format!(
            "Twitch refused the sign-in ({other}).",
            other = if other.is_empty() { "no reason" } else { other }
        ))),
    }
}

/// Swaps a refresh token for new tokens. Twitch invalidates the old refresh token, so the
/// caller must save the result before using it (twitch_oauth2's `take()` pitfall, R-45).
pub async fn refresh(
    http: &reqwest::Client,
    ep: &Endpoints,
    client_id: &str,
    refresh_token: &str,
) -> Result<Tokens, TwitchError> {
    let res = http
        .post(format!("{}/oauth2/token", ep.id))
        .form(&[("client_id", client_id), ("grant_type", "refresh_token"), ("refresh_token", refresh_token)])
        .send()
        .await
        .map_err(TwitchError::network)?;
    match res.status().as_u16() {
        200..=299 => Ok(tokens(res.json().await.map_err(TwitchError::network)?)),
        400 | 401 => Err(TwitchError::SignInNeeded),
        _ => Err(TwitchError::from_response(res).await),
    }
}

/// Checks a token and says whose it is.
pub async fn validate(http: &reqwest::Client, ep: &Endpoints, access_token: &str) -> Result<Identity, TwitchError> {
    let res = http
        .get(format!("{}/oauth2/validate", ep.id))
        .header("Authorization", format!("OAuth {access_token}"))
        .send()
        .await
        .map_err(TwitchError::network)?;
    match res.status().as_u16() {
        200..=299 => res.json().await.map_err(TwitchError::network),
        401 => Err(TwitchError::SignInNeeded),
        _ => Err(TwitchError::from_response(res).await),
    }
}

fn tokens(t: TokenResponse) -> Tokens {
    Tokens {
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        expires_at: now_ms() + t.expires_in.clamp(0, 365 * 86_400) * 1000,
        scopes: t.scope,
    }
}
