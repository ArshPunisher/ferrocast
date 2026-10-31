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
use crate::frames::{LiveFrame, NoticeLevel};
use crate::state::{AccountView, ConnectorView};
use crate::{Engine, EngineError, now};

pub(crate) const INSTALL: &str = "relay-install";
const KICK_TOKENS: &str = "kick-tokens";

impl Engine {
    pub(crate) fn relay_url(&self) -> Result<String, EngineError> {
        self.apps().relay_url.clone().ok_or_else(|| {
            EngineError::Unavailable("This build of Ferrocast has no relay set up, so Kick and tips can't work.".into())
        })
    }

    pub(crate) fn relay_http(&self) -> reqwest::Client {
        reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap_or_default()
    }

    /// The install's `id.secret`, registering with the relay the first time.
    pub(crate) async fn relay_auth(&self) -> Result<String, EngineError> {
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
        let connected = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(req))
            .await
            .map_err(|_| "timed out connecting to the relay".to_string())?;
        let (ws, _) = match connected {
            Ok(c) => c,
            // The relay no longer knows this install (it removes ones that never connected
            // after a week): forget it, so the next attempt registers a fresh one.
            Err(tokio_tungstenite::tungstenite::Error::Http(res)) if res.status().as_u16() == 401 => {
                return match self.inner.secrets.delete(INSTALL) {
                    Ok(()) => Err("the relay didn't recognise this install; registering again".into()),
                    Err(e) => {
                        Err(format!("the relay didn't recognise this install, and it couldn't be forgotten: {e}"))
                    }
                };
            }
            Err(e) => return Err(e.to_string()),
        };
        self.set_relay_state("connected", None);
        let (mut tx, mut rx) = ws.split();
        // Answers to guests' offers, made on their own tasks (the Studio takes a moment).
        let (replies_tx, mut replies) = tokio::sync::mpsc::unbounded_channel::<String>();
        // `bot_seen` every 30 s while the cloud bot is on: the cloud stands by (cloud_bot.rs).
        let mut beat = tokio::time::interval(crate::cloud_bot::HEARTBEAT);
        let mut heard = tokio::time::Instant::now();
        loop {
            // The relay pings every 30 s; silence for 90 s means the link is dead.
            let next = tokio::select! {
                _ = beat.tick() => {
                    if self.cloud_bot_on().await {
                        // The chat platforms this PC answers: the cloud covers the others.
                        let seen = self.cloud_bot_seen_frame().to_string();
                        tx.send(Message::text(seen)).await.map_err(|e| e.to_string())?;
                    }
                    continue;
                }
                Some(r) = replies.recv() => {
                    tx.send(Message::text(r)).await.map_err(|e| e.to_string())?;
                    continue;
                }
                _ = tokio::time::sleep_until(heard + Duration::from_secs(90)) => None,
                m = rx.next() => Some(m),
            };
            let msg = match next {
                None => return Err("the relay went quiet".into()),
                Some(None) => return Err("the relay closed the link".into()),
                Some(Some(Err(e))) => return Err(e.to_string()),
                Some(Some(Ok(m))) => m,
            };
            heard = tokio::time::Instant::now();
            let Message::Text(text) = msg else { continue };
            let Ok(item) = serde_json::from_str::<Value>(&text) else { continue };
            if item["type"] == "guest_offer" {
                let (engine, out) = (self.clone(), replies_tx.clone());
                tokio::spawn(async move {
                    let _ = out.send(engine.guest_offer(&item).await.to_string());
                });
                continue;
            }
            if item["type"] != "item" {
                continue;
            }
            self.handle_relay_item(item["kind"].as_str().unwrap_or(""), &item["payload"]).await;
            let ack = json!({ "type": "ack", "id": item["id"] }).to_string();
            tx.send(Message::text(ack)).await.map_err(|e| e.to_string())?;
        }
    }

    pub(crate) async fn handle_relay_item(&self, kind: &str, payload: &Value) {
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
                    self.notice("account:kick", NoticeLevel::Error, &format!("Kick sign-in didn't finish: {e}"));
                }
            }
            "tip" => {
                if let Some(e) = tip_event(payload) {
                    let _ = self.submit(e).await;
                }
            }
            // The cloud bot changed a counter while this PC was off (cloud_bot.rs).
            "bot_counter" => self.apply_cloud_counter(payload).await,
            // A phone banned or timed out someone through the cloud bot while this PC was off.
            "cloud_moderation" => self.apply_cloud_moderation(payload).await,
            // The web dashboard asked for this PC (ACCOUNTS.md §7): open the tunnel, without
            // Pro too while signed in to an account.
            "remote_wanted" => {
                if self.remote_allowed().await {
                    self.set_remote_wanted(true).await;
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
            self.notice(
                "account:kick",
                NoticeLevel::Warn,
                &format!("Kick is connected, but subscribing to its events failed: {e}"),
            );
        }
        self.start_kick(api, account);
        self.notice("account:kick", NoticeLevel::Info, "Kick is connected.");
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
        // No standing caveat: the dashboard warns only while the relay link is down.
        v.detail = None;
        let view = st.connectors.clone();
        drop(st);
        // Open dashboards (and a waiting "Connect Kick" dialog) learn it now, not on reload.
        self.send_live(LiveFrame::new("status", &json!({ "connectors": view })));
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

    /// Sends the tip page settings to the relay; returns the page and webhook URLs.
    pub async fn save_tips(&self, cfg: &Value) -> Result<Value, EngineError> {
        let url = self.relay_url()?;
        let auth = self.relay_auth().await?;
        self.ensure_relay_link();
        let res = self
            .relay_http()
            .put(format!("{url}/v1/tips/config"))
            .bearer_auth(&auth)
            .json(cfg)
            .send()
            .await
            .map_err(|e| EngineError::Unavailable(format!("Couldn't reach the Ferrocast relay: {e}")))?;
        let status = res.status().as_u16();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        let message = || {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("The relay refused the tip page settings.")
                .to_string()
        };
        match status {
            200..=299 => Ok(v),
            409 => Err(EngineError::Conflict(message())),
            429 => Err(EngineError::TooMany(message())),
            401 | 403 => Err(EngineError::Unavailable(
                "The Ferrocast relay doesn't recognise this install. Reconnect Kick or save the tip page again in a minute."
                    .into(),
            )),
            400 | 413 | 415 | 422 => Err(EngineError::Invalid(message())),
            _ => Err(EngineError::Unavailable(message())),
        }
    }

    /// Takes the tip page down on the relay (its name stays held for this install).
    pub async fn delete_tips(&self) -> Result<(), EngineError> {
        let url = self.relay_url()?;
        let auth = self.relay_auth().await?;
        let res = self
            .relay_http()
            .delete(format!("{url}/v1/tips/config"))
            .bearer_auth(&auth)
            .send()
            .await
            .map_err(|e| EngineError::Unavailable(format!("Couldn't reach the Ferrocast relay: {e}")))?;
        match res.status().as_u16() {
            200..=299 => Ok(()),
            401 | 403 => Err(EngineError::Unavailable(
                "The Ferrocast relay doesn't recognise this install. Try again in a minute.".into(),
            )),
            s => Err(EngineError::Unavailable(format!("The relay couldn't take the tip page down (it answered {s})."))),
        }
    }

    pub(crate) fn forget_kick(&self) -> Result<(), EngineError> {
        self.inner.secrets.delete(KICK_TOKENS).map_err(|e| EngineError::Unavailable(e.to_string()))
    }
}

/// A relay `tip` item → a Tip event, or a Reversal for refunds and chargebacks. The source
/// id is `provider:payment` for both, so goals take back exactly what that payment added.
pub(crate) fn tip_event(p: &Value) -> Option<ferrocast_core::event::Event> {
    use ferrocast_core::event::{Actor, Event, EventKind, PaymentStatus, ReversalReason, TipProvider};
    let provider_key = p["provider"].as_str()?;
    let provider = match provider_key {
        "razorpay" => TipProvider::Razorpay,
        "stripe" => TipProvider::Stripe,
        "ko_fi" => TipProvider::KoFi,
        "paypal" => TipProvider::Paypal,
        _ => return None,
    };
    let payment = p["payment_id"].as_str().filter(|s| !s.is_empty())?;
    let key = format!("{provider_key}:{payment}");
    // When the relay received it: a tip held while the desktop was off belongs to that day's
    // goals and tax records, not today's. Never in the future.
    let at = Millis(p["at"].as_i64().filter(|t| *t > 0).map_or(now().0, |t| t.min(now().0)));
    let kind = match p["status"].as_str()? {
        "captured" => {
            use ferrocast_core::money::Money;
            let currency = p["currency"].as_str()?;
            // Razorpay and Stripe send integer minor units with their digit count; Ko-fi sends
            // a decimal string. Either way core's own currency table decides the exponent.
            let money = match (p["amount_minor"].as_i64(), p["digits"].as_u64()) {
                (Some(minor), Some(digits)) => Money::from_scaled(minor, u32::try_from(digits).ok()?, currency).ok()?,
                _ => Money::parse_decimal_lenient(p["amount"].as_str()?, currency).ok()?,
            };
            if money.minor <= 0 {
                return None;
            }
            let name = p["name"].as_str().filter(|n| !n.trim().is_empty()).unwrap_or("Someone");
            let mut actor = Actor::named(Platform::Tips, name);
            actor.login = None;
            EventKind::Tip {
                actor,
                money,
                message: p["message"].as_str().filter(|m| !m.trim().is_empty()).map(String::from),
                provider,
                payment_ref: payment.to_string(),
                status: PaymentStatus::Captured,
            }
        }
        "refunded" => {
            return Some(Event::live(
                Platform::Tips,
                Some(format!("{key}#refund")),
                at,
                EventKind::Reversal { original_source_id: key, reason: ReversalReason::Refund },
            ));
        }
        "disputed" => {
            return Some(Event::live(
                Platform::Tips,
                Some(format!("{key}#dispute")),
                at,
                EventKind::Reversal { original_source_id: key, reason: ReversalReason::Chargeback },
            ));
        }
        _ => return None,
    };
    Some(Event::live(Platform::Tips, Some(key), at, kind))
}

#[cfg(test)]
mod tip_tests {
    use super::tip_event;
    use ferrocast_core::event::{EventKind, PaymentStatus};
    use serde_json::json;

    #[test]
    fn tips_and_reversals_share_a_key() {
        let tip = tip_event(
            &json!({ "provider": "razorpay", "status": "captured", "payment_id": "pay_1", "amount": "500.00",
                                     "currency": "INR", "name": "Asha", "message": "gg" }),
        )
        .unwrap();
        assert_eq!(tip.source_id.as_deref(), Some("razorpay:pay_1"));
        assert!(
            matches!(&tip.kind, EventKind::Tip { money, status: PaymentStatus::Captured, .. } if money.minor == 50_000)
        );
        let refund =
            tip_event(&json!({ "provider": "razorpay", "status": "refunded", "payment_id": "pay_1" })).unwrap();
        assert!(
            matches!(&refund.kind, EventKind::Reversal { original_source_id, .. } if original_source_id == "razorpay:pay_1")
        );
        let isk = tip_event(&json!({ "provider": "stripe", "status": "captured", "payment_id": "pi_1",
            "amount_minor": 150_000, "digits": 2, "currency": "ISK" }))
        .unwrap();
        assert!(matches!(&isk.kind, EventKind::Tip { money, .. } if money.minor == 1_500), "Stripe's ISK hundredths");
        let jpy = tip_event(&json!({ "provider": "ko_fi", "status": "captured", "payment_id": "k1",
            "amount": "500.00", "currency": "JPY" }))
        .unwrap();
        assert!(matches!(&jpy.kind, EventKind::Tip { money, .. } if money.minor == 500), "Ko-fi's two places on yen");
        for bad in [
            json!({ "provider": "stripe", "status": "captured", "payment_id": "x", "amount_minor": -5, "digits": 2, "currency": "USD" }),
            json!({ "provider": "stripe", "status": "captured", "payment_id": "x", "amount_minor": 5, "digits": 99, "currency": "USD" }),
            json!({ "provider": "venmo", "status": "captured", "payment_id": "x", "amount": "1", "currency": "USD" }),
            json!({ "provider": "stripe", "status": "captured", "payment_id": "", "amount": "1", "currency": "USD" }),
            json!({ "provider": "stripe", "status": "captured", "payment_id": "x", "amount": "-5", "currency": "USD" }),
            json!({ "provider": "stripe", "status": "captured", "payment_id": "x", "amount": "0", "currency": "USD" }),
            json!({ "provider": "stripe", "status": "weird", "payment_id": "x" }),
        ] {
            assert!(tip_event(&bad).is_none(), "{bad}");
        }
    }
}
