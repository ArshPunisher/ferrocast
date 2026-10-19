//! One `liveChatMessages.streamList` connection as plain batches of mapped events, for
//! callers that hold their own tokens (the relay's cloud bot, Pro F1). The desktop's own
//! reader is `stream::run`; both use the same gRPC call and the same mapping.
//!
//! Each opened stream is one call to YouTube (5 quota units, however long it stays open).

use std::time::Duration;

use ferrocast_core::event::{Event, Millis, Origin};
use tonic::Code;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::proto::v3_data_live_chat_message_service_client::V3DataLiveChatMessageServiceClient;
use crate::proto::{LiveChatMessageListRequest, LiveChatMessageListResponse};
use crate::{map, now_ms};

/// Why a stream couldn't open or stopped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeedError {
    #[error("YouTube needs a new sign-in")]
    SignInNeeded,
    #[error("YouTube's daily limit is used up")]
    Quota,
    #[error("the live chat ended")]
    ChatEnded,
    #[error("YouTube refused: {0}")]
    Refused(String),
    #[error("couldn't reach YouTube: {0}")]
    Lost(String),
}

/// One answer on the stream: the mapped lines, where to resume, and whether the chat ended.
#[derive(Debug, Clone, Default)]
pub struct Batch {
    pub events: Vec<Event>,
    pub next_page_token: Option<String>,
    pub offline: bool,
}

/// An open `streamList`.
#[derive(Debug)]
pub struct Feed {
    inner: tonic::Streaming<LiveChatMessageListResponse>,
    first: bool,
}

/// HTTP/2 pings every 30 s, a connection that doesn't answer one in 10 s is dropped (also
/// while no call is open), so a silently dead stream ends instead of hanging.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// The gRPC endpoint with keepalive (`interval`, `timeout`).
pub(crate) fn endpoint(url: &str, (interval, timeout): (Duration, Duration)) -> Result<Endpoint, String> {
    Ok(Endpoint::from_shared(url.to_string())
        .map_err(|e| e.to_string())?
        .connect_timeout(Duration::from_secs(10))
        .http2_keep_alive_interval(interval)
        .keep_alive_timeout(timeout)
        .keep_alive_while_idle(true))
}

async fn channel(url: &str, keepalive: (Duration, Duration)) -> Result<Channel, String> {
    let mut ep = endpoint(url, keepalive)?;
    if url.starts_with("https://") {
        ep = ep.tls_config(ClientTlsConfig::new().with_webpki_roots()).map_err(|e| e.to_string())?;
    }
    ep.connect().await.map_err(|e| e.to_string())
}

fn status_err(status: &tonic::Status) -> FeedError {
    match status.code() {
        Code::Unauthenticated => FeedError::SignInNeeded,
        Code::ResourceExhausted => FeedError::Quota,
        Code::NotFound | Code::FailedPrecondition => FeedError::ChatEnded,
        Code::PermissionDenied | Code::InvalidArgument => {
            FeedError::Refused(status.message().chars().take(200).collect())
        }
        _ => FeedError::Lost(status.message().chars().take(200).collect()),
    }
}

/// Opens the stream for `live_chat_id` with `access_token`, resuming at `page_token` when
/// given. Without a page token, the first answer replays recent chat: those lines come
/// marked [`Origin::Backfill`].
pub async fn open(
    grpc_url: &str,
    access_token: &str,
    live_chat_id: &str,
    page_token: Option<String>,
) -> Result<Feed, FeedError> {
    open_tuned(grpc_url, access_token, live_chat_id, page_token, (KEEPALIVE_INTERVAL, KEEPALIVE_TIMEOUT)).await
}

/// [`open`] with its own keepalive (tests use short ones).
pub(crate) async fn open_tuned(
    grpc_url: &str,
    access_token: &str,
    live_chat_id: &str,
    page_token: Option<String>,
    keepalive: (Duration, Duration),
) -> Result<Feed, FeedError> {
    let ch = channel(grpc_url, keepalive).await.map_err(FeedError::Lost)?;
    let mut client = V3DataLiveChatMessageServiceClient::new(ch);
    let first = page_token.is_none();
    let mut req = tonic::Request::new(LiveChatMessageListRequest {
        live_chat_id: Some(live_chat_id.to_string()),
        hl: None,
        profile_image_size: Some(64),
        max_results: None,
        page_token,
        part: vec!["id".into(), "snippet".into(), "authorDetails".into()],
    });
    let auth = format!("Bearer {access_token}").parse().map_err(|_| FeedError::SignInNeeded)?;
    req.metadata_mut().insert("authorization", auth);
    match client.stream_list(req).await {
        Ok(r) => Ok(Feed { inner: r.into_inner(), first }),
        Err(status) => Err(status_err(&status)),
    }
}

impl Feed {
    /// The next answer, or `None` when YouTube closed the stream (normal now and then:
    /// open again from the last page token).
    pub async fn next(&mut self) -> Result<Option<Batch>, FeedError> {
        match self.inner.message().await {
            Ok(Some(resp)) => {
                let backfill = std::mem::replace(&mut self.first, false);
                let events = resp
                    .items
                    .iter()
                    .filter_map(|item| map::map(item, Millis(now_ms())))
                    .map(|mut e| {
                        if backfill {
                            e.origin = Origin::Backfill;
                        }
                        e
                    })
                    .collect();
                Ok(Some(Batch {
                    events,
                    next_page_token: resp.next_page_token.clone().filter(|t| !t.is_empty()),
                    offline: resp.offline_at.as_deref().is_some_and(|s| !s.is_empty()),
                }))
            }
            Ok(None) => Ok(None),
            Err(status) => Err(status_err(&status)),
        }
    }
}
