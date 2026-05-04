//! The EventSub WebSocket session (R-18, R-21).
//!
//! - The watchdog uses the keepalive interval Twitch gives in the welcome, so a socket that
//!   died while the PC slept is noticed within seconds, not after a restart.
//! - `session_reconnect` is a handover: we connect to the new URL, keep reading the old
//!   socket until the new one says welcome, then drop the old one. Subscriptions carry
//!   over, so nothing is re-created (re-subscribing gets 409s).
//! - Any other loss reconnects from scratch with exponential backoff and jitter, and
//!   subscribes again.

use std::time::Duration;

use ferrocast_connector::EventSink;
use ferrocast_core::event::{ConnectorStatus, Event, EventKind, Millis, Platform};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::helix::Helix;
use crate::map::{self, SUBSCRIPTIONS};
use crate::{TwitchError, now_ms};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Twitch closes a socket that hasn't subscribed within 10 s of the welcome.
const WELCOME_WAIT: Duration = Duration::from_secs(10);
/// Extra slack on top of Twitch's keepalive before we call a socket dead.
const KEEPALIVE_SLACK: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A session that lasted this long resets the backoff.
const STABLE_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Welcome {
    pub session_id: String,
    pub keepalive: Duration,
}

enum End {
    /// Twitch moved us: carry on at this URL without re-subscribing.
    Moved(Box<Ws>, Welcome),
    Lost(String),
    SignInNeeded,
}

/// Runs until the sink closes or sign-in is needed. Emits connector status events.
pub async fn run(helix: Helix, broadcaster: String, url: String, sink: EventSink) {
    let mut attempt: u32 = 0;
    loop {
        let started = tokio::time::Instant::now();
        let outcome = match connect(&url).await {
            Ok((ws, welcome)) => match subscribe_all(&helix, &broadcaster, &welcome.session_id, &sink).await {
                Ok(()) => {
                    status(&sink, ConnectorStatus::Connected).await;
                    follow(ws, welcome, &sink).await
                }
                Err(TwitchError::SignInNeeded) => End::SignInNeeded,
                Err(e) => End::Lost(e.to_string()),
            },
            Err(TwitchError::SignInNeeded) => End::SignInNeeded,
            Err(e) => End::Lost(e.to_string()),
        };
        match outcome {
            End::SignInNeeded => {
                status(&sink, ConnectorStatus::SignInNeeded).await;
                return;
            }
            // `follow` completes handovers itself and never returns `Moved`.
            End::Moved(..) => continue,
            End::Lost(reason) => {
                if sink.is_closed() {
                    return;
                }
                if started.elapsed() >= STABLE_AFTER {
                    attempt = 0;
                }
                attempt = attempt.saturating_add(1);
                tracing::warn!(attempt, %reason, "Twitch connection lost; reconnecting");
                status(&sink, ConnectorStatus::Reconnecting { attempt }).await;
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
}

/// 1 s, 2 s, 4 s … up to a minute, plus up to half a second of jitter so many apps
/// reconnecting after a Twitch blip don't arrive in lockstep.
pub fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_secs(1u64 << attempt.saturating_sub(1).min(6)).min(BACKOFF_MAX);
    let jitter = u64::try_from(now_ms().rem_euclid(500)).unwrap_or(0);
    base + Duration::from_millis(jitter)
}

async fn status(sink: &EventSink, s: ConnectorStatus) {
    let _ = sink.send(Event::live(Platform::Twitch, None, Millis(now_ms()), EventKind::Connector(s))).await;
}

/// Connects and waits for the welcome.
pub async fn connect(url: &str) -> Result<(Ws, Welcome), TwitchError> {
    let (mut ws, _) = tokio::time::timeout(WELCOME_WAIT, tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| TwitchError::Network("timed out connecting to EventSub".into()))?
        .map_err(TwitchError::network)?;
    let deadline = tokio::time::Instant::now() + WELCOME_WAIT;
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| TwitchError::Protocol("no welcome from EventSub".into()))?
            .ok_or_else(|| TwitchError::Network("EventSub closed before the welcome".into()))?
            .map_err(TwitchError::network)?;
        let Message::Text(text) = msg else { continue };
        let v: Value = serde_json::from_str(&text).map_err(|e| TwitchError::Protocol(e.to_string()))?;
        if v.pointer("/metadata/message_type").and_then(Value::as_str) == Some("session_welcome") {
            let session_id = v
                .pointer("/payload/session/id")
                .and_then(Value::as_str)
                .ok_or_else(|| TwitchError::Protocol("welcome without a session id".into()))?
                .to_string();
            let secs = v.pointer("/payload/session/keepalive_timeout_seconds").and_then(Value::as_u64).unwrap_or(10);
            return Ok((ws, Welcome { session_id, keepalive: Duration::from_secs(secs.clamp(1, 600)) }));
        }
    }
}

async fn subscribe_all(helix: &Helix, broadcaster: &str, session: &str, sink: &EventSink) -> Result<(), TwitchError> {
    let mut missing = Vec::new();
    for sub in SUBSCRIPTIONS {
        match helix.subscribe(session, sub.kind, sub.version, sub.condition_json(broadcaster)).await {
            Ok(()) => {}
            Err(TwitchError::MissingScope(_)) => missing.push(sub.kind.to_string()),
            // A revoked or not-yet-allowed type is a per-subscription problem, not fatal.
            Err(TwitchError::Refused(m)) => {
                tracing::warn!(kind = sub.kind, %m, "subscription refused")
            }
            Err(e) => return Err(e),
        }
    }
    if !missing.is_empty() {
        status(sink, ConnectorStatus::ScopesMissing { scopes: missing }).await;
    }
    Ok(())
}

/// Reads one session until it ends. Handles keepalives, notifications and handovers.
async fn follow(mut ws: Ws, mut welcome: Welcome, sink: &EventSink) -> End {
    loop {
        let wait = welcome.keepalive + KEEPALIVE_SLACK.min(welcome.keepalive);
        let msg = match tokio::time::timeout(wait, ws.next()).await {
            Err(_) => {
                return End::Lost("no keepalive from EventSub (sleep or network change?)".into());
            }
            Ok(None) => return End::Lost("EventSub closed the connection".into()),
            Ok(Some(Err(e))) => return End::Lost(e.to_string()),
            Ok(Some(Ok(m))) => m,
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(frame) => {
                // 4003/4004 etc. mean we did something wrong; anything else is a blip.
                return End::Lost(format!(
                    "EventSub closed: {}",
                    frame.map(|f| f.code.to_string()).unwrap_or_default()
                ));
            }
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match v.pointer("/metadata/message_type").and_then(Value::as_str) {
            Some("session_keepalive") => {}
            Some("notification") => {
                if !deliver(&v, sink).await {
                    return End::Lost("the engine stopped".into());
                }
            }
            Some("session_reconnect") => {
                let Some(url) = v.pointer("/payload/session/reconnect_url").and_then(Value::as_str) else {
                    return End::Lost("reconnect without a URL".into());
                };
                match handover(ws, url, sink).await {
                    End::Moved(new_ws, new_welcome) => {
                        ws = *new_ws;
                        welcome = new_welcome;
                        tracing::info!("EventSub handover complete");
                    }
                    other => return other,
                }
            }
            Some("revocation") => {
                let kind = v.pointer("/payload/subscription/type").and_then(Value::as_str).unwrap_or("?");
                let reason = v.pointer("/payload/subscription/status").and_then(Value::as_str).unwrap_or("?");
                tracing::warn!(kind, reason, "Twitch revoked a subscription");
                if reason == "authorization_revoked" {
                    return End::SignInNeeded;
                }
            }
            _ => {}
        }
    }
}

/// Connects to the new URL while still reading the old socket, so events sent to the old
/// one during the switch aren't lost.
async fn handover(mut old: Ws, url: &str, sink: &EventSink) -> End {
    let connecting = connect(url);
    tokio::pin!(connecting);
    loop {
        tokio::select! {
            res = &mut connecting => {
                return match res {
                    Ok((ws, welcome)) => End::Moved(Box::new(ws), welcome),
                    Err(e) => End::Lost(format!("handover failed: {e}")),
                };
            }
            msg = old.next() => {
                if let Some(Ok(Message::Text(t))) = msg
                    && let Ok(v) = serde_json::from_str::<Value>(&t)
                    && v.pointer("/metadata/message_type").and_then(Value::as_str) == Some("notification")
                {
                    deliver(&v, sink).await;
                }
            }
        }
    }
}

/// Maps and sends one notification. `false` if the engine is gone.
async fn deliver(v: &Value, sink: &EventSink) -> bool {
    let kind = v.pointer("/metadata/subscription_type").and_then(Value::as_str).unwrap_or("");
    let id = v.pointer("/metadata/message_id").and_then(Value::as_str).unwrap_or("");
    let Some(event) = v.pointer("/payload/event") else {
        return true;
    };
    sink.send(map::map(kind, id, event, Millis(now_ms()))).await.is_ok()
}
