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

pub use api::{Api, Privacy, PrivacySource};
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
    #[error("YouTube's daily chat limit is used up; it resets after midnight Pacific time")]
    QuotaExceeded,
    /// Too many requests in a short time (`rateLimitExceeded`, `userRateLimitExceeded`, a 429):
    /// not the day's quota. It passes in seconds; ask again after this long.
    #[error("YouTube asked Ferrocast to slow down for a moment")]
    RateLimited(Duration),
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
        let retry_after = res
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body: serde_json::Value = res.json().await.unwrap_or_default();
        let reason = body.pointer("/error/errors/0/reason").and_then(|v| v.as_str()).unwrap_or("");
        let message = body.pointer("/error/message").and_then(|v| v.as_str()).unwrap_or("").to_string();
        match (status, reason) {
            (401, _) => YoutubeError::SignInNeeded,
            // The day's units are gone: nothing works until midnight Pacific.
            (_, "quotaExceeded" | "dailyLimitExceeded") => YoutubeError::QuotaExceeded,
            // Too many requests in a burst: it passes in seconds (5 s unless YouTube says, at most a minute).
            (403 | 429, "rateLimitExceeded" | "userRateLimitExceeded") | (429, _) => {
                YoutubeError::RateLimited(rate_wait(retry_after))
            }
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
                ConnectorError::RateLimited { platform, retry_after: quota_resets_in(now_ms(), quota_jitter_s()) }
            }
            YoutubeError::RateLimited(retry_after) => ConnectorError::RateLimited { platform, retry_after },
            YoutubeError::Refused(message) | YoutubeError::Protocol(message) => {
                ConnectorError::Refused { platform, message }
            }
        }
    }
}

/// How long a burst limit asks to wait: what YouTube said, else 5 s, between 1 s and a minute.
pub(crate) fn rate_wait(said: Option<Duration>) -> Duration {
    said.unwrap_or(Duration::from_secs(5)).clamp(Duration::from_secs(1), Duration::from_secs(60))
}

/// A few minutes of spread, so every Ferrocast doesn't come back at the same second (0 to 299 s).
pub(crate) fn quota_jitter_s() -> u64 {
    let mut b = [0u8; 2];
    if getrandom::fill(&mut b).is_err() {
        return 0;
    }
    u64::from(u16::from_le_bytes(b)) % 300
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil year of a day number.
fn year_of_day(z: i64) -> i64 {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    yoe + era * 400 + i64::from(m <= 2)
}

/// The day number of the `n`th Sunday (1-based) of a month.
fn nth_sunday(year: i64, month: i64, n: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    // 1970-01-01 was a Thursday: (day + 4) % 7 is 0 on a Sunday.
    let to_sunday = (7 - (first + 4).rem_euclid(7)) % 7;
    first + to_sunday + (n - 1) * 7
}

/// Pacific time's offset from UTC in hours at `utc_ms`: -7 in summer time (second Sunday of
/// March at 2:00 to the first Sunday of November at 2:00), else -8.
fn pacific_offset_h(utc_ms: i64) -> i64 {
    const DAY: i64 = 86_400_000;
    const HOUR: i64 = 3_600_000;
    let year = year_of_day(utc_ms.div_euclid(DAY));
    let start = nth_sunday(year, 3, 2) * DAY + 10 * HOUR;
    let end = nth_sunday(year, 11, 1) * DAY + 9 * HOUR;
    if (start..end).contains(&utc_ms) { -7 } else { -8 }
}

/// How long until the day's quota is back: the next midnight in Pacific time (YouTube's units
/// reset then), `jitter_s` seconds after it.
pub(crate) fn quota_resets_in(now_ms: i64, jitter_s: u64) -> Duration {
    const DAY: i64 = 86_400_000;
    const HOUR: i64 = 3_600_000;
    let mut off = pacific_offset_h(now_ms);
    let mut next = 0;
    // The offset can change between now and then (the night the clocks move, at 2:00, not at midnight).
    for _ in 0..2 {
        let local = now_ms + off * HOUR;
        next = (local.div_euclid(DAY) + 1) * DAY - off * HOUR;
        off = pacific_offset_h(next);
    }
    Duration::from_millis(u64::try_from(next - now_ms).unwrap_or(0)) + Duration::from_secs(jitter_s)
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
