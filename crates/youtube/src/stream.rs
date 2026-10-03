//! Finding the live chat and holding the `streamList` stream open while live.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use ferrocast_connector::EventSink;
use ferrocast_core::event::{ConnectorStatus, Event, EventKind, Millis, Origin, Platform};
use tokio::sync::Notify;
use tonic::Code;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::api::Api;
use crate::proto::LiveChatMessageListRequest;
use crate::proto::v3_data_live_chat_message_service_client::V3DataLiveChatMessageServiceClient;
use crate::{YoutubeError, map, now_ms};

/// How often to look for a live broadcast when nothing else says one started.
const LOOK_EVERY: Duration = Duration::from_secs(600);
/// After a wake (OBS started streaming, another platform went live), YouTube can take a
/// few seconds to mark the broadcast active: look again this often, this many times.
const EAGER_EVERY: Duration = Duration::from_secs(15);
const EAGER_LOOKS: u32 = 5;

/// The live chat id the connector sends to, shared with the stream task.
pub type LiveChat = Arc<RwLock<Option<String>>>;

enum End {
    ChatEnded,
    Lost(String),
    SignInNeeded,
    Quota,
}

async fn emit(sink: &EventSink, kind: EventKind) -> bool {
    sink.send(Event::live(Platform::Youtube, None, Millis(now_ms()), kind)).await.is_ok()
}

/// Runs until the sink closes or sign-in is needed. `wake` asks for an immediate look
/// (another platform just went live).
pub async fn run(api: Arc<Api>, sink: EventSink, wake: Arc<Notify>, live: LiveChat) {
    let mut attempt = 0u32;
    let mut eager = 0u32;
    if !emit(&sink, EventKind::Connector(ConnectorStatus::Connected)).await {
        return;
    }
    loop {
        match api.active_live_chat().await {
            Ok(Some(chat)) => {
                attempt = 0;
                *live.write().unwrap_or_else(|p| p.into_inner()) = Some(chat.clone());
                emit(&sink, EventKind::StreamOnline { title: None }).await;
                let end = follow(&api, &chat, &sink).await;
                *live.write().unwrap_or_else(|p| p.into_inner()) = None;
                match end {
                    End::ChatEnded => {
                        emit(&sink, EventKind::StreamOffline).await;
                    }
                    End::SignInNeeded => {
                        emit(&sink, EventKind::Connector(ConnectorStatus::SignInNeeded)).await;
                        return;
                    }
                    End::Quota => quota_pause(&sink).await,
                    End::Lost(reason) => {
                        attempt += 1;
                        tracing::warn!(%reason, attempt, "YouTube chat stream lost");
                        emit(&sink, EventKind::Connector(ConnectorStatus::Reconnecting { attempt })).await;
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                }
            }
            Ok(None) => {
                let wait = if eager > 0 {
                    eager -= 1;
                    EAGER_EVERY
                } else {
                    LOOK_EVERY
                };
                tokio::select! {
                    _ = wake.notified() => eager = EAGER_LOOKS,
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            Err(YoutubeError::SignInNeeded) => {
                emit(&sink, EventKind::Connector(ConnectorStatus::SignInNeeded)).await;
                return;
            }
            Err(YoutubeError::QuotaExceeded) => quota_pause(&sink).await,
            Err(e) => {
                attempt += 1;
                emit(&sink, EventKind::Connector(ConnectorStatus::Reconnecting { attempt })).await;
                tracing::warn!(error = %e, "couldn't check YouTube for a live stream");
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
        if sink.is_closed() {
            return;
        }
        emit(&sink, EventKind::Connector(ConnectorStatus::Connected)).await;
    }
}

async fn quota_pause(sink: &EventSink) {
    emit(sink, EventKind::Connector(ConnectorStatus::Disconnected { reason: YoutubeError::QuotaExceeded.to_string() }))
        .await;
    tokio::time::sleep(Duration::from_secs(3600)).await;
}

pub fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_secs(1u64 << attempt.saturating_sub(1).min(6)).min(Duration::from_secs(60));
    base + Duration::from_millis(u64::try_from(now_ms().rem_euclid(500)).unwrap_or(0))
}

async fn channel(url: &str) -> Result<Channel, String> {
    let mut ep =
        Endpoint::from_shared(url.to_string()).map_err(|e| e.to_string())?.connect_timeout(Duration::from_secs(10));
    if url.starts_with("https://") {
        ep = ep.tls_config(ClientTlsConfig::new().with_webpki_roots()).map_err(|e| e.to_string())?;
    }
    ep.connect().await.map_err(|e| e.to_string())
}

/// Holds the stream, resuming from the last page token after drops, until the chat ends.
async fn follow(api: &Arc<Api>, chat: &str, sink: &EventSink) -> End {
    let mut page_token: Option<String> = None;
    let mut first_batch = true;
    let mut drops = 0u32;
    loop {
        let ch = match channel(&api.ep.grpc).await {
            Ok(c) => c,
            Err(e) => return End::Lost(e),
        };
        let mut client = V3DataLiveChatMessageServiceClient::new(ch);
        let token = match api.access().await {
            Ok(t) => t,
            Err(YoutubeError::SignInNeeded) => return End::SignInNeeded,
            Err(e) => return End::Lost(e.to_string()),
        };
        let mut req = tonic::Request::new(LiveChatMessageListRequest {
            live_chat_id: Some(chat.to_string()),
            hl: None,
            profile_image_size: Some(64),
            max_results: None,
            page_token: page_token.clone(),
            part: vec!["id".into(), "snippet".into(), "authorDetails".into()],
        });
        if let Ok(v) = format!("Bearer {token}").parse() {
            req.metadata_mut().insert("authorization", v);
        }
        let mut stream = match client.stream_list(req).await {
            Ok(r) => r.into_inner(),
            Err(status) => match status.code() {
                Code::Unauthenticated => {
                    if api.refresh_after_401(&token).await.is_err() {
                        return End::SignInNeeded;
                    }
                    continue;
                }
                Code::ResourceExhausted => return End::Quota,
                Code::NotFound | Code::FailedPrecondition => return End::ChatEnded,
                _ => return End::Lost(status.message().to_string()),
            },
        };
        loop {
            match stream.message().await {
                Ok(Some(resp)) => {
                    drops = 0;
                    if let Some(next) = resp.next_page_token.clone().filter(|t| !t.is_empty()) {
                        page_token = Some(next);
                    }
                    for item in &resp.items {
                        if let Some(mut e) = map::map(item, Millis(now_ms())) {
                            // The first answer replays recent chat; it's history, not new.
                            if first_batch {
                                e.origin = Origin::Backfill;
                            }
                            if sink.send(e).await.is_err() {
                                return End::Lost("the engine stopped".into());
                            }
                        }
                    }
                    first_batch = false;
                    if resp.offline_at.as_deref().is_some_and(|s| !s.is_empty()) {
                        return End::ChatEnded;
                    }
                }
                Ok(None) => break,
                Err(status) if status.code() == Code::ResourceExhausted => return End::Quota,
                Err(status) => {
                    tracing::debug!(message = %status.message(), "YouTube stream dropped");
                    break;
                }
            }
        }
        // The server closed the stream (normal every so often): resume from the token.
        drops += 1;
        if drops > 5 {
            return End::Lost("the chat stream keeps dropping".into());
        }
        tokio::time::sleep(backoff(drops)).await;
    }
}
