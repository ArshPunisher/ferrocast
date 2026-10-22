//! The interface every platform adapter implements (R-17).
//!
//! Adapters translate a platform into [`Event`]s pushed into an [`EventSink`], and carry out
//! actions (send chat, ban, chat modes) through [`Connector`]. Everything above this line
//! is platform-agnostic, so when a platform changes its API only its adapter changes.

use std::time::Duration;

use async_trait::async_trait;
use ferrocast_core::event::{Event, Platform};
use ferrocast_core::moderation::{Capabilities, ModeChange};
use tokio::sync::mpsc;

/// Where adapters send events. Bounded so a flood applies backpressure instead of memory.
pub type EventSink = mpsc::Sender<Event>;

/// Channel size for the event sink: a few seconds of a very busy chat.
pub const SINK_CAPACITY: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectorError {
    #[error("{0} isn't connected")]
    NotConnected(Platform),
    #[error("{platform} doesn't let apps {what}")]
    NotSupported { platform: Platform, what: &'static str },
    #[error("{platform} asked us to slow down; try again in {} s", retry_after.as_secs())]
    RateLimited { platform: Platform, retry_after: Duration },
    #[error("{0} needs you to sign in again")]
    SignInNeeded(Platform),
    #[error("{platform} is missing permission: {scope}")]
    MissingScope { platform: Platform, scope: String },
    #[error("{platform} refused: {message}")]
    Refused { platform: Platform, message: String },
    #[error("couldn't reach {platform}: {message}")]
    Network { platform: Platform, message: String },
}

impl ConnectorError {
    /// Whether retrying later might work (the dashboard offers "Try again" for these).
    pub fn is_temporary(&self) -> bool {
        matches!(self, ConnectorError::RateLimited { .. } | ConnectorError::Network { .. })
    }
}

pub type Result<T> = std::result::Result<T, ConnectorError>;

/// Actions on one platform for the signed-in channel.
#[async_trait]
pub trait Connector: Send + Sync + std::fmt::Debug {
    fn platform(&self) -> Platform;

    /// What this platform lets apps do right now (depends on granted scopes).
    fn capabilities(&self) -> Capabilities;

    /// Sends a chat message as the channel's bot identity, optionally as a reply.
    async fn send_chat(&self, text: &str, reply_to: Option<&str>) -> Result<()>;

    async fn delete_message(&self, message_id: &str) -> Result<()>;

    async fn timeout(&self, user_id: &str, seconds: u32, reason: Option<&str>) -> Result<()>;

    async fn ban(&self, user_id: &str, reason: Option<&str>) -> Result<()>;

    async fn unban(&self, user_id: &str) -> Result<()>;

    async fn set_mode(&self, change: ModeChange) -> Result<()>;

    /// The game another channel last streamed (for shoutouts), when the platform says.
    /// `Ok(None)` when unknown; platforms without a lookup keep this default.
    async fn last_game(&self, _login: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// Changes the stream's title, category, tags and language (fields left `None` stay).
    /// Returns the fields the platform took; fields it has no API for are left out.
    async fn update_stream_info(&self, _info: &StreamInfo) -> Result<Vec<String>> {
        Err(ConnectorError::NotSupported { platform: self.platform(), what: "change stream info" })
    }

    /// Sets the stream's thumbnail (a JPEG or PNG), when the platform lets apps do that.
    async fn set_thumbnail(&self, _image: &[u8], _mime: &str) -> Result<()> {
        Err(ConnectorError::NotSupported { platform: self.platform(), what: "set a stream thumbnail" })
    }

    /// Categories (games) matching `query`, best first, for the stream-info form.
    async fn search_categories(&self, _query: &str) -> Result<Vec<CategoryHit>> {
        Ok(Vec::new())
    }

    /// Puts a marker in the live stream's timeline (Twitch stream markers), when supported.
    async fn create_marker(&self, _description: &str) -> Result<()> {
        Err(ConnectorError::NotSupported { platform: self.platform(), what: "add stream markers" })
    }

    /// When `login` followed the channel, if the platform says (Unix ms).
    async fn followed_at(&self, _user_id: &str) -> Result<Option<i64>> {
        Ok(None)
    }
}

pub use ferrocast_core::stream_info::StreamInfo;

/// One category search result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CategoryHit {
    pub id: String,
    pub name: String,
    pub image_url: Option<String>,
}

/// The longest chat message each platform accepts. Adapters split longer bot replies.
pub fn max_chat_chars(platform: Platform) -> usize {
    match platform {
        Platform::Youtube => 200,
        _ => 500,
    }
}

/// Splits text into chunks of at most `max` characters, at word boundaries when possible.
pub fn split_chat(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        while word.chars().count() > max {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let head: String = word.chars().take(max).collect();
            word = word.chars().skip(max).collect();
            out.push(head);
        }
        let need = if cur.is_empty() { word.chars().count() } else { cur.chars().count() + 1 + word.chars().count() };
        if need > max {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_words_and_hard_splits_giant_words() {
        assert_eq!(split_chat("one two three", 7), vec!["one two", "three"]);
        assert_eq!(split_chat("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(split_chat("  ", 10), Vec::<String>::new());
        let long = "नमस्ते ".repeat(100);
        assert!(split_chat(&long, 200).iter().all(|c| c.chars().count() <= 200));
    }

    #[test]
    fn temporary_errors() {
        assert!(ConnectorError::Network { platform: Platform::Kick, message: "x".into() }.is_temporary());
        assert!(!ConnectorError::SignInNeeded(Platform::Twitch).is_temporary());
    }
}
