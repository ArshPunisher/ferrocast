//! The YouTube adapter (R-24): Google sign-in with PKCE, live chat over the gRPC
//! `liveChatMessages.streamList` (no polling, no scraping), and REST for sending, deleting
//! and banning.
//!
//! Quota is the constraint: every user of the app shares one Google project's daily
//! units. So the adapter looks for a live chat only on start, when another platform goes
//! live, and every ten minutes, and holds one long-lived stream while live.

pub mod api;
pub mod connector;
pub mod feed;
pub mod map;
pub mod oauth;
pub mod stream;

#[allow(clippy::all, clippy::pedantic, missing_debug_implementations, unreachable_pub)]
pub mod proto {
    tonic::include_proto!("youtube.api.v3");
}

use std::time::Duration;

use ferrocast_connector::ConnectorError;
use ferrocast_core::event::Platform;

pub use api::Api;
pub use connector::YoutubeConnector;
pub use oauth::{AppCredentials, Tokens};

/// Where Google lives. Tests point these at fakes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub api: String,
    pub grpc: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            api: "https://www.googleapis.com/youtube/v3".into(),
            grpc: "https://youtube.googleapis.com".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum YoutubeError {
    #[error("couldn't reach YouTube: {0}")]
    Network(String),
    #[error("YouTube needs you to sign in again")]
    SignInNeeded,
    #[error("{0}")]
    SignIn(String),
    #[error("YouTube's daily limit for Ferrocast is used up; chat comes back after midnight Pacific time")]
    QuotaExceeded,
    #[error("YouTube refused: {0}")]
    Refused(String),
    #[error("YouTube sent something unexpected: {0}")]
    Protocol(String),
}

impl YoutubeError {
    pub(crate) fn network(e: impl std::fmt::Display) -> Self {
        YoutubeError::Network(e.to_string())
    }

    /// Maps a non-success REST response, recognising quota and auth problems.
    pub(crate) async fn from_response(res: reqwest::Response) -> Self {
        let status = res.status().as_u16();
        let body: serde_json::Value = res.json().await.unwrap_or_default();
        let reason = body.pointer("/error/errors/0/reason").and_then(|v| v.as_str()).unwrap_or("");
        let message = body.pointer("/error/message").and_then(|v| v.as_str()).unwrap_or("").to_string();
        match (status, reason) {
            (401, _) => YoutubeError::SignInNeeded,
            (403, "quotaExceeded" | "rateLimitExceeded" | "dailyLimitExceeded") => YoutubeError::QuotaExceeded,
            _ if message.is_empty() => YoutubeError::Refused(format!("status {status}")),
            _ => YoutubeError::Refused(message),
        }
    }
}

impl From<YoutubeError> for ConnectorError {
    fn from(e: YoutubeError) -> Self {
        let platform = Platform::Youtube;
        match e {
            YoutubeError::Network(message) => ConnectorError::Network { platform, message },
            YoutubeError::SignInNeeded | YoutubeError::SignIn(_) => ConnectorError::SignInNeeded(platform),
            YoutubeError::QuotaExceeded => {
                ConnectorError::RateLimited { platform, retry_after: Duration::from_secs(3600) }
            }
            YoutubeError::Refused(message) | YoutubeError::Protocol(message) => {
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
