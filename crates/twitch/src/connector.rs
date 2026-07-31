//! Twitch actions for the engine (send chat, moderate, chat modes).

use async_trait::async_trait;
use ferrocast_connector::{Connector, Result};
use ferrocast_core::event::{ChatMode, Platform};
use ferrocast_core::moderation::{Capabilities, ModeChange};
use serde_json::json;

use crate::helix::Helix;

/// The signed-in channel. The streamer is broadcaster, moderator and bot sender.
#[derive(Debug, Clone)]
pub struct TwitchConnector {
    pub helix: Helix,
    pub user_id: String,
}

#[async_trait]
impl Connector for TwitchConnector {
    fn platform(&self) -> Platform {
        Platform::Twitch
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            modes: vec![
                ChatMode::FollowersOnly,
                ChatMode::SubscribersOnly,
                ChatMode::EmoteOnly,
                ChatMode::Slow,
                ChatMode::UniqueChat,
            ],
            can_ban: true,
            can_timeout: true,
            can_delete: true,
        }
    }

    async fn send_chat(&self, text: &str, reply_to: Option<&str>) -> Result<()> {
        Ok(self.helix.send_chat(&self.user_id, &self.user_id, text, reply_to).await?)
    }

    async fn delete_message(&self, message_id: &str) -> Result<()> {
        Ok(self.helix.delete_message(&self.user_id, &self.user_id, message_id).await?)
    }

    async fn timeout(&self, user_id: &str, seconds: u32, reason: Option<&str>) -> Result<()> {
        Ok(self.helix.ban(&self.user_id, &self.user_id, user_id, Some(seconds), reason).await?)
    }

    async fn ban(&self, user_id: &str, reason: Option<&str>) -> Result<()> {
        Ok(self.helix.ban(&self.user_id, &self.user_id, user_id, None, reason).await?)
    }

    async fn unban(&self, user_id: &str) -> Result<()> {
        Ok(self.helix.unban(&self.user_id, &self.user_id, user_id).await?)
    }

    async fn set_mode(&self, change: ModeChange) -> Result<()> {
        Ok(self.helix.chat_settings(&self.user_id, &self.user_id, mode_patch(change)).await?)
    }

    async fn last_game(&self, login: &str) -> Result<Option<String>> {
        Ok(self.helix.channel_game(login).await?)
    }
}

/// The Helix chat-settings body for one change.
pub fn mode_patch(c: ModeChange) -> serde_json::Value {
    match c.mode {
        ChatMode::FollowersOnly if c.enabled => {
            json!({ "follower_mode": true, "follower_mode_duration": c.value.unwrap_or(0).min(129_600) })
        }
        ChatMode::FollowersOnly => json!({ "follower_mode": false }),
        ChatMode::Slow if c.enabled => {
            json!({ "slow_mode": true, "slow_mode_wait_time": c.value.unwrap_or(10).clamp(3, 120) })
        }
        ChatMode::Slow => json!({ "slow_mode": false }),
        ChatMode::EmoteOnly => json!({ "emote_mode": c.enabled }),
        ChatMode::SubscribersOnly => json!({ "subscriber_mode": c.enabled }),
        ChatMode::UniqueChat => json!({ "unique_chat_mode": c.enabled }),
    }
}
