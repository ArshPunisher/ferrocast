//! Helix requests: one place that adds auth, retries once after a 401 with a refreshed
//! token, and turns 403/429 into errors the dashboard can explain (R-19).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrocast_connector::NativePollTally;
use ferrocast_core::poll::PollLimits;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Auth, TwitchError};

pub const TWITCH_POLL_LIMITS: PollLimits = PollLimits {
    min_options: 2,
    max_options: 5,
    min_duration_s: 15,
    max_duration_s: 1800,
    supports_no_timer: true,
    question_chars: 60,
    option_chars: 25,
};

pub fn is_affiliate_or_partner(broadcaster_type: &str) -> bool {
    matches!(broadcaster_type, "affiliate" | "partner")
}

#[derive(Debug, Clone)]
pub struct Helix {
    pub(crate) auth: Arc<Auth>,
    /// The ingest server last picked from Twitch's list, and when (see `nearest_ingest`).
    pub(crate) ingest: Arc<Mutex<Option<(Instant, String)>>>,
    /// How long a picked server is reused before the list is read again.
    pub(crate) ingest_keep: Duration,
    /// Cached `GET /users` `broadcaster_type` (`"affiliate"`, `"partner"`, or `""`).
    pub(crate) broadcaster_type: Arc<Mutex<Option<String>>>,
}

/// A 429 that asks for this long or less is waited out and tried again; a longer one is told.
const RETRY_UP_TO: Duration = Duration::from_secs(5);
/// How many times one request is tried again after a 429.
const RATE_RETRIES: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    pub id: String,
    pub login: String,
    pub display_name: String,
    #[serde(default)]
    pub profile_image_url: String,
    #[serde(default)]
    pub broadcaster_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TwitchPoll {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub choices: Vec<TwitchPollChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TwitchPollChoice {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub votes: u64,
}

impl TwitchPoll {
    pub fn into_tally(self) -> NativePollTally {
        NativePollTally {
            external_id: self.id,
            counts: self.choices.into_iter().map(|c| c.votes).collect(),
            ended: !self.status.eq_ignore_ascii_case("ACTIVE"),
        }
    }
}

impl Helix {
    pub fn new(auth: Arc<Auth>) -> Self {
        Helix {
            auth,
            ingest: Arc::new(Mutex::new(None)),
            ingest_keep: crate::ingest::INGEST_KEEP,
            broadcaster_type: Arc::new(Mutex::new(None)),
        }
    }

    /// How long a picked ingest server is reused (the default is an hour; zero reads the list each time).
    #[must_use]
    pub fn with_ingest_cache(mut self, keep: Duration) -> Self {
        self.ingest_keep = keep;
        self
    }

    /// Sends a request; on 401 refreshes the token once and tries again.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, TwitchError> {
        let mut token = self.auth.access().await?;
        let (mut refreshed, mut limited) = (false, 0u32);
        loop {
            let mut req = self
                .auth
                .http
                .request(method.clone(), format!("{}{path}", self.auth.ep.api))
                .query(query)
                .header("Client-Id", &self.auth.client_id)
                .bearer_auth(&token);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let res = req.send().await.map_err(TwitchError::network)?;
            if res.status().as_u16() == 401 && !refreshed {
                // A missing scope is also a 401: refreshing wouldn't help, and each refresh spends
                // Twitch's single-use refresh token (the M10 scan). Only an expired token is
                // refreshed.
                let err = TwitchError::from_response(res).await;
                if matches!(err, TwitchError::MissingScope(_)) {
                    return Err(err);
                }
                token = self.auth.refresh_after_401(&token).await?;
                refreshed = true;
                // A beat before using the new token: the API edge can take a moment to know it.
                tokio::time::sleep(Duration::from_millis(400)).await;
                continue;
            }
            if res.status().as_u16() == 401 && refreshed {
                // Refused again with a token just issued. A missing scope says so (the permission
                // to fix); otherwise the first times it is more likely the API edge catching up than
                // the streamer being signed out, so it is said as a network trouble and the connector
                // retries with its backoff; refused three times in a row, the sign-in no longer works.
                let err = TwitchError::from_response(res).await;
                if matches!(err, TwitchError::MissingScope(_)) {
                    return Err(err);
                }
                if self.auth.note_fresh_refused() >= 3 {
                    return Err(TwitchError::SignInNeeded);
                }
                return Err(TwitchError::Network("Twitch didn't accept a fresh sign-in yet".into()));
            }
            if res.status().as_u16() == 429 {
                // The request wasn't done: waiting until the budget is back (Ratelimit-Reset) and
                // sending it again is safe for any method, and a burst during a hype train is
                // seconds. A longer wait is the person's to be told.
                let err = TwitchError::from_response(res).await;
                match err {
                    TwitchError::RateLimited(wait) if wait <= RETRY_UP_TO && limited < RATE_RETRIES => {
                        limited += 1;
                        tracing::debug!(?wait, path, "Twitch asked for a pause; trying again");
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    e => return Err(e),
                }
            }
            if !res.status().is_success() {
                return Err(TwitchError::from_response(res).await);
            }
            self.auth.note_accepted();
            if res.status().as_u16() == 204 {
                return Ok(Value::Null);
            }
            return res.json().await.map_err(TwitchError::network);
        }
    }

    pub async fn me(&self) -> Result<User, TwitchError> {
        let v = self.call(Method::GET, "/users", &[], None).await?;
        let first = v
            .get("data")
            .and_then(|d| d.get(0))
            .cloned()
            .ok_or_else(|| TwitchError::Protocol("no user returned".into()))?;
        let user: User = serde_json::from_value(first).map_err(|e| TwitchError::Protocol(e.to_string()))?;
        *self.broadcaster_type.lock().unwrap_or_else(|p| p.into_inner()) = Some(user.broadcaster_type.clone());
        Ok(user)
    }

    /// Returns cached `broadcaster_type` (`"affiliate"`, `"partner"`, or `""`), fetching `/users` once if unset.
    pub async fn broadcaster_type(&self) -> Result<String, TwitchError> {
        if let Some(bt) = self.broadcaster_type.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return Ok(bt);
        }
        Ok(self.me().await?.broadcaster_type)
    }

    pub fn mark_non_affiliate(&self) {
        *self.broadcaster_type.lock().unwrap_or_else(|p| p.into_inner()) = Some(String::new());
    }

    /// Starts a Twitch Helix poll (`duration` clamped to 15..=1800s; No-timer uses 1800s and ends early).
    pub async fn create_poll(
        &self,
        broadcaster: &str,
        title: &str,
        choices: &[String],
        duration_s: u32,
    ) -> Result<TwitchPoll, TwitchError> {
        let choice_objs: Vec<Value> = choices.iter().map(|c| json!({ "title": c })).collect();
        let body = json!({
            "broadcaster_id": broadcaster,
            "title": title,
            "choices": choice_objs,
            "duration": duration_s.clamp(15, 1800),
        });
        match self.call(Method::POST, "/polls", &[], Some(body)).await {
            Ok(v) => parse_poll_first(v),
            Err(TwitchError::Refused(m)) => {
                let lower = m.to_lowercase();
                if lower.contains("affiliate") || lower.contains("partner") {
                    self.mark_non_affiliate();
                }
                Err(TwitchError::Refused(m))
            }
            Err(e) => Err(e),
        }
    }

    /// Reads the current state of one Helix poll by id.
    pub async fn get_poll(&self, broadcaster: &str, id: &str) -> Result<Option<TwitchPoll>, TwitchError> {
        let v = self.call(Method::GET, "/polls", &[("broadcaster_id", broadcaster), ("id", id)], None).await?;
        let Some(first) = v.get("data").and_then(|d| d.get(0)).cloned() else {
            return Ok(None);
        };
        serde_json::from_value(first).map(Some).map_err(|e| TwitchError::Protocol(e.to_string()))
    }

    /// Ends an active Twitch Helix poll (`status: "TERMINATED"`) and returns its final counts.
    pub async fn end_poll(&self, broadcaster: &str, id: &str) -> Result<Option<TwitchPoll>, TwitchError> {
        let body = json!({
            "broadcaster_id": broadcaster,
            "id": id,
            "status": "TERMINATED",
        });
        match self.call(Method::PATCH, "/polls", &[], Some(body)).await {
            Ok(v) => parse_poll_first(v).map(Some),
            Err(TwitchError::Refused(m)) => {
                if let Ok(Some(existing)) = self.get_poll(broadcaster, id).await
                    && !existing.status.eq_ignore_ascii_case("ACTIVE")
                {
                    return Ok(Some(existing));
                }
                Err(TwitchError::Refused(m))
            }
            Err(e) => Err(e),
        }
    }

    /// The game a channel last streamed (`game_name` of Get Channel Information), by login.
    /// `None` for an unknown login or a channel that never set one.
    pub async fn channel_game(&self, login: &str) -> Result<Option<String>, TwitchError> {
        let login = login.trim().trim_start_matches('@').to_lowercase();
        if login.is_empty() || login.len() > 25 || !login.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Ok(None);
        }
        let users = self.call(Method::GET, "/users", &[("login", login.as_str())], None).await?;
        let Some(id) = users.pointer("/data/0/id").and_then(Value::as_str).map(str::to_string) else {
            return Ok(None);
        };
        let ch = self.call(Method::GET, "/channels", &[("broadcaster_id", id.as_str())], None).await?;
        Ok(ch
            .pointer("/data/0/game_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(|g| g.chars().take(80).collect()))
    }

    pub async fn send_chat(
        &self,
        broadcaster: &str,
        sender: &str,
        text: &str,
        reply_to: Option<&str>,
    ) -> Result<(), TwitchError> {
        let mut body = json!({ "broadcaster_id": broadcaster, "sender_id": sender, "message": text });
        if let Some(r) = reply_to {
            body["reply_parent_message_id"] = json!(r);
        }
        let v = self.call(Method::POST, "/chat/messages", &[], Some(body)).await?;
        // Twitch answers 200 even when AutoMod or a chat setting dropped the message.
        let sent = v.pointer("/data/0/is_sent").and_then(Value::as_bool).unwrap_or(true);
        if !sent {
            let reason =
                v.pointer("/data/0/drop_reason/message").and_then(Value::as_str).unwrap_or("dropped by chat settings");
            return Err(TwitchError::Refused(format!("the message wasn't sent: {reason}")));
        }
        Ok(())
    }

    pub async fn delete_message(
        &self,
        broadcaster: &str,
        moderator: &str,
        message_id: &str,
    ) -> Result<(), TwitchError> {
        let q = [("broadcaster_id", broadcaster), ("moderator_id", moderator), ("message_id", message_id)];
        self.call(Method::DELETE, "/moderation/chat", &q, None).await.map(|_| ())
    }

    /// `seconds = None` bans; `Some(n)` times out for n seconds (1 s to 14 days).
    pub async fn ban(
        &self,
        broadcaster: &str,
        moderator: &str,
        user_id: &str,
        seconds: Option<u32>,
        reason: Option<&str>,
    ) -> Result<(), TwitchError> {
        let mut data = json!({ "user_id": user_id });
        if let Some(s) = seconds {
            data["duration"] = json!(s.clamp(1, 1_209_600));
        }
        if let Some(r) = reason.filter(|r| !r.is_empty()) {
            data["reason"] = json!(r.chars().take(500).collect::<String>());
        }
        let q = [("broadcaster_id", broadcaster), ("moderator_id", moderator)];
        self.call(Method::POST, "/moderation/bans", &q, Some(json!({ "data": data }))).await.map(|_| ())
    }

    pub async fn unban(&self, broadcaster: &str, moderator: &str, user_id: &str) -> Result<(), TwitchError> {
        let q = [("broadcaster_id", broadcaster), ("moderator_id", moderator), ("user_id", user_id)];
        self.call(Method::DELETE, "/moderation/bans", &q, None).await.map(|_| ())
    }

    pub async fn chat_settings(&self, broadcaster: &str, moderator: &str, patch: Value) -> Result<(), TwitchError> {
        let q = [("broadcaster_id", broadcaster), ("moderator_id", moderator)];
        self.call(Method::PATCH, "/chat/settings", &q, Some(patch)).await.map(|_| ())
    }

    /// Subscribes the WebSocket session to one EventSub type.
    pub async fn subscribe(
        &self,
        session_id: &str,
        kind: &str,
        version: &str,
        condition: Value,
    ) -> Result<(), TwitchError> {
        let body = json!({
            "type": kind, "version": version, "condition": condition,
            "transport": { "method": "websocket", "session_id": session_id },
        });
        match self.call(Method::POST, "/eventsub/subscriptions", &[], Some(body)).await {
            Ok(_) => Ok(()),
            // Already subscribed on this session (after a reconnect handoff): fine.
            Err(TwitchError::Refused(m)) if m.to_lowercase().contains("already exists") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Concurrent viewer count for `broadcaster` from `GET /streams?user_id=<id>`.
    /// Returns `Ok(None)` when offline or not currently `"live"`.
    pub async fn stream_viewers(&self, broadcaster: &str) -> Result<Option<u64>, TwitchError> {
        let v = self.call(Method::GET, "/streams", &[("user_id", broadcaster)], None).await?;
        Ok(parse_stream_viewers(&v))
    }
}

fn parse_poll_first(v: Value) -> Result<TwitchPoll, TwitchError> {
    let first = v
        .get("data")
        .and_then(|d| d.get(0))
        .cloned()
        .ok_or_else(|| TwitchError::Protocol("no poll returned".into()))?;
    serde_json::from_value(first).map_err(|e| TwitchError::Protocol(e.to_string()))
}

/// Extracts `viewer_count` from a Helix `GET /streams` response when a stream is `"live"`.
pub fn parse_stream_viewers(v: &Value) -> Option<u64> {
    let items = v.get("data").and_then(Value::as_array)?;
    items.iter().find_map(|s| {
        if s.get("type").and_then(Value::as_str) != Some("live") {
            return None;
        }
        s.get("viewer_count").and_then(Value::as_u64)
    })
}
