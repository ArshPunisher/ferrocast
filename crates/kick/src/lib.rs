//! The Kick adapter (R-25).
//!
//! Kick delivers events only as webhooks to a public URL, so they reach the desktop through
//! Ferrocast's relay (which verifies Kick's signature). This crate maps those payloads and
//! calls Kick's official public API for chat and moderation. No unofficial chat socket.

pub mod api;
pub mod map;

mod connector;

use std::time::Duration;

use ferrocast_connector::ConnectorError;
use ferrocast_core::event::Platform;

pub use api::{KickApi, KickTokens};
pub use connector::KickConnector;

/// The webhook events Ferrocast subscribes to, as `(name, version)`.
pub const EVENTS: &[(&str, u32)] = &[
    ("chat.message.sent", 1),
    ("channel.followed", 1),
    ("channel.subscription.new", 1),
    ("channel.subscription.renewal", 1),
    ("channel.subscription.gifts", 1),
    ("channel.reward.redemption.updated", 1),
    ("livestream.status.updated", 1),
    ("moderation.banned", 1),
    ("kicks.gifted", 1),
];

/// Every scope Ferrocast uses, asked for once (R-22).
pub const SCOPES: &[&str] = &[
    "user:read",
    "channel:read",
    "chat:write",
    "events:subscribe",
    "moderation:ban",
    "moderation:chat_message:manage",
    "channel:rewards:read",
    "kicks:read",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KickError {
    #[error("couldn't reach Kick: {0}")]
    Network(String),
    #[error("Kick needs you to sign in again")]
    SignInNeeded,
    #[error("Kick asked us to slow down")]
    RateLimited(Duration),
    #[error("Kick refused: {0}")]
    Refused(String),
}

impl KickError {
    pub(crate) fn network(e: impl std::fmt::Display) -> Self {
        KickError::Network(e.to_string())
    }

    pub(crate) async fn from_response(res: reqwest::Response) -> Self {
        let status = res.status().as_u16();
        let retry = res
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2);
        let message = res
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
            .unwrap_or_else(|| format!("status {status}"));
        match status {
            401 => KickError::SignInNeeded,
            429 => KickError::RateLimited(Duration::from_secs(retry.clamp(1, 600))),
            _ => KickError::Refused(message),
        }
    }
}

impl From<KickError> for ConnectorError {
    fn from(e: KickError) -> Self {
        let platform = Platform::Kick;
        match e {
            KickError::Network(message) => ConnectorError::Network { platform, message },
            KickError::SignInNeeded => ConnectorError::SignInNeeded(platform),
            KickError::RateLimited(retry_after) => ConnectorError::RateLimited { platform, retry_after },
            KickError::Refused(message) => ConnectorError::Refused { platform, message },
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
