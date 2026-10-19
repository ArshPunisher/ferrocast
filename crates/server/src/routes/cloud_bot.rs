//! The 24/7 cloud bot (Pro F1, API.md §9.7): the relay's routes passed through, plus this
//! PC's side (the pushed config's sync state).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::Value;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// `GET /pro/bot` → the relay's view with `pro_available` and `sync`.
pub async fn view(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.cloud_bot_view().await?))
}

/// `PUT /pro/bot/enabled {enabled}`: switching on pushes the config first.
pub async fn enabled(State(app): State<AppState>, Body(b): Body<Value>) -> ApiResult<Json<Value>> {
    let Some(on) = b.get("enabled").and_then(Value::as_bool) else {
        return Err(ApiError::invalid("enabled: true or false."));
    };
    Ok(Json(app.engine.cloud_bot_enable(on).await?))
}

/// `POST /pro/bot/{platform}/connect {account?, moderation?}` → 201 `{auth_url, expires_at}`;
/// `{retry: true}` tries a connected channel's error again → 200 with the view.
pub async fn connect(
    State(app): State<AppState>,
    Path(platform): Path<String>,
    Body(b): Body<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !b.is_object() {
        return Err(ApiError::invalid("The request must be a JSON object."));
    }
    match b.get("retry") {
        None | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => return Ok((StatusCode::OK, Json(app.engine.cloud_bot_retry(&platform).await?))),
        Some(_) => return Err(ApiError::invalid("retry: true or false.")),
    }
    Ok((StatusCode::CREATED, Json(app.engine.cloud_bot_connect(&platform, b).await?)))
}

/// `DELETE /pro/bot/{platform}` → 204: its sign-ins are revoked.
pub async fn disconnect(State(app): State<AppState>, Path(platform): Path<String>) -> ApiResult<StatusCode> {
    app.engine.cloud_bot_disconnect(&platform).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// End to end: dashboard API → engine → a real relay (cloud bot not set up there).
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrocast_engine::{Engine, PlatformApps};
    use ferrocast_relay::billing::store::Synced;
    use ferrocast_secrets::Secrets as _;
    use reqwest::Method;
    use serde_json::{Value, json};

    use crate::{ServeOptions, serve};

    #[tokio::test]
    async fn the_cloud_bot_comes_through_from_the_relay_with_its_errors() {
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
        tokio::spawn(async move { axum::serve(rl, rapp).await });
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
        let apps = PlatformApps { relay_url: Some(relay.clone()), ..Default::default() };
        let engine = Engine::start_with(dir.path(), secrets.clone(), apps).await.unwrap();
        let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
        let (base, token) = (server.base_url(), server.sessions.desktop_token().to_string());
        let http = reqwest::Client::new();
        let call = |m: Method, path: &str, body: Option<Value>| {
            let mut r = http.request(m, format!("{base}/api/v1{path}")).bearer_auth(&token);
            if let Some(b) = body {
                r = r.json(&b);
            }
            async move {
                let res = r.send().await.unwrap();
                let s = res.status().as_u16();
                (s, res.json::<Value>().await.unwrap_or(Value::Null))
            }
        };
        // No Pro: the relay's 402 with its hint.
        let (s, v) = call(Method::GET, "/pro/bot", None).await;
        assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")), "{v}");
        assert!(v["error"]["hint"].as_str().unwrap_or("").contains("Settings"), "{v}");
        let auth = secrets.get("relay-install").unwrap().expect("registered");
        let install = auth.split('.').next().unwrap().to_string();
        relay_db.add_pro_sub("sub_b", "monthly", "e", "created", &install).unwrap();
        let synced = Synced {
            status: "active".into(),
            current_end: Some(ferrocast_relay::now_ms() + 30 * 86_400_000),
            cancel_at_end: None,
        };
        relay_db.sync_pro_sub(None, "sub_b", &synced).unwrap();
        let (s, v) = call(Method::GET, "/pro/bot", None).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!((v["enabled"].as_bool(), v["duty"]["who"].as_str()), (Some(false), Some("off")));
        assert_eq!(v["available"], json!({ "twitch": false, "kick": false, "youtube": false }));
        assert_eq!(v["sync"]["state"], "off");
        assert!(v["pro_available"].is_boolean());
        // Not set up on that relay: 503 with the hint; unknown platforms never reach it.
        let (s, v) = call(Method::POST, "/pro/bot/twitch/connect", Some(json!({ "account": "bot" }))).await;
        assert_eq!((s, v["error"]["code"].as_str()), (503, Some("platform_unavailable")), "{v}");
        let (s, v) = call(Method::POST, "/pro/bot/tiktok/connect", Some(json!({}))).await;
        assert_eq!((s, v["error"]["code"].as_str()), (404, Some("platform_not_found")), "{v}");
        let (s, v) = call(Method::PUT, "/pro/bot/enabled", Some(json!({ "enabled": true }))).await;
        assert_eq!((s, v["error"]["code"].as_str()), (422, Some("no_platform")), "{v}");
        let (s, v) = call(Method::PUT, "/pro/bot/enabled", Some(json!({ "enabled": "yes" }))).await;
        assert_eq!(s, 400, "{v}");
        let (s, v) = call(Method::POST, "/pro/bot/twitch/connect", Some(json!({ "retry": "yes" }))).await;
        assert_eq!((s, v["error"]["code"].as_str()), (400, Some("invalid")), "{v}");
        let (s, v) = call(Method::POST, "/pro/bot/twitch/connect", Some(json!({ "retry": true }))).await;
        assert_eq!((s, v["error"]["code"].as_str()), (503, Some("platform_unavailable")), "the relay's answer: {v}");
        let (s, _) = call(Method::DELETE, "/pro/bot/kick", None).await;
        assert_eq!(s, 204);
    }
}
