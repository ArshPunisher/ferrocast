//! Pro's weekly email for the dashboard's `#/history` (API.md §9.8). The engine passes each
//! call to the relay, which checks Pro, keeps the address sealed and sends the emails; the
//! relay's status, code, message and hint come through unchanged.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// `GET /pro/history/email` → `{ status, address_hint, expires_at, day, next_at, … }`.
pub async fn view(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.weekly_email().await?))
}

/// `PUT /pro/history/email {address}` → 202 `pending`, the confirm email sent.
pub async fn save(State(app): State<AppState>, Body(b): Body<Value>) -> ApiResult<(StatusCode, Json<Value>)> {
    let address = b
        .get("address")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::invalid("address: the email address, as text.").hint("Send { address }."))?;
    if address.len() > 320 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_email",
            "That email address is too long.",
        )
        .hint("Check it for typos, or use another address."));
    }
    Ok((StatusCode::ACCEPTED, Json(app.engine.save_weekly_email(address).await?)))
}

/// `DELETE /pro/history/email` → 204: no more emails, the address deleted on the relay.
pub async fn stop(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.engine.stop_weekly_email().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// End to end: dashboard API → engine → a real relay with a fake mail sender.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrocast_engine::{Engine, PlatformApps};
    use ferrocast_relay::billing::store::Synced;
    use ferrocast_relay::mail::{FakeMailer, MailConfig, Mailer};
    use ferrocast_secrets::Secrets;
    use reqwest::{Method, StatusCode};
    use serde_json::{Value, json};

    use crate::{ServeOptions, serve};

    struct World {
        base: String,
        token: String,
        http: reqwest::Client,
        relay_db: Arc<ferrocast_relay::Db>,
        secrets: Arc<ferrocast_secrets::MemorySecrets>,
        fake: Arc<FakeMailer>,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
        _engine: Engine,
    }

    async fn world(relay_up: bool) -> World {
        world_budget(relay_up, ferrocast_relay::mail::DAILY_BUDGET).await
    }

    async fn world_budget(relay_up: bool, budget: i64) -> World {
        let rdir = tempfile::tempdir().unwrap();
        let rl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay = format!("http://{}", rl.local_addr().unwrap());
        let cfg = ferrocast_relay::Config {
            public_url: relay.clone(),
            tip_url: None,
            kick_client_id: None,
            kick_client_secret: None,
            kick_id_url: relay.clone(),
            kick_api_url: relay.clone(),
        };
        let fake = Arc::new(FakeMailer::new());
        // 32 zero bytes, base64.
        let vault = ferrocast_relay::vault::Vault::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
        let state = ferrocast_relay::AppState::new(cfg, ferrocast_relay::Db::open(&rdir.path().join("r.db")).unwrap())
            .with_vault(Some(vault))
            .with_mail(
                Some(fake.clone() as Arc<dyn Mailer>),
                MailConfig::new(None, Some(b"a server test link key")).with_budget(budget),
            );
        let relay_db = state.db.clone();
        let rapp = ferrocast_relay::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
        if relay_up {
            tokio::spawn(async move { axum::serve(rl, rapp).await });
        } else {
            drop(rl);
        }
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
        let apps = PlatformApps { relay_url: Some(relay.clone()), ..Default::default() };
        let engine = Engine::start_with(dir.path(), secrets.clone(), apps).await.unwrap();
        let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
        World {
            base: server.base_url(),
            token: server.sessions.desktop_token().to_string(),
            http: reqwest::Client::new(),
            relay_db,
            secrets,
            fake,
            _dirs: (dir, rdir),
            _engine: engine,
        }
    }

    impl World {
        async fn call(&self, method: Method, body: Option<Value>) -> (StatusCode, Value) {
            let mut r =
                self.http.request(method, format!("{}/api/v1/pro/history/email", self.base)).bearer_auth(&self.token);
            if let Some(b) = body {
                r = r.json(&b);
            }
            let res = r.send().await.unwrap();
            let s = res.status();
            (s, res.json().await.unwrap_or(Value::Null))
        }

        fn make_pro(&self) {
            let auth = self.secrets.get("relay-install").unwrap().expect("registered with the relay");
            let install = auth.split('.').next().unwrap().to_string();
            self.relay_db.add_pro_sub("sub_m", "monthly", "e", "created", &install).unwrap();
            let s = Synced {
                status: "active".into(),
                current_end: Some(ferrocast_relay::now_ms() + 30 * 86_400_000),
                cancel_at_end: None,
            };
            self.relay_db.sync_pro_sub(None, "sub_m", &s).unwrap();
        }
    }

    #[tokio::test]
    async fn the_weekly_email_comes_through_from_the_relay_with_its_errors() {
        let w = world(true).await;
        let (s, v) = w.call(Method::GET, None).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (402, Some("pro_required")), "{v}");
        w.make_pro();
        let (s, v) = w.call(Method::GET, None).await;
        assert_eq!((s.as_u16(), v["status"].as_str()), (200, Some("none")), "{v}");
        // The engine checks the body's shape; the relay checks the address.
        let (s, _) = w.call(Method::PUT, Some(json!({ "email": "a@b.co" }))).await;
        assert_eq!(s, 400);
        let (s, v) = w.call(Method::PUT, Some(json!({ "address": "nope" }))).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (422, Some("invalid_email")));
        assert!(v["error"]["hint"].as_str().unwrap().contains("typos"));
        let (s, v) = w.call(Method::PUT, Some(json!({ "address": "asha@gmail.com" }))).await;
        assert_eq!((s.as_u16(), v["status"].as_str()), (202, Some("pending")), "{v}");
        assert_eq!(v["address_hint"], "a•••@gmail.com");
        assert_eq!(w.fake.sent().len(), 1);
        let (s, v) = w.call(Method::GET, None).await;
        assert_eq!((s.as_u16(), v["status"].as_str()), (200, Some("pending")));
        // The relay's 429 keeps its Retry-After.
        for _ in 0..2 {
            assert_eq!(w.call(Method::PUT, Some(json!({ "address": "asha@gmail.com" }))).await.0, 202);
        }
        let res = w
            .http
            .put(format!("{}/api/v1/pro/history/email", w.base))
            .bearer_auth(&w.token)
            .json(&json!({ "address": "asha@gmail.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 429);
        assert!(res.headers().contains_key("retry-after"));
        let (s, _) = w.call(Method::DELETE, None).await;
        assert_eq!(s, 204);
        assert_eq!(w.call(Method::GET, None).await.1["status"], "none");
    }

    /// The relay's daily budget used up: its 503 `mail_busy` and hint come through.
    #[tokio::test]
    async fn the_relays_daily_mail_budget_comes_through_as_try_again_tomorrow() {
        let w = world_budget(true, 0).await;
        // The first call registers this install with the relay.
        assert_eq!(w.call(Method::GET, None).await.0, 402);
        w.make_pro();
        let (s, v) = w.call(Method::PUT, Some(json!({ "address": "asha@gmail.com" }))).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (503, Some("mail_busy")), "{v}");
        assert_eq!(v["error"]["hint"], "Try again tomorrow.");
        assert!(w.fake.sent().is_empty());
    }

    #[tokio::test]
    async fn the_relay_offline_is_a_502_with_a_hint() {
        let w = world(false).await;
        let (s, v) = w.call(Method::GET, None).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (502, Some("relay_unreachable")), "{v}");
        assert!(!v["error"]["hint"].as_str().unwrap_or("").is_empty());
    }
}
