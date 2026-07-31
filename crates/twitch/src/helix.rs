//! Helix requests: one place that adds auth, retries once after a 401 with a refreshed
//! token, and turns 403/429 into errors the dashboard can explain (R-19).

use std::sync::Arc;

use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Auth, TwitchError};

#[derive(Debug, Clone)]
pub struct Helix {
    pub(crate) auth: Arc<Auth>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    pub id: String,
    pub login: String,
    pub display_name: String,
    #[serde(default)]
    pub profile_image_url: String,
}

impl Helix {
    pub fn new(auth: Arc<Auth>) -> Self {
        Helix { auth }
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
        for attempt in 0..2 {
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
            if res.status().as_u16() == 401 && attempt == 0 {
                token = self.auth.refresh_after_401(&token).await?;
                continue;
            }
            if !res.status().is_success() {
                return Err(TwitchError::from_response(res).await);
            }
            if res.status().as_u16() == 204 {
                return Ok(Value::Null);
            }
            return res.json().await.map_err(TwitchError::network);
        }
        Err(TwitchError::SignInNeeded)
    }

    pub async fn me(&self) -> Result<User, TwitchError> {
        let v = self.call(Method::GET, "/users", &[], None).await?;
        let first = v
            .get("data")
            .and_then(|d| d.get(0))
            .cloned()
            .ok_or_else(|| TwitchError::Protocol("no user returned".into()))?;
        serde_json::from_value(first).map_err(|e| TwitchError::Protocol(e.to_string()))
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
}
