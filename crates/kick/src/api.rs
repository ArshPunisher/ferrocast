//! Kick's public REST API. Token refresh needs the app's client secret, which lives only
//! on the relay (R-46), so refreshing is a callback the engine wires to the relay.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{EVENTS, KickError, now_ms};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KickTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

impl KickTokens {
    pub fn needs_refresh(&self, now: i64) -> bool {
        now >= self.expires_at - 300_000
    }
}

pub type RefreshFuture = Pin<Box<dyn Future<Output = Result<KickTokens, KickError>> + Send>>;
type Refresher = Arc<dyn Fn(String) -> RefreshFuture + Send + Sync>;
type OnTokens = Arc<dyn Fn(&KickTokens) -> Result<(), String> + Send + Sync>;

pub struct KickApi {
    http: reqwest::Client,
    base: String,
    tokens: Mutex<KickTokens>,
    refresher: Refresher,
    on_tokens: OnTokens,
}

impl fmt::Debug for KickApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KickApi").field("base", &self.base).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KickUser {
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

impl KickApi {
    pub fn new(base: &str, tokens: KickTokens, refresher: Refresher, on_tokens: OnTokens) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("Ferrocast/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        KickApi { http, base: base.trim_end_matches('/').to_string(), tokens: Mutex::new(tokens), refresher, on_tokens }
    }

    async fn access(&self) -> Result<String, KickError> {
        let mut t = self.tokens.lock().await;
        if t.needs_refresh(now_ms()) {
            *t = self.refresh_locked(&t).await?;
        }
        Ok(t.access_token.clone())
    }

    async fn refresh_after_401(&self, seen: &str) -> Result<String, KickError> {
        let mut t = self.tokens.lock().await;
        if t.access_token != seen {
            return Ok(t.access_token.clone());
        }
        *t = self.refresh_locked(&t).await?;
        Ok(t.access_token.clone())
    }

    async fn refresh_locked(&self, current: &KickTokens) -> Result<KickTokens, KickError> {
        let fresh = (self.refresher)(current.refresh_token.clone()).await?;
        (self.on_tokens)(&fresh).map_err(|e| KickError::Refused(format!("couldn't save the new Kick sign-in: {e}")))?;
        Ok(fresh)
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, KickError> {
        let mut token = self.access().await?;
        for attempt in 0..2 {
            let mut req =
                self.http.request(method.clone(), format!("{}{path}", self.base)).query(query).bearer_auth(&token);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let res = req.send().await.map_err(KickError::network)?;
            if res.status().as_u16() == 401 && attempt == 0 {
                token = self.refresh_after_401(&token).await?;
                continue;
            }
            if !res.status().is_success() {
                return Err(KickError::from_response(res).await);
            }
            if res.status().as_u16() == 204 {
                return Ok(Value::Null);
            }
            return Ok(res.json().await.unwrap_or(Value::Null));
        }
        Err(KickError::SignInNeeded)
    }

    /// The token's owner (Kick returns the caller when no id is given).
    pub async fn me(&self) -> Result<KickUser, KickError> {
        let v = self.call(Method::GET, "/public/v1/users", &[], None).await?;
        let u = v.pointer("/data/0").ok_or_else(|| KickError::Refused("no user returned".into()))?;
        let id = match u.get("user_id") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => return Err(KickError::Refused("the user has no id".into())),
        };
        Ok(KickUser {
            id,
            name: u.get("name").and_then(Value::as_str).unwrap_or("Kick").to_string(),
            avatar_url: u.get("profile_picture").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from),
        })
    }

    pub async fn send_chat(&self, broadcaster: &str, content: &str, reply_to: Option<&str>) -> Result<(), KickError> {
        let mut body = json!({ "type": "user", "content": content, "broadcaster_user_id": numeric(broadcaster)? });
        if let Some(r) = reply_to {
            body["reply_to_message_id"] = json!(r);
        }
        self.call(Method::POST, "/public/v1/chat", &[], Some(body)).await.map(|_| ())
    }

    pub async fn delete_message(&self, message_id: &str) -> Result<(), KickError> {
        let id: String = message_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        if id.is_empty() {
            return Err(KickError::Refused("that isn't a message id".into()));
        }
        self.call(Method::DELETE, &format!("/public/v1/chat/{id}"), &[], None).await.map(|_| ())
    }

    /// `seconds = None` bans; otherwise a timeout, rounded up to Kick's whole minutes.
    pub async fn ban(
        &self,
        broadcaster: &str,
        user: &str,
        seconds: Option<u32>,
        reason: Option<&str>,
    ) -> Result<(), KickError> {
        let mut body = json!({ "broadcaster_user_id": numeric(broadcaster)?, "user_id": numeric(user)? });
        if let Some(s) = seconds {
            body["duration"] = json!(s.div_ceil(60).clamp(1, 10_080));
        }
        if let Some(r) = reason.filter(|r| !r.is_empty()) {
            body["reason"] = json!(r.chars().take(100).collect::<String>());
        }
        self.call(Method::POST, "/public/v1/moderation/bans", &[], Some(body)).await.map(|_| ())
    }

    pub async fn unban(&self, broadcaster: &str, user: &str) -> Result<(), KickError> {
        let body = json!({ "broadcaster_user_id": numeric(broadcaster)?, "user_id": numeric(user)? });
        self.call(Method::DELETE, "/public/v1/moderation/bans", &[], Some(body)).await.map(|_| ())
    }

    /// Subscribes the channel's events to the app's webhook (delivered to the relay).
    pub async fn subscribe_events(&self, broadcaster: &str) -> Result<(), KickError> {
        let events: Vec<Value> = EVENTS.iter().map(|(n, v)| json!({ "name": n, "version": v })).collect();
        let body = json!({ "broadcaster_user_id": numeric(broadcaster)?, "events": events, "method": "webhook" });
        self.call(Method::POST, "/public/v1/events/subscriptions", &[], Some(body)).await.map(|_| ())
    }
}

impl KickApi {
    /// Categories matching `query` (`GET /public/v1/categories?q=`), up to 10.
    pub async fn search_categories(&self, query: &str) -> Result<Vec<ferrocast_connector::CategoryHit>, KickError> {
        let q: String = query.trim().chars().take(100).collect();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let v = self.call(Method::GET, "/public/v1/categories", &[("q", q.as_str())], None).await?;
        Ok(v.get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| {
                let id = match c.get("id")? {
                    Value::Number(n) => n.to_string(),
                    Value::String(s) => s.clone(),
                    _ => return None,
                };
                Some(ferrocast_connector::CategoryHit {
                    id,
                    name: c.get("name")?.as_str()?.to_string(),
                    image_url: c.get("thumbnail").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from),
                })
            })
            .take(10)
            .collect())
    }

    /// `PATCH /public/v1/channels`: title, category and tags (`channel:write`). Kick has
    /// no stream language. Returns the fields sent.
    pub async fn update_channel(
        &self,
        info: &ferrocast_core::stream_info::StreamInfo,
    ) -> Result<Vec<String>, KickError> {
        let mut body = json!({});
        let mut applied = Vec::new();
        if let Some(t) = &info.title {
            body["stream_title"] = json!(t);
            applied.push("title".to_string());
        }
        if let Some(c) = &info.category {
            let id = match c.ids.kick.clone() {
                Some(id) => id,
                None => self
                    .search_categories(&c.name)
                    .await?
                    .into_iter()
                    .find(|h| h.name.eq_ignore_ascii_case(c.name.trim()))
                    .map(|h| h.id)
                    .ok_or_else(|| KickError::Refused(format!("Kick has no category called \"{}\"", c.name)))?,
            };
            body["category_id"] = json!(numeric(&id)?);
            applied.push("category".to_string());
        }
        if let Some(tags) = &info.tags {
            body["custom_tags"] = json!(tags);
            applied.push("tags".to_string());
        }
        if applied.is_empty() {
            return Ok(applied);
        }
        match self.call(Method::PATCH, "/public/v1/channels", &[], Some(body)).await {
            Err(KickError::Forbidden(_)) => Err(KickError::MissingScope("channel:write".into())),
            other => other.map(|_| applied),
        }
    }
}

impl KickApi {
    /// Go live everywhere: `GET /public/v1/channels` for the signed-in channel's stream URL and
    /// key. Kick leaves the key out without `streamkey:read`, so an older sign-in is told to
    /// reconnect.
    pub async fn ingest(&self) -> Result<ferrocast_connector::Ingest, KickError> {
        let v = match self.call(Method::GET, "/public/v1/channels", &[], None).await {
            Err(KickError::Forbidden(_)) => return Err(KickError::MissingScope("streamkey:read".into())),
            other => other?,
        };
        let stream = v.pointer("/data/0/stream");
        let field = |name: &str| {
            stream
                .and_then(|s| s.get(name))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        let key = field("key").ok_or_else(|| KickError::MissingScope("streamkey:read".into()))?;
        let url = field("url")
            .filter(|u| u.starts_with("rtmps://") || u.starts_with("rtmp://"))
            .ok_or_else(|| KickError::Refused("Kick didn't give out the stream address".into()))?;
        Ok(ferrocast_connector::Ingest { url: url.trim_end_matches('/').to_string(), key })
    }
}

fn numeric(id: &str) -> Result<u64, KickError> {
    id.parse().map_err(|_| KickError::Refused(format!("\"{id}\" isn't a Kick user id")))
}
