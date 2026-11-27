//! YouTube actions for the engine. YouTube has no API for chat modes (slow mode,
//! members-only), so panic mode says so instead of pretending (R-09).

use std::sync::Arc;

use async_trait::async_trait;
use ferrocast_connector::{Connector, ConnectorError, NativePollSpec, NativePollTally, Result};
use ferrocast_core::event::Platform;
use ferrocast_core::moderation::{Capabilities, ModeChange};
use ferrocast_core::poll::{PlatformPollCapability, PollCapabilityMode};

use crate::api::{Api, YOUTUBE_POLL_LIMITS};
use crate::stream::LiveChat;
use crate::{YoutubeError, now_ms, quota_jitter_s, quota_resets_in};

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

    fn map_err(&self, err: YoutubeError) -> ConnectorError {
        if matches!(err, YoutubeError::QuotaExceeded) {
            let retry_after = quota_resets_in(now_ms(), quota_jitter_s());
            self.api.note_quota_exceeded(retry_after);
            return ConnectorError::RateLimited { platform: Platform::Youtube, retry_after };
        }
        ConnectorError::from(err)
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

    async fn update_stream_info(&self, info: &ferrocast_connector::StreamInfo) -> Result<Vec<String>> {
        Ok(self.api.update_broadcast_info(info).await?)
    }

    async fn schedule_broadcast(
        &self,
        info: &ferrocast_connector::StreamInfo,
        start_ms: i64,
    ) -> Result<ferrocast_connector::ScheduledBroadcast> {
        let title = info.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "Live".to_string());
        let id = self.api.schedule_broadcast(&title, info.description.as_deref(), start_ms).await?;
        let url = Some(format!("https://www.youtube.com/watch?v={id}"));
        Ok(ferrocast_connector::ScheduledBroadcast { id, url })
    }

    async fn search_categories(&self, query: &str) -> Result<Vec<ferrocast_connector::CategoryHit>> {
        Ok(crate::api::search_video_categories(query))
    }

    async fn set_thumbnail(&self, image: &[u8], mime: &str) -> Result<()> {
        Ok(self.api.set_thumbnail(image, mime).await?)
    }

    async fn last_vod(&self, since_ms: i64) -> Result<Option<(String, i64)>> {
        Ok(self.api.last_broadcast(since_ms).await?)
    }

    async fn set_chapters(&self, video: &str, chapters: &str) -> Result<()> {
        Ok(self.api.set_chapters(video, chapters).await?)
    }

    async fn upload_video(
        &self,
        meta: &ferrocast_connector::VideoUpload,
        source: &mut dyn ferrocast_connector::UploadSource,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<String> {
        Ok(self.api.upload_video(meta, source, progress).await?)
    }

    async fn peek_ingest(&self) -> Result<String> {
        Ok(self.api.peek_ingest().await?)
    }

    async fn peek_vertical_ingest(&self) -> Result<String> {
        Ok(self.api.peek_vertical_ingest().await?)
    }

    async fn ingest(&self) -> Result<ferrocast_connector::Ingest> {
        Ok(self.api.ingest().await?)
    }

    fn set_vertical_keys(&self, keys: Vec<String>) {
        self.api.set_vertical_keys(keys);
    }

    async fn vertical_ingest(&self) -> Result<ferrocast_connector::Ingest> {
        Ok(self.api.vertical_ingest().await?)
    }

    async fn viewer_count(&self) -> Result<Option<u64>> {
        if let Some(retry_after) = self.api.quota_retry_after() {
            return Err(ConnectorError::RateLimited { platform: Platform::Youtube, retry_after });
        }
        let mut ids = self.api.active_broadcast_ids();
        let used_cache = !ids.is_empty();
        if ids.is_empty() {
            let active = self.api.active_broadcasts().await.map_err(|e| self.map_err(e))?;
            ids = active.into_iter().map(|b| b.id).collect();
        }
        if ids.is_empty() {
            return Ok(None);
        }
        let count = self.api.concurrent_viewers(&ids).await.map_err(|e| self.map_err(e))?;
        if count.is_some() {
            self.api.clear_missing_viewers_recheck();
            return Ok(count);
        }
        if used_cache && self.api.take_missing_viewers_recheck() {
            let active = self.api.active_broadcasts().await.map_err(|e| self.map_err(e))?;
            let fresh_ids: Vec<String> = active.into_iter().map(|b| b.id).collect();
            if fresh_ids.is_empty() {
                return Ok(None);
            }
            if fresh_ids != ids {
                let res = self.api.concurrent_viewers(&fresh_ids).await.map_err(|e| self.map_err(e))?;
                if res.is_some() {
                    self.api.clear_missing_viewers_recheck();
                }
                return Ok(res);
            }
        }
        Ok(None)
    }

    async fn poll_capability(&self) -> Option<PlatformPollCapability> {
        let is_live = self.live.read().unwrap_or_else(|p| p.into_inner()).is_some();
        Some(PlatformPollCapability {
            platform: Platform::Youtube,
            mode: if is_live { PollCapabilityMode::Native } else { PollCapabilityMode::ChatFallback },
            reason: if is_live { None } else { Some("not_live".into()) },
            limits: YOUTUBE_POLL_LIMITS,
        })
    }

    async fn create_native_poll(&self, spec: &NativePollSpec) -> Result<NativePollTally> {
        if let Some(retry_after) = self.api.quota_retry_after() {
            return Err(ConnectorError::RateLimited { platform: Platform::Youtube, retry_after });
        }
        if spec.question.chars().count() > YOUTUBE_POLL_LIMITS.question_chars {
            return Err(ConnectorError::Refused {
                platform: Platform::Youtube,
                message: format!("Question exceeds YouTube {} character limit.", YOUTUBE_POLL_LIMITS.question_chars),
            });
        }
        if spec.options.len() < YOUTUBE_POLL_LIMITS.min_options
            || spec.options.len() > YOUTUBE_POLL_LIMITS.max_options
            || spec.options.iter().any(|o| o.chars().count() > YOUTUBE_POLL_LIMITS.option_chars)
        {
            return Err(ConnectorError::Refused {
                platform: Platform::Youtube,
                message: format!(
                    "YouTube polls allow {} to {} answers of at most {} characters.",
                    YOUTUBE_POLL_LIMITS.min_options, YOUTUBE_POLL_LIMITS.max_options, YOUTUBE_POLL_LIMITS.option_chars
                ),
            });
        }
        let chat = self.chat()?;
        self.api.create_poll(&chat, &spec.question, &spec.options).await.map_err(|e| self.map_err(e))
    }

    async fn get_native_poll(&self, external_id: &str) -> Result<Option<NativePollTally>> {
        Ok(self.api.latest_poll(external_id))
    }

    async fn end_native_poll(&self, external_id: &str) -> Result<Option<NativePollTally>> {
        if let Some(retry_after) = self.api.quota_retry_after() {
            return Err(ConnectorError::RateLimited { platform: Platform::Youtube, retry_after });
        }
        self.api.close_poll(external_id).await.map_err(|e| self.map_err(e))
    }
}
