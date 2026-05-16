//! The REST side: tokens (refreshed once at a time and saved before use), finding the
//! live chat, sending, deleting and banning.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex};

use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::oauth::{self, AppCredentials, Tokens};
use crate::{Endpoints, YoutubeError, now_ms};

type OnTokens = Arc<dyn Fn(&Tokens) -> Result<(), String> + Send + Sync>;

pub struct Api {
    pub(crate) http: reqwest::Client,
    pub(crate) ep: Endpoints,
    app: AppCredentials,
    tokens: Mutex<Tokens>,
    on_tokens: OnTokens,
    /// Ban ids by banned channel, so "unban" can find the ban YouTube created.
    bans: StdMutex<HashMap<String, String>>,
}

impl fmt::Debug for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Api").field("api", &self.ep.api).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Channel {
    pub id: String,
    pub title: String,
    pub avatar_url: Option<String>,
}

impl Api {
    pub fn new(http: reqwest::Client, ep: Endpoints, app: AppCredentials, tokens: Tokens, on_tokens: OnTokens) -> Self {
        Api { http, ep, app, tokens: Mutex::new(tokens), on_tokens, bans: StdMutex::new(HashMap::new()) }
    }

    pub async fn access(&self) -> Result<String, YoutubeError> {
        let mut t = self.tokens.lock().await;
        if t.needs_refresh(now_ms()) {
            *t = self.refresh_locked(&t).await?;
        }
        Ok(t.access_token.clone())
    }

    pub async fn refresh_after_401(&self, seen: &str) -> Result<String, YoutubeError> {
        let mut t = self.tokens.lock().await;
        if t.access_token != seen {
            return Ok(t.access_token.clone());
        }
        *t = self.refresh_locked(&t).await?;
        Ok(t.access_token.clone())
    }

    async fn refresh_locked(&self, current: &Tokens) -> Result<Tokens, YoutubeError> {
        let fresh = oauth::refresh(&self.http, &self.ep, &self.app, current).await?;
        (self.on_tokens)(&fresh)
            .map_err(|e| YoutubeError::Refused(format!("couldn't save the new YouTube sign-in: {e}")))?;
        Ok(fresh)
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, YoutubeError> {
        let mut token = self.access().await?;
        for attempt in 0..2 {
            let mut req =
                self.http.request(method.clone(), format!("{}{path}", self.ep.api)).query(query).bearer_auth(&token);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let res = req.send().await.map_err(YoutubeError::network)?;
            if res.status().as_u16() == 401 && attempt == 0 {
                token = self.refresh_after_401(&token).await?;
                continue;
            }
            if !res.status().is_success() {
                return Err(YoutubeError::from_response(res).await);
            }
            if res.status().as_u16() == 204 {
                return Ok(Value::Null);
            }
            return res.json().await.map_err(YoutubeError::network);
        }
        Err(YoutubeError::SignInNeeded)
    }

    /// The signed-in channel (1 unit).
    pub async fn me(&self) -> Result<Channel, YoutubeError> {
        let v = self.call(Method::GET, "/channels", &[("part", "snippet"), ("mine", "true")], None).await?;
        let item = v
            .pointer("/items/0")
            .ok_or_else(|| YoutubeError::SignIn("This Google account has no YouTube channel.".into()))?;
        Ok(Channel {
            id: item["id"].as_str().unwrap_or("").to_string(),
            title: item.pointer("/snippet/title").and_then(Value::as_str).unwrap_or("YouTube").to_string(),
            avatar_url: item.pointer("/snippet/thumbnails/default/url").and_then(Value::as_str).map(String::from),
        })
    }

    /// The live chat of the channel's active broadcast, if it's live now (1 unit).
    pub async fn active_live_chat(&self) -> Result<Option<String>, YoutubeError> {
        let q = [("part", "snippet"), ("broadcastStatus", "active"), ("broadcastType", "all"), ("maxResults", "5")];
        let v = self.call(Method::GET, "/liveBroadcasts", &q, None).await?;
        Ok(v.get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|i| i.pointer("/snippet/liveChatId").and_then(Value::as_str).map(String::from)))
    }

    pub async fn send(&self, live_chat_id: &str, text: &str) -> Result<(), YoutubeError> {
        let body = json!({ "snippet": { "liveChatId": live_chat_id, "type": "textMessageEvent",
                                        "textMessageDetails": { "messageText": text } } });
        self.call(Method::POST, "/liveChat/messages", &[("part", "snippet")], Some(body)).await.map(|_| ())
    }

    pub async fn delete(&self, message_id: &str) -> Result<(), YoutubeError> {
        self.call(Method::DELETE, "/liveChat/messages", &[("id", message_id)], None).await.map(|_| ())
    }

    /// Bans a channel from the live chat; `seconds` makes it a timeout.
    pub async fn ban(&self, live_chat_id: &str, channel_id: &str, seconds: Option<u32>) -> Result<(), YoutubeError> {
        let mut snippet = json!({ "liveChatId": live_chat_id, "type": "permanent", "bannedUserDetails": { "channelId": channel_id } });
        if let Some(s) = seconds {
            snippet["type"] = json!("temporary");
            snippet["banDurationSeconds"] = json!(s.clamp(1, 86_400 * 14));
        }
        let v = self
            .call(Method::POST, "/liveChat/bans", &[("part", "snippet")], Some(json!({ "snippet": snippet })))
            .await?;
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            self.bans.lock().unwrap_or_else(|p| p.into_inner()).insert(channel_id.to_string(), id.to_string());
        }
        Ok(())
    }

    /// YouTube unbans by the id of the ban it created; we only know ids for bans made from
    /// this app during this run.
    pub async fn unban(&self, channel_id: &str) -> Result<bool, YoutubeError> {
        let id = self.bans.lock().unwrap_or_else(|p| p.into_inner()).remove(channel_id);
        match id {
            Some(id) => self.call(Method::DELETE, "/liveChat/bans", &[("id", &id)], None).await.map(|_| true),
            None => Ok(false),
        }
    }
}
