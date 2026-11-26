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
    /// The day's units are gone.
    Quota,
    /// A burst limit: try again in seconds.
    RateLimited(Duration),
}

/// How long the loop waits after a limit (None: as it should; tests shorten them).
#[derive(Debug, Clone, Copy, Default)]
pub struct Waits {
    /// After a burst limit.
    pub rate: Option<Duration>,
    /// After the day's quota is used up.
    pub quota: Option<Duration>,
}

/// The wait before the `attempt`th try after a burst limit: 5 s, doubling to a minute, never less
/// than YouTube asked for.
pub fn rate_backoff(attempt: u32, asked: Duration) -> Duration {
    let own = Duration::from_secs(5u64 << attempt.saturating_sub(1).min(4)).min(Duration::from_secs(60));
    own.max(asked).min(Duration::from_secs(60))
}

async fn emit(sink: &EventSink, kind: EventKind) -> bool {
    sink.send(Event::live(Platform::Youtube, None, Millis(now_ms()), kind)).await.is_ok()
}

/// Runs until the sink closes or sign-in is needed. `wake` asks for an immediate look
/// (another platform just went live).
pub async fn run(api: Arc<Api>, sink: EventSink, wake: Arc<Notify>, live: LiveChat) {
    run_with(api, sink, wake, live, Waits::default()).await;
}

/// [`run`] with its waits given.
pub async fn run_with(api: Arc<Api>, sink: EventSink, wake: Arc<Notify>, live: LiveChat, waits: Waits) {
    let mut attempt = 0u32;
    let mut eager = 0u32;
    if !emit(&sink, EventKind::Connector(ConnectorStatus::Connected)).await {
        return;
    }
    // Connected is said when it starts and again only after a call worked following a failure:
    // never between failed attempts (it flashed green, then red, every time it woke up).
    let mut connected = true;
    loop {
        match api.active_broadcasts().await {
            Ok(broadcasts) => {
                if let Some(chat) = broadcasts.iter().find_map(|b| b.live_chat_id.clone()) {
                    attempt = 0;
                    if !connected {
                        emit(&sink, EventKind::Connector(ConnectorStatus::Connected)).await;
                        connected = true;
                    }
                    *live.write().unwrap_or_else(|p| p.into_inner()) = Some(chat.clone());
                    emit(&sink, EventKind::StreamOnline { title: None }).await;
                    let end = follow(&api, &chat, &sink).await;
                    *live.write().unwrap_or_else(|p| p.into_inner()) = None;
                    match end {
                        End::ChatEnded => {
                            api.clear_active_broadcasts();
                            emit(&sink, EventKind::StreamOffline).await;
                        }
                        End::SignInNeeded => {
                            emit(&sink, EventKind::Connector(ConnectorStatus::SignInNeeded)).await;
                            return;
                        }
                        End::Quota => {
                            connected = false;
                            quota_pause(&api, &sink, waits).await;
                        }
                        End::RateLimited(asked) => {
                            connected = false;
                            attempt += 1;
                            emit(&sink, EventKind::Connector(ConnectorStatus::Reconnecting { attempt })).await;
                            tokio::time::sleep(waits.rate.unwrap_or_else(|| rate_backoff(attempt, asked))).await;
                        }
                        End::Lost(reason) => {
                            connected = false;
                            attempt += 1;
                            tracing::warn!(%reason, attempt, "YouTube chat stream lost");
                            emit(&sink, EventKind::Connector(ConnectorStatus::Reconnecting { attempt })).await;
                            tokio::time::sleep(backoff(attempt)).await;
                        }
                    }
                } else {
                    if !connected {
                        emit(&sink, EventKind::Connector(ConnectorStatus::Connected)).await;
                        connected = true;
                    }
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
            }
            Err(YoutubeError::SignInNeeded) => {
                emit(&sink, EventKind::Connector(ConnectorStatus::SignInNeeded)).await;
                return;
            }
            Err(YoutubeError::QuotaExceeded) => {
                connected = false;
                quota_pause(&api, &sink, waits).await;
            }
            Err(YoutubeError::RateLimited(asked)) => {
                connected = false;
                attempt += 1;
                tracing::warn!(attempt, "YouTube asked for a moment's pause");
                emit(&sink, EventKind::Connector(ConnectorStatus::Reconnecting { attempt })).await;
                tokio::time::sleep(waits.rate.unwrap_or_else(|| rate_backoff(attempt, asked))).await;
            }
            Err(e) => {
                connected = false;
                attempt += 1;
                emit(&sink, EventKind::Connector(ConnectorStatus::Reconnecting { attempt })).await;
                tracing::warn!(error = %e, "couldn't check YouTube for a live stream");
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
        if sink.is_closed() {
            return;
        }
    }
}

/// How often the chat connection is pinged, and how long an answer may take before it's
/// taken as dead.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(20);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// The day's units are used up: said, then nothing is asked until they are back (midnight in
/// Pacific time, and a few minutes of spread), not every hour for nothing.
async fn quota_pause(api: &Api, sink: &EventSink, waits: Waits) {
    emit(sink, EventKind::Connector(ConnectorStatus::Disconnected { reason: YoutubeError::QuotaExceeded.to_string() }))
        .await;
    let wait = waits.quota.unwrap_or_else(|| crate::quota_resets_in(now_ms(), crate::quota_jitter_s()));
    api.note_quota_exceeded(wait);
    tokio::time::sleep(wait).await;
    api.clear_quota_pause();
}

/// A gRPC "resource exhausted" is the day's quota when it says so, else a burst.
fn exhausted(status: &tonic::Status) -> End {
    let m = status.message().to_ascii_lowercase();
    if m.contains("quota") || m.contains("daily") { End::Quota } else { End::RateLimited(Duration::from_secs(5)) }
}

pub fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_secs(1u64 << attempt.saturating_sub(1).min(6)).min(Duration::from_secs(60));
    base + Duration::from_millis(u64::try_from(now_ms().rem_euclid(500)).unwrap_or(0))
}

async fn channel(url: &str) -> Result<Channel, String> {
    channel_with(url, KEEPALIVE_EVERY, KEEPALIVE_TIMEOUT).await
}

/// The chat channel, pinging after `every` of silence and giving up on a ping unanswered for
/// `timeout`.
async fn channel_with(url: &str, every: Duration, timeout: Duration) -> Result<Channel, String> {
    // HTTP/2 pings: a connection that died silently (Wi-Fi blip, a router dropping the idle
    // mapping) ends the stream within ~30 s and the chat reconnects; without them it hung
    // forever, chat quietly gone for the rest of the stream (the M10 scan, M10-02). The
    // client pings only after `every` without anything from the server, so a chat with traffic
    // sends none.
    let mut ep = Endpoint::from_shared(url.to_string())
        .map_err(|e| e.to_string())?
        .connect_timeout(Duration::from_secs(10))
        .http2_keep_alive_interval(every)
        .keep_alive_timeout(timeout)
        .keep_alive_while_idle(true)
        .tcp_keepalive(Some(every));
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
    // The stream refused a token this many times in a row (reset when it delivers).
    let mut refused = 0u32;
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
        let opened_at = now_ms();
        let mut stream = match client.stream_list(req).await {
            Ok(r) => r.into_inner(),
            Err(status) => match status.code() {
                Code::Unauthenticated => {
                    // Only Google refusing the sign-in for good ends the chat. A refresh that
                    // failed for any other reason (the token endpoint or the network having a bad
                    // moment, the Keychain refusing the save) is tried again with the usual wait;
                    // and a fresh token that is refused again is a sign-in that no longer works,
                    // not a reason to ask over and over.
                    refused += 1;
                    if refused > 2 {
                        return End::SignInNeeded;
                    }
                    match api.refresh_after_401(&token).await {
                        // A moment before asking again: Google's edge can take a beat to accept a
                        // token it just issued, and three tries in a few ms would end the chat.
                        Ok(_) => {
                            tokio::time::sleep(Duration::from_millis(600 * u64::from(refused))).await;
                            continue;
                        }
                        Err(YoutubeError::SignInNeeded) => return End::SignInNeeded,
                        Err(e) => return End::Lost(e.to_string()),
                    }
                }
                Code::ResourceExhausted => return exhausted(&status),
                Code::NotFound | Code::FailedPrecondition => return End::ChatEnded,
                _ => return End::Lost(status.message().to_string()),
            },
        };
        loop {
            match stream.message().await {
                Ok(Some(resp)) => {
                    drops = 0;
                    refused = 0;
                    if let Some(next) = resp.next_page_token.clone().filter(|t| !t.is_empty()) {
                        page_token = Some(next);
                    }
                    for item in &resp.items {
                        if let Some(mut e) = map::map(item, Millis(now_ms())) {
                            // The first answer replays recent chat; it's history, not new
                            // (a line published after the stream opened is new even there).
                            let published = item
                                .snippet
                                .as_ref()
                                .and_then(|s| s.published_at.as_deref())
                                .and_then(map::published_ms);
                            if first_batch && published.is_none_or(|p| p < opened_at - crate::feed::BACKFILL_SLACK_MS) {
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
                Err(status) if status.code() == Code::ResourceExhausted => return exhausted(&status),
                // The token expiring while the stream is open (often at the hour): the refusal comes
                // on the stream, not at its start. Refresh and reopen, as at the start.
                Err(status) if status.code() == Code::Unauthenticated => {
                    refused += 1;
                    if refused > 2 {
                        return End::SignInNeeded;
                    }
                    match api.refresh_after_401(&token).await {
                        Ok(_) => break,
                        Err(YoutubeError::SignInNeeded) => return End::SignInNeeded,
                        Err(e) => return End::Lost(e.to_string()),
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A server that accepts the connection and then never says a word (a dead Wi-Fi link the
    /// client wasn't told about): the call ends with an error within the keepalive time, so
    /// `follow` reconnects, instead of waiting for ever.
    #[tokio::test]
    async fn a_connection_that_goes_silent_ends_the_call() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = l.accept().await {
                held.push(s); // kept open, never read from or written to
            }
        });
        let call = |ch: Channel| async move {
            let mut client = V3DataLiveChatMessageServiceClient::new(ch);
            client
                .stream_list(tonic::Request::new(LiveChatMessageListRequest {
                    live_chat_id: Some("chat".into()),
                    ..Default::default()
                }))
                .await
                .map(|_| ())
        };
        let ch = channel_with(&url, Duration::from_millis(200), Duration::from_millis(200)).await.unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(5), call(ch)).await;
        let status = ended.expect("a silent connection must end the call").unwrap_err();
        assert_eq!(status.code(), Code::Unavailable, "{status:?}");
        // Without pings (the old channel) the same server leaves the call waiting.
        let plain = Endpoint::from_shared(url).unwrap().connect().await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), call(plain)).await.is_err(), "no pings: it hangs");
    }
}
