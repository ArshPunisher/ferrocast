//! The engine's side of the relay: one install identity, one outbound WebSocket that
//! receives queued items (Kick events, Kick sign-ins), and Kick's sign-in and refresh
//! through the relay (the client secret lives only there, R-46).
//!
//! If the relay is down, Twitch and YouTube carry on; Kick events wait in the relay's queue
//! and arrive when the link comes back (R-01).

use std::sync::Arc;
use std::time::Duration;

use ferrocast_core::event::{ConnectorStatus, Millis, Platform};
use ferrocast_kick::{KickApi, KickConnector, KickError, KickTokens};
use ferrocast_store::repo;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::accounts::ConnectFlow;
use crate::frames::NoticeLevel;
use crate::state::{AccountView, ConnectorView};
use crate::{Engine, EngineError, now};

const INSTALL: &str = "relay-install";
const KICK_TOKENS: &str = "kick-tokens";

impl Engine {
    fn relay_url(&self) -> Result<String, EngineError> {
        self.apps().relay_url.clone().ok_or_else(|| {
            EngineError::Unavailable("This build of Ferrocast has no relay set up, so Kick can't connect.".into())
        })
    }

    fn relay_http(&self) -> reqwest::Client {
        reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap_or_default()
    }

    /// The install's `id.secret`, registering with the relay the first time.
    async fn relay_auth(&self) -> Result<String, EngineError> {
        if let Some(a) = self.inner.secrets.get(INSTALL).map_err(|e| EngineError::Unavailable(e.to_string()))? {
            return Ok(a);
        }
        let url = self.relay_url()?;
        let res = self
            .relay_http()
            .post(format!("{url}/v1/installs"))
            .send()
            .await
            .map_err(|e| EngineError::Unavailable(format!("Couldn't reach the Ferrocast relay: {e}")))?;
        if !res.status().is_success() {
            return Err(EngineError::Unavailable(format!(
                "The Ferrocast relay refused (status {}).",
                res.status().as_u16()
            )));
        }
        let v: Value = res.json().await.map_err(|e| EngineError::Unavailable(e.to_string()))?;
        let auth = format!("{}.{}", v["install_id"].as_str().unwrap_or(""), v["install_secret"].as_str().unwrap_or(""));
        self.inner.secrets.set(INSTALL, &auth).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        Ok(auth)
    }

    /// Starts the relay link once (idempotent).
    pub(crate) fn ensure_relay_link(&self) {
        let mut slot = self.inner.relay_link.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let engine = self.clone();
        *slot = Some(tokio::spawn(async move { engine.relay_loop().await }));
    }

    async fn relay_loop(&self) {
        let mut attempt = 0u32;
        loop {
            match self.relay_session().await {
                Ok(()) => attempt = 0,
                Err(e) => {
                    attempt = attempt.saturating_add(1);
                    tracing::warn!(error = %e, attempt, "relay link lost");
                    self.set_relay_state("reconnecting", Some(e));
                }
            }
            let wait = Duration::from_secs(1u64 << attempt.min(6)).min(Duration::from_secs(60));
            tokio::time::sleep(wait + Duration::from_millis(u64::try_from(now().0.rem_euclid(500)).unwrap_or(0))).await;
        }
    }

    fn set_relay_state(&self, state: &str, detail: Option<String>) {
        let mut st = self.state_mut();
        let v = st.connectors.entry("relay".into()).or_insert_with(ConnectorView::off);
        v.state = state.into();
        v.detail = detail;
        v.last_event_at = Some(now().0);
    }

    /// One connection: read items, hand each to the engine, ack it.
    async fn relay_session(&self) -> Result<(), String> {
        let url = self.relay_url().map_err(|e| e.to_string())?;
        let auth = self.relay_auth().await.map_err(|e| e.to_string())?;
        let ws_url = format!("{}/v1/link", url.replacen("http", "ws", 1));
        let mut req = ws_url.into_client_request().map_err(|e| e.to_string())?;
        req.headers_mut()
            .insert("authorization", format!("Bearer {auth}").parse().map_err(|_| "bad install id".to_string())?);
        let (ws, _) = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(req))
            .await
            .map_err(|_| "timed out connecting to the relay".to_string())?
            .map_err(|e| e.to_string())?;
        self.set_relay_state("connected", None);
        let (mut tx, mut rx) = ws.split();
        loop {
            // The relay pings every 30 s; silence for 90 s means the link is dead.
            let msg = match tokio::time::timeout(Duration::from_secs(90), rx.next()).await {
                Err(_) => return Err("the relay went quiet".into()),
                Ok(None) => return Err("the relay closed the link".into()),
                Ok(Some(Err(e))) => return Err(e.to_string()),
                Ok(Some(Ok(m))) => m,
            };
            let Message::Text(text) = msg else { continue };
            let Ok(item) = serde_json::from_str::<Value>(&text) else { continue };
            if item["type"] != "item" {
                continue;
            }
            self.handle_relay_item(item["kind"].as_str().unwrap_or(""), &item["payload"]).await;
            let ack = json!({ "type": "ack", "id": item["id"] }).to_string();
            tx.send(Message::text(ack)).await.map_err(|e| e.to_string())?;
        }
    }

    async fn handle_relay_item(&self, kind: &str, payload: &Value) {
        match kind {
            "kick_event" => {
                // Queued before a disconnect: Kick is gone now, so drop it.
                if self.connector(Platform::Kick).is_none() {
                    return;
                }
                let ty = payload["type"].as_str().unwrap_or("");
                let id = payload["message_id"].as_str().unwrap_or("");
                let e = ferrocast_kick::map::map(ty, id, &payload["body"], Millis(now().0));
                let _ = self.submit(e).await;
            }
            "kick_connected" => {
                if let Err(e) = self.finish_kick(payload).await {
                    self.notice(NoticeLevel::Error, &format!("Kick sign-in didn't finish: {e}"));
                }
            }
            other => tracing::debug!(kind = other, "ignored a relay item"),
        }
    }

    pub(crate) async fn connect_kick(&self) -> Result<ConnectFlow, EngineError> {
        let url = self.relay_url()?;
        let auth = self.relay_auth().await?;
        self.ensure_relay_link();
        let res = self
            .relay_http()
            .post(format!("{url}/v1/kick/oauth"))
            .bearer_auth(&auth)
            .send()
            .await
            .map_err(|e| EngineError::Unavailable(format!("Couldn't reach the Ferrocast relay: {e}")))?;
        let v: Value = res.json().await.map_err(|e| EngineError::Unavailable(e.to_string()))?;
        match v["url"].as_str() {
            Some(u) => Ok(ConnectFlow::Browser { url: u.to_string() }),
            None => Err(EngineError::Unavailable(
                v.pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("The relay couldn't start Kick sign-in.")
                    .to_string(),
            )),
        }
    }

    async fn finish_kick(&self, payload: &Value) -> Result<(), EngineError> {
        let tokens: KickTokens = serde_json::from_value(payload["tokens"].clone())
            .map_err(|e| EngineError::Invalid(format!("bad Kick tokens: {e}")))?;
        let user_id = payload.pointer("/user/id").and_then(Value::as_str).unwrap_or("").to_string();
        if user_id.is_empty() {
            return Err(EngineError::Invalid("the Kick account has no id".into()));
        }
        let name = payload.pointer("/user/name").and_then(Value::as_str).unwrap_or("Kick").to_string();
        let avatar =
            payload.pointer("/user/avatar_url").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
        let json = serde_json::to_string(&tokens).map_err(|e| EngineError::Internal(e.to_string()))?;
        self.inner.secrets.set(KICK_TOKENS, &json).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        let account = repo::settings::Account {
            platform: "kick".into(),
            user_id: user_id.clone(),
            login: name.clone(),
            display_name: name,
            avatar_url: avatar,
            scopes: ferrocast_kick::SCOPES.iter().map(|s| s.to_string()).collect(),
            connected_at: now().0,
        };
        let saved = account.clone();
        self.db(move |s| repo::settings::save_account(s, &saved)).await?;
        let api = self.kick_api(tokens)?;
        // Events go to the app's webhook (the relay); subscribe this channel to them.
        if let Err(e) = api.subscribe_events(&user_id).await {
            self.notice(NoticeLevel::Warn, &format!("Kick is connected, but subscribing to its events failed: {e}"));
        }
        self.start_kick(api, account);
        self.notice(NoticeLevel::Info, "Kick is connected.");
        Ok(())
    }

    fn kick_api(&self, tokens: KickTokens) -> Result<Arc<KickApi>, EngineError> {
        let relay = self.relay_url()?;
        let engine = self.clone();
        let refresher = Arc::new(move |refresh_token: String| {
            let engine = engine.clone();
            let relay = relay.clone();
            Box::pin(async move {
                let auth = engine.relay_auth().await.map_err(|e| KickError::Network(e.to_string()))?;
                let res = engine
                    .relay_http()
                    .post(format!("{relay}/v1/kick/refresh"))
                    .bearer_auth(auth)
                    .json(&json!({ "refresh_token": refresh_token }))
                    .send()
                    .await
                    .map_err(|e| KickError::Network(e.to_string()))?;
                match res.status().as_u16() {
                    200..=299 => res.json::<KickTokens>().await.map_err(|e| KickError::Network(e.to_string())),
                    401 => Err(KickError::SignInNeeded),
                    s => Err(KickError::Refused(format!("the relay answered {s}"))),
                }
            }) as ferrocast_kick::api::RefreshFuture
        });
        let secrets = self.inner.secrets.clone();
        let persist = Arc::new(move |t: &KickTokens| {
            let json = serde_json::to_string(t).map_err(|e| e.to_string())?;
            secrets.set(KICK_TOKENS, &json).map_err(|e| e.to_string())
        });
        Ok(Arc::new(KickApi::new(&self.apps().kick_api, tokens, refresher, persist)))
    }

    fn start_kick(&self, api: Arc<KickApi>, account: repo::settings::Account) {
        self.register_connector(Arc::new(KickConnector { api, broadcaster_id: account.user_id.clone() }));
        let mut st = self.state_mut();
        let v = st.connectors.entry("kick".into()).or_insert_with(ConnectorView::off);
        v.account = Some(AccountView { name: account.display_name.clone(), avatar_url: account.avatar_url.clone() });
        v.state = "connected".into();
        v.detail = Some("Events can arrive a few seconds late on Kick.".into());
        drop(st);
        self.ensure_relay_link();
    }

    /// On start: Kick saved from an earlier run.
    pub(crate) async fn resume_kick(&self, account: repo::settings::Account) {
        let saved = self.inner.secrets.get(KICK_TOKENS).ok().flatten();
        match saved.and_then(|j| serde_json::from_str::<KickTokens>(&j).ok()) {
            Some(t) => match self.kick_api(t) {
                Ok(api) => self.start_kick(api, account),
                Err(e) => tracing::warn!(%e, "Kick is saved but can't start"),
            },
            None => {
                let _ = self
                    .submit(ferrocast_core::event::Event::live(
                        Platform::Kick,
                        None,
                        Millis(0),
                        ferrocast_core::event::EventKind::Connector(ConnectorStatus::SignInNeeded),
                    ))
                    .await;
            }
        }
    }

    pub(crate) fn forget_kick(&self) -> Result<(), EngineError> {
        self.inner.secrets.delete(KICK_TOKENS).map_err(|e| EngineError::Unavailable(e.to_string()))
    }
}
