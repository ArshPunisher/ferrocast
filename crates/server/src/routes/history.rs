//! Pro stream history for the dashboard's `#/history` (API.md §9.1). The engine reads it
//! from the relay; these routes check the query and pass the answer (or the relay's error,
//! with its status) through.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use ferrocast_core::event::Platform;
use serde_json::Value;

use crate::AppState;
use crate::error::{ApiError, ApiResult};

type Q = HashMap<String, String>;
/// `from`, `to`, `platform`.
type Filter = (Option<i64>, Option<i64>, Option<Platform>);

fn ms(q: &Q, key: &str) -> Result<Option<i64>, ApiError> {
    match q.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(v) => v
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .map(Some)
            .ok_or_else(|| ApiError::invalid(format!("{key}: a time in milliseconds."))),
    }
}

fn filter(q: &Q) -> Result<Filter, ApiError> {
    let platform = match q.get("platform").map(String::as_str).filter(|s| !s.is_empty()) {
        None => None,
        Some(p) => Some(super::platform(p)?),
    };
    Ok((ms(q, "from")?, ms(q, "to")?, platform))
}

/// `GET /pro/history?from&to&platform` → `{from, to, streams, truncated, weekly}`.
pub async fn list(State(app): State<AppState>, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    let (from, to, platform) = filter(&q)?;
    Ok(Json(app.engine.stream_history(from, to, platform).await?))
}

/// `GET /pro/history/samples?platform&key` → `{platform, key, samples: [{at, viewers}]}`.
pub async fn samples(State(app): State<AppState>, Query(q): Query<Q>) -> ApiResult<Json<Value>> {
    let platform = match q.get("platform").map(String::as_str).filter(|s| !s.is_empty()) {
        Some(p) => super::platform(p)?,
        None => return Err(ApiError::invalid("platform: twitch, youtube or kick.")),
    };
    let key = q.get("key").map(|k| k.trim()).filter(|k| !k.is_empty() && k.len() <= 64);
    let key = key.ok_or_else(|| ApiError::invalid("key: the stream's key from the list."))?;
    Ok(Json(app.engine.stream_history_samples(platform, key).await?))
}

/// `GET /pro/history/export.csv?from&to&platform`: the CSV to save.
pub async fn export(State(app): State<AppState>, Query(q): Query<Q>) -> ApiResult<Response> {
    let (from, to, platform) = filter(&q)?;
    let body = app.engine.stream_history_csv(from, to, platform).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"ferrocast-history.csv\""),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}

/// End to end: dashboard API → engine → a real relay.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrocast_engine::{Engine, PlatformApps};
    use ferrocast_relay::billing::store::Synced;
    use ferrocast_secrets::Secrets;
    use reqwest::StatusCode;
    use serde_json::{Value, json};

    use crate::{ServeOptions, serve};

    struct World {
        base: String,
        relay: String,
        token: String,
        http: reqwest::Client,
        relay_db: Arc<ferrocast_relay::Db>,
        secrets: Arc<ferrocast_secrets::MemorySecrets>,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
        _engine: Engine,
    }

    async fn world(relay_up: bool) -> World {
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
        let state = ferrocast_relay::AppState::new(cfg, ferrocast_relay::Db::open(&rdir.path().join("r.db")).unwrap());
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
            relay,
            token: server.sessions.desktop_token().to_string(),
            http: reqwest::Client::new(),
            relay_db,
            secrets,
            _dirs: (dir, rdir),
            _engine: engine,
        }
    }

    impl World {
        async fn get(&self, path: &str) -> reqwest::Response {
            self.http.get(format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token).send().await.unwrap()
        }

        async fn json(&self, path: &str) -> (StatusCode, Value) {
            let res = self.get(path).await;
            let s = res.status();
            (s, res.json().await.unwrap_or(Value::Null))
        }

        /// Gives the engine's relay install an active subscription.
        fn make_pro(&self) -> String {
            let auth = self.secrets.get("relay-install").unwrap().expect("registered with the relay");
            let install = auth.split('.').next().unwrap().to_string();
            self.relay_db.add_pro_sub("sub_h", "monthly", "e", "created", &install).unwrap();
            let s = Synced {
                status: "active".into(),
                current_end: Some(ferrocast_relay::now_ms() + 30 * 86_400_000),
                cancel_at_end: None,
            };
            self.relay_db.sync_pro_sub(None, "sub_h", &s).unwrap();
            auth
        }
    }

    #[tokio::test]
    async fn history_comes_through_from_the_relay_with_its_errors() {
        let w = world(true).await;
        // No Pro and no account yet: the relay's 403 and its words reach the dashboard.
        let (s, v) = w.json("/pro/history").await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (403, Some("account_required")), "{v}");
        assert!(v["error"]["hint"].as_str().unwrap().contains("Settings › Account"));
        let auth = w.make_pro();
        let start = ferrocast_relay::now_ms() - 2 * 3_600_000;
        let res = w
            .http
            .post(format!("{}/v1/pro/history/streams", w.relay))
            .bearer_auth(&auth)
            .json(&json!({ "platform": "kick", "key": "kick:1", "started_at": start, "ended_at": start + 3_600_000,
                           "title": "=cmd", "peak_viewers": 12, "avg_viewers": 8 }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 201);
        let (s, v) = w.json("/pro/history").await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["streams"][0]["platform"], "kick");
        assert!(v["weekly"].as_array().unwrap().len() >= 4);
        let (s, v) = w.json("/pro/history?platform=twitch").await;
        assert_eq!((s.as_u16(), v["streams"].as_array().map(Vec::len)), (200, Some(0)));
        assert_eq!(v["plan"], "pro");
        // One stream's samples come through, and a missing one is the relay's 404.
        let (s, v) = w.json("/pro/history/samples?platform=kick&key=kick:1").await;
        assert_eq!((s.as_u16(), v["samples"].as_array().map(Vec::len)), (200, Some(0)), "{v}");
        let (s, v) = w.json("/pro/history/samples?platform=kick&key=kick:404").await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (404, Some("stream_not_found")), "{v}");
        assert_eq!(w.json("/pro/history/samples?platform=kick").await.0, 400);
        let csv = w.get("/pro/history/export.csv").await;
        assert_eq!(csv.status(), 200);
        assert!(csv.headers()["content-type"].to_str().unwrap().starts_with("text/csv"));
        assert!(csv.headers()["content-disposition"].to_str().unwrap().contains("ferrocast-history.csv"));
        let text = csv.text().await.unwrap();
        assert!(text.starts_with("platform,started_utc"));
        assert!(text.contains("\"'=cmd\""), "{text}");
        // Checked here first, then by the relay.
        for q in ["from=abc", "to=-5", "platform=tiktok"] {
            assert_eq!(w.json(&format!("/pro/history?{q}")).await.0, 400, "{q}");
            assert_eq!(w.get(&format!("/pro/history/export.csv?{q}")).await.status(), 400, "{q}");
        }
        let (s, v) = w.json("/pro/history?from=10&to=5").await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (422, Some("invalid_range")), "{v}");
    }

    #[tokio::test]
    async fn with_the_relay_down_history_says_so() {
        let w = world(false).await;
        let (s, v) = w.json("/pro/history").await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (502, Some("relay_unreachable")), "{v}");
        let res = w.get("/pro/history/export.csv").await;
        assert_eq!(res.status(), 502);
    }
}
