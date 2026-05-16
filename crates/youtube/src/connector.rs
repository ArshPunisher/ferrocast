//! YouTube actions for the engine. YouTube has no API for chat modes (slow mode,
//! members-only), so panic mode says so instead of pretending (R-09).

use std::sync::Arc;

use async_trait::async_trait;
use ferrocast_connector::{Connector, ConnectorError, Result};
use ferrocast_core::event::Platform;
use ferrocast_core::moderation::{Capabilities, ModeChange};

use crate::api::Api;
use crate::stream::LiveChat;

#[derive(Debug, Clone)]
pub struct YoutubeConnector {
    pub api: Arc<Api>,
    pub live: LiveChat,
}

impl YoutubeConnector {
    fn chat(&self) -> Result<String> {
        self.live.read().unwrap_or_else(|p| p.into_inner()).clone().ok_or(ConnectorError::Refused {
            platform: Platform::Youtube,
            message: "you aren't live on YouTube right now".into(),
        })
    }
}

#[async_trait]
impl Connector for YoutubeConnector {
    fn platform(&self) -> Platform {
        Platform::Youtube
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { modes: vec![], can_ban: true, can_timeout: true, can_delete: true }
    }

    async fn send_chat(&self, text: &str, _reply_to: Option<&str>) -> Result<()> {
        // YouTube chat has no replies; the text goes out as a plain message.
        let chat = self.chat()?;
        Ok(self.api.send(&chat, text).await?)
    }

    async fn delete_message(&self, message_id: &str) -> Result<()> {
        Ok(self.api.delete(message_id).await?)
    }

    async fn timeout(&self, user_id: &str, seconds: u32, _reason: Option<&str>) -> Result<()> {
        let chat = self.chat()?;
        Ok(self.api.ban(&chat, user_id, Some(seconds)).await?)
    }

    async fn ban(&self, user_id: &str, _reason: Option<&str>) -> Result<()> {
        let chat = self.chat()?;
        Ok(self.api.ban(&chat, user_id, None).await?)
    }

    async fn unban(&self, user_id: &str) -> Result<()> {
        if self.api.unban(user_id).await? {
            Ok(())
        } else {
            Err(ConnectorError::NotSupported {
                platform: Platform::Youtube,
                what: "unban someone banned outside Ferrocast (use YouTube Studio)",
            })
        }
    }

    async fn set_mode(&self, _change: ModeChange) -> Result<()> {
        Err(ConnectorError::NotSupported { platform: Platform::Youtube, what: "change chat modes" })
    }
}
