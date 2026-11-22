//! Facebook actions for the engine: comments, a live video's title, going live and scheduling.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrocast_connector::{Connector, ConnectorError, Ingest, Result, ScheduledBroadcast, StreamInfo};
use ferrocast_core::event::Platform;
use ferrocast_core::moderation::Capabilities;

use crate::FacebookError;
use crate::graph::Graph;

fn map(e: FacebookError) -> ConnectorError {
    let platform = Platform::Facebook;
    match e {
        FacebookError::Network(m) => ConnectorError::Network { platform, message: m },
        FacebookError::SignInNeeded => ConnectorError::SignInNeeded(platform),
        FacebookError::RateLimited(d) => ConnectorError::RateLimited { platform, retry_after: d },
        FacebookError::Refused(m) | FacebookError::Protocol(m) | FacebookError::SignIn(m) => {
            ConnectorError::Refused { platform, message: m }
        }
    }
}

/// Where a Facebook stream's picture goes before a video is made (read-only: nothing is created).
pub const INGEST_SERVER: &str = "rtmps://live-api-s.facebook.com:443/rtmp/";

#[derive(Debug, Clone)]
pub struct FacebookConnector {
    pub graph: Arc<Graph>,
    /// The live video made by Go live (comments and the title go to it).
    live: Arc<Mutex<Option<String>>>,
}

impl FacebookConnector {
    pub fn new(graph: Arc<Graph>) -> Self {
        FacebookConnector { graph, live: Arc::new(Mutex::new(None)) }
    }

    /// The live video made by Go live, if any.
    pub fn live_video(&self) -> Option<String> {
        self.live.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    async fn the_live_video(&self) -> Result<String> {
        if let Some(id) = self.live_video() {
            return Ok(id);
        }
        self.graph.current_live().await.map_err(map)?.ok_or_else(|| ConnectorError::Refused {
            platform: Platform::Facebook,
            message: "the Page isn't live on Facebook".into(),
        })
    }
}

#[async_trait]
impl Connector for FacebookConnector {
    fn platform(&self) -> Platform {
        Platform::Facebook
    }

    fn capabilities(&self) -> Capabilities {
        // Comments can be answered and removed; a Page can't ban or time out a viewer from an app.
        Capabilities { modes: vec![], can_ban: false, can_timeout: false, can_delete: true }
    }

    async fn send_chat(&self, text: &str, _reply_to: Option<&str>) -> Result<()> {
        let live = self.the_live_video().await?;
        self.graph.post_comment(&live, text).await.map_err(map)
    }

    async fn delete_message(&self, message_id: &str) -> Result<()> {
        self.graph.delete_comment(message_id).await.map_err(map)
    }

    async fn timeout(&self, _user_id: &str, _seconds: u32, _reason: Option<&str>) -> Result<()> {
        Err(ConnectorError::NotSupported { platform: Platform::Facebook, what: "time out a viewer" })
    }

    async fn ban(&self, _user_id: &str, _reason: Option<&str>) -> Result<()> {
        Err(ConnectorError::NotSupported { platform: Platform::Facebook, what: "ban a viewer" })
    }

    async fn unban(&self, _user_id: &str) -> Result<()> {
        Err(ConnectorError::NotSupported { platform: Platform::Facebook, what: "unban a viewer" })
    }

    async fn set_mode(&self, _change: ferrocast_core::moderation::ModeChange) -> Result<()> {
        Err(ConnectorError::NotSupported { platform: Platform::Facebook, what: "change chat modes" })
    }

    async fn update_stream_info(&self, info: &StreamInfo) -> Result<Vec<String>> {
        let live = self.the_live_video().await?;
        self.graph.update_live(&live, info.title.as_deref(), info.description.as_deref()).await.map_err(map)
    }

    /// Read-only: Facebook's server address, so a test or pre-flight never makes a live video.
    async fn peek_ingest(&self) -> Result<String> {
        Ok(INGEST_SERVER.to_string())
    }

    /// Going live: a live video made now, with its own key.
    async fn ingest(&self) -> Result<Ingest> {
        let video = self.graph.create_live_video("Live", None, None).await.map_err(map)?;
        *self.live.lock().unwrap_or_else(|p| p.into_inner()) = Some(video.id.clone());
        Ok(Ingest { url: video.server, key: video.key, backup: None, note: Some("Facebook · new live video".into()) })
    }

    async fn schedule_broadcast(&self, info: &StreamInfo, start_ms: i64) -> Result<ScheduledBroadcast> {
        let title = info.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "Live".to_string());
        let video = self
            .graph
            .create_live_video(&title, info.description.as_deref(), Some(start_ms / 1000))
            .await
            .map_err(map)?;
        Ok(ScheduledBroadcast { id: video.id, url: video.permalink })
    }
}
