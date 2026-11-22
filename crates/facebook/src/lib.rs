//! The Facebook adapter: Graph API sign-in (a Page), live videos (going live now, or scheduled
//! for later) and live comments, all through Facebook's official API. No scraping, no unofficial
//! sockets. The Page access token it ends up with is a secret: it only ever goes in a header
//! and in the secret store, never in a URL, a log line or an error message.

pub mod feed;
pub mod graph;
pub mod oauth;

mod connector;

use std::time::Duration;

pub use connector::FacebookConnector;
pub use graph::{Comment, Graph, LiveVideo, Page};
pub use oauth::{AppCredentials, UserGrant};

/// What Ferrocast asks for: the Pages the person manages, going live and scheduling on one, and
/// reading and answering the comments on it.
pub const SCOPES: &[&str] = &[
    "pages_show_list",
    "pages_read_engagement",
    "pages_manage_posts",
    "pages_manage_engagement",
    "pages_read_user_content",
    "publish_video",
];

/// Where Facebook's sign-in and Graph API are (tests point both at a fake).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub authorize: String,
    pub graph: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            authorize: "https://www.facebook.com/v21.0/dialog/oauth".into(),
            graph: "https://graph.facebook.com/v21.0".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FacebookError {
    #[error("couldn't reach Facebook: {0}")]
    Network(String),
    #[error("Facebook needs you to sign in again")]
    SignInNeeded,
    #[error("Facebook asked Ferrocast to slow down")]
    RateLimited(Duration),
    #[error("Facebook refused: {0}")]
    Refused(String),
    #[error("Facebook sent something unexpected: {0}")]
    Protocol(String),
    #[error("{0}")]
    SignIn(String),
}

impl FacebookError {
    /// A failed request, said without the address it went to.
    pub(crate) fn from_reqwest(e: reqwest::Error) -> Self {
        FacebookError::Network(e.without_url().to_string())
    }

    /// Facebook's answer to a refused call: `{ "error": { "message", "code" } }`.
    pub(crate) async fn from_response(res: reqwest::Response) -> Self {
        let status = res.status().as_u16();
        let body: serde_json::Value = res.json().await.unwrap_or_default();
        let code = body.pointer("/error/code").and_then(serde_json::Value::as_i64).unwrap_or(0);
        let message = body
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("it said no without a reason")
            .to_string();
        match code {
            190 | 102 => FacebookError::SignInNeeded,
            // Rate and call-volume limits.
            4 | 17 | 32 | 341 | 613 => FacebookError::RateLimited(Duration::from_secs(60)),
            _ if status == 401 => FacebookError::SignInNeeded,
            _ if status == 429 => FacebookError::RateLimited(Duration::from_secs(60)),
            _ => FacebookError::Refused(message.chars().take(300).collect()),
        }
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("Ferrocast/", env!("CARGO_PKG_VERSION")))
        // A redirect would carry the token somewhere else.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests;
