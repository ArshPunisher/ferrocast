//! Pro smart features for the dashboard (API.md §4 "Smart features"): the settings,
//! approving the reply facts, the smart moderation log and its actions, the written recap,
//! and the Insights page's top supporters (local, not Pro).

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use ferrocast_engine::smart::{SmartPatch, Suggestion};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// `GET /pro/smart`.
pub async fn view(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.smart_view().await))
}

/// `PUT /pro/smart {moderation?, act_timeout_s?, rules?, replies?, about?, reply_gap_s?}`.
pub async fn save(State(app): State<AppState>, Body(b): Body<SmartPatch>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.smart_save(b).await?))
}

#[derive(Debug, Deserialize)]
pub struct ApproveIn {
    about: String,
}

/// `POST /pro/smart/about/approve {about}`.
pub async fn approve(State(app): State<AppState>, Body(b): Body<ApproveIn>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.smart_approve(&b.about).await?))
}

/// `GET /pro/smart/moderation` → `{items, replies}`.
pub async fn log(State(app): State<AppState>) -> Json<Value> {
    Json(app.engine.smart_log())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyIn {
    action: Option<String>,
    seconds: Option<u32>,
}

fn id_ok(id: &str) -> Result<(), ApiError> {
    if id.is_empty() || id.len() > 80 {
        return Err(ApiError::invalid("That suggestion id isn't right.").hint("Reload the page."));
    }
    Ok(())
}

/// `POST /pro/smart/moderation/{id}/apply {action?, seconds?}`.
pub async fn apply(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Suggestion>> {
    id_ok(&id)?;
    // An empty body does what the suggestion says.
    let b: ApplyIn = if body.iter().all(u8::is_ascii_whitespace) {
        ApplyIn::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::invalid(format!("A field is missing or wrong: {e}")))?
    };
    if let Some(s) = b.seconds
        && !(1..=1_209_600).contains(&s)
    {
        return Err(ApiError::invalid("seconds: 1 second to 14 days."));
    }
    Ok(Json(app.engine.smart_apply(&id, b.action.as_deref(), b.seconds).await?))
}

/// `POST /pro/smart/moderation/{id}/undo`.
pub async fn undo(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Suggestion>> {
    id_ok(&id)?;
    Ok(Json(app.engine.smart_undo(&id).await?))
}

/// `POST /pro/smart/moderation/{id}/dismiss`.
pub async fn dismiss(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Suggestion>> {
    id_ok(&id)?;
    Ok(Json(app.engine.smart_dismiss(&id)?))
}

fn since(q: &HashMap<String, String>, key: &str) -> Result<Option<i64>, ApiError> {
    match q.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(raw) => {
            let now = ferrocast_engine::now().0;
            raw.parse::<i64>()
                .ok()
                .filter(|v| (0..=now + 60_000).contains(v))
                .map(Some)
                .ok_or_else(|| ApiError::invalid(format!("{key}: a time in Unix milliseconds, not in the future.")))
        }
    }
}

/// `GET /recap/summary` → the last written recap.
pub async fn recap_saved(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.smart_recap_saved().await?))
}

/// `POST /recap/summary?since=` → `{started_at, summary, at}`.
pub async fn recap(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.smart_recap(since(&q, "since")?).await?))
}

/// `GET /insights/supporters?from&to` (from defaults to 30 days ago).
pub async fn supporters(
    State(app): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let now = ferrocast_engine::now().0;
    let from = since(&q, "from")?.unwrap_or(now - 30 * 86_400_000);
    let to = since(&q, "to")?;
    if to.is_some_and(|t| t <= from) {
        return Err(ApiError::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_range",
            "from must be before to.",
        )
        .hint("Pick a range again."));
    }
    if to.unwrap_or(now) - from > 400 * 86_400_000 {
        return Err(ApiError::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_range",
            "A range can be up to 400 days.",
        )
        .hint("Pick a shorter range."));
    }
    let v = app.engine.supporters(from, to).await?;
    Ok(Json(json!(v)))
}

/// End to end: dashboard API → engine → a real relay with a fake model.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrocast_engine::{Engine, PlatformApps};
    use ferrocast_relay::billing::store::Synced;
    use ferrocast_relay::smart::{FakeSmart, SmartProvider};
    use ferrocast_secrets::Secrets;
    use reqwest::StatusCode;
    use serde_json::{Value, json};

    use crate::{ServeOptions, serve};

    struct World {
        base: String,
        token: String,
        http: reqwest::Client,
        relay_db: Arc<ferrocast_relay::Db>,
        secrets: Arc<ferrocast_secrets::MemorySecrets>,
        engine: Engine,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    async fn world() -> World {
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
        let fake: Arc<dyn SmartProvider> = Arc::new(FakeSmart::answering("A calm two-hour stream."));
        let state = ferrocast_relay::AppState::new(cfg, ferrocast_relay::Db::open(&rdir.path().join("r.db")).unwrap())
            .with_smart(Some(fake), 100);
        let relay_db = state.db.clone();
        let rapp = ferrocast_relay::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(rl, rapp).await });
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
            engine,
            _dirs: (dir, rdir),
        }
    }

    impl World {
        async fn send(&self, m: reqwest::Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
            let mut r = self.http.request(m, format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token);
            if let Some(b) = body {
                r = r.json(&b);
            }
            let res = r.send().await.unwrap();
            let s = res.status();
            (s, res.json().await.unwrap_or(Value::Null))
        }

        async fn make_pro(&self) {
            // Registers the install with the relay first.
            let _ = self.send(reqwest::Method::POST, "/pro/refresh", None).await;
            let auth = self.secrets.get("relay-install").unwrap().expect("registered with the relay");
            let install = auth.split('.').next().unwrap().to_string();
            self.relay_db.add_pro_sub("sub_s", "monthly", "e", "created", &install).unwrap();
            let s = Synced {
                status: "active".into(),
                current_end: Some(ferrocast_relay::now_ms() + 30 * 86_400_000),
                cancel_at_end: None,
            };
            self.relay_db.sync_pro_sub(None, "sub_s", &s).unwrap();
            let _ = self.send(reqwest::Method::POST, "/pro/refresh", None).await;
        }
    }

    #[tokio::test]
    async fn smart_routes_check_input_and_pass_the_relays_answers() {
        let w = world().await;
        let (s, v) = w.send(reqwest::Method::GET, "/pro/smart", None).await;
        assert_eq!((s, v["active"].as_bool()), (StatusCode::OK, Some(false)), "{v}");
        let (s, v) = w.send(reqwest::Method::PUT, "/pro/smart", Some(json!({ "act_timeout_s": 2 }))).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (400, Some("invalid")), "{v}");
        let (s, _) = w.send(reqwest::Method::PUT, "/pro/smart", Some(json!({ "nonsense": 1 }))).await;
        assert_eq!(s.as_u16(), 400, "unknown fields are refused");
        let (s, v) = w
            .send(reqwest::Method::PUT, "/pro/smart", Some(json!({ "moderation": "suggest", "about": "Live at 8pm" })))
            .await;
        assert_eq!((s, v["settings"]["moderation"].as_str()), (StatusCode::OK, Some("suggest")), "{v}");
        let (s, v) =
            w.send(reqwest::Method::POST, "/pro/smart/about/approve", Some(json!({ "about": "Live at 9pm" }))).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (409, Some("about_changed")), "{v}");
        let (s, v) =
            w.send(reqwest::Method::POST, "/pro/smart/about/approve", Some(json!({ "about": "Live at 8pm" }))).await;
        assert_eq!((s, v["about_approved"].as_bool()), (StatusCode::OK, Some(true)), "{v}");
        let (s, v) = w.send(reqwest::Method::POST, "/pro/smart/moderation/nope/undo", None).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (404, Some("not_found")), "{v}");
        let (s, v) = w.send(reqwest::Method::GET, "/pro/smart/moderation", None).await;
        assert_eq!((s, v["items"].as_array().map(Vec::len)), (StatusCode::OK, Some(0)));
        // The recap without Pro: 402 with the words; with Pro and no stream: 409.
        let (s, v) = w.send(reqwest::Method::POST, "/recap/summary", None).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (402, Some("pro_required")), "{v}");
        w.make_pro().await;
        let (_, v) = w.send(reqwest::Method::GET, "/pro/smart", None).await;
        assert_eq!(v["active"], true, "{v}");
        let (s, v) = w.send(reqwest::Method::POST, "/recap/summary", None).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (409, Some("no_stream")), "{v}");
        let (s, v) = w.send(reqwest::Method::POST, "/recap/summary?since=abc", None).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (400, Some("invalid")), "{v}");
        let (s, v) = w.send(reqwest::Method::GET, "/recap/summary", None).await;
        assert_eq!((s, v["summary"].clone()), (StatusCode::OK, Value::Null));
        let _ = &w.engine;
    }

    #[tokio::test]
    async fn supporters_come_from_this_pc_and_ranges_are_checked() {
        let w = world().await;
        let (s, v) = w.send(reqwest::Method::GET, "/insights/supporters", None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert!(v["subscribers"].is_array() && v["tippers"].is_array() && v["top_chatters"].is_array(), "{v}");
        let now = ferrocast_engine::now().0;
        for q in [format!("from={now}&to={}", now - 1), format!("from={}", now - 500 * 86_400_000)] {
            let (s, v) = w.send(reqwest::Method::GET, &format!("/insights/supporters?{q}"), None).await;
            assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (422, Some("invalid_range")), "{q}: {v}");
        }
        let (s, _) = w.send(reqwest::Method::GET, "/insights/supporters?from=soon", None).await;
        assert_eq!(s.as_u16(), 400);
    }
}
