//! The Twitch adapter: device-code sign-in, EventSub over WebSocket, Helix for chat and
//! moderation (R-21: no IRC, no PubSub, no unofficial endpoints).
//!
//! Everything platform-specific stays in this crate. The engine sees [`Event`]s in its sink
//! and calls [`TwitchConnector`] through the shared `Connector` trait.
//!
//! [`Event`]: ferrocast_core::event::Event

pub mod auth;
pub mod channel;
pub mod eventsub;
pub mod helix;
pub mod ingest;
pub mod map;

pub mod connector;
mod token;

use std::time::Duration;

use ferrocast_connector::ConnectorError;
use ferrocast_core::event::Platform;

pub use connector::TwitchConnector;
pub use token::Auth;

/// Where Twitch lives. Tests point these at a fake server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub id: String,
    pub api: String,
    pub eventsub: String,
    /// Twitch's ingest list, nearest server first.
    pub ingest: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            id: "https://id.twitch.tv".into(),
            api: "https://api.twitch.tv/helix".into(),
            eventsub: "wss://eventsub.wss.twitch.tv/ws".into(),
            ingest: "https://ingest.twitch.tv".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TwitchError {
    #[error("couldn't reach Twitch: {0}")]
    Network(String),
    #[error("Twitch needs you to sign in again")]
    SignInNeeded,
    #[error("{0}")]
    SignIn(String),
    #[error("Twitch is missing permission: {0}")]
    MissingScope(String),
    #[error("Twitch asked us to slow down")]
    RateLimited(Duration),
    #[error("Twitch refused: {0}")]
    Refused(String),
    #[error("Twitch sent something unexpected: {0}")]
    Protocol(String),
}

impl TwitchError {
    pub(crate) fn network(e: impl std::fmt::Display) -> Self {
        TwitchError::Network(e.to_string())
    }

    /// Turns a non-success Helix/ID response into an error, keeping Twitch's own message.
    pub(crate) async fn from_response(res: reqwest::Response) -> Self {
        let status = res.status().as_u16();
        let reset =
            res.headers().get("ratelimit-reset").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
        let message = res
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
            .unwrap_or_else(|| format!("status {status}"));
        match status {
            // Twitch answers a missing scope with 401 "Missing scope: …".
            401 if message.to_lowercase().contains("missing scope") => TwitchError::MissingScope(message),
            401 => TwitchError::SignInNeeded,
            403 if message.to_lowercase().contains("scope") || message.to_lowercase().contains("permission") => {
                TwitchError::MissingScope(message)
            }
            429 => {
                let wait = reset.map(|r| (r * 1000 - now_ms()).max(1000)).unwrap_or(2000);
                TwitchError::RateLimited(Duration::from_millis(u64::try_from(wait).unwrap_or(2000)))
            }
            _ => TwitchError::Refused(message),
        }
    }
}

impl From<TwitchError> for ConnectorError {
    fn from(e: TwitchError) -> Self {
        let platform = Platform::Twitch;
        match e {
            TwitchError::Network(message) => ConnectorError::Network { platform, message },
            TwitchError::SignInNeeded | TwitchError::SignIn(_) => ConnectorError::SignInNeeded(platform),
            TwitchError::MissingScope(scope) => ConnectorError::MissingScope { platform, scope },
            TwitchError::RateLimited(retry_after) => ConnectorError::RateLimited { platform, retry_after },
            TwitchError::Refused(message) | TwitchError::Protocol(message) => {
                ConnectorError::Refused { platform, message }
            }
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// One HTTP client for everything Twitch, with sane timeouts.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("Ferrocast/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
