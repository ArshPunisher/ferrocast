//! Kick actions for the engine. Kick's public API has no chat-mode endpoints yet, so panic
//! mode lists Kick's modes as unsupported rather than pretending (R-09).

use std::sync::Arc;

use async_trait::async_trait;
use ferrocast_connector::{Connector, ConnectorError, Result};
use ferrocast_core::event::Platform;
use ferrocast_core::moderation::{Capabilities, ModeChange};

use crate::api::KickApi;

#[derive(Debug, Clone)]
pub struct KickConnector {
    pub api: Arc<KickApi>,
    pub broadcaster_id: String,
}

#[async_trait]
impl Connector for KickConnector {
    fn platform(&self) -> Platform {
        Platform::Kick
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { modes: vec![], can_ban: true, can_timeout: true, can_delete: true }
    }

    async fn send_chat(&self, text: &str, reply_to: Option<&str>) -> Result<()> {
        Ok(self.api.send_chat(&self.broadcaster_id, text, reply_to).await?)
    }

    async fn delete_message(&self, message_id: &str) -> Result<()> {
        Ok(self.api.delete_message(message_id).await?)
    }

    async fn timeout(&self, user_id: &str, seconds: u32, reason: Option<&str>) -> Result<()> {
        Ok(self.api.ban(&self.broadcaster_id, user_id, Some(seconds), reason).await?)
    }

    async fn ban(&self, user_id: &str, reason: Option<&str>) -> Result<()> {
        Ok(self.api.ban(&self.broadcaster_id, user_id, None, reason).await?)
    }

    async fn unban(&self, user_id: &str) -> Result<()> {
        Ok(self.api.unban(&self.broadcaster_id, user_id).await?)
    }

    async fn update_stream_info(&self, info: &ferrocast_connector::StreamInfo) -> Result<Vec<String>> {
        Ok(self.api.update_channel(info).await?)
    }

    async fn ingest(&self) -> Result<ferrocast_connector::Ingest> {
        Ok(self.api.ingest().await?)
    }

    async fn search_categories(&self, query: &str) -> Result<Vec<ferrocast_connector::CategoryHit>> {
        Ok(self.api.search_categories(query).await?)
    }

    async fn set_mode(&self, _change: ModeChange) -> Result<()> {
        Err(ConnectorError::NotSupported { platform: Platform::Kick, what: "change chat modes yet" })
    }
}
