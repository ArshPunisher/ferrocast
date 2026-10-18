//! Pro cloud backups for Settings › Your data (API.md §9.3). The engine encrypts on this PC
//! and talks to the relay; these routes check the body and pass the answer (or the relay's
//! error, with its status) through. The recovery key only ever goes to this dashboard.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::Value;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// `GET /pro/backups` → this PC's key and upload state plus the relay's list.
pub async fn list(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.cloud_backups().await?))
}

/// `POST /pro/backups`: back up now (waits for the upload) → 201 with the new list.
pub async fn now(State(app): State<AppState>) -> ApiResult<(StatusCode, Json<Value>)> {
    Ok((StatusCode::CREATED, Json(app.engine.cloud_backup_now().await?)))
}

/// `GET /pro/backups/key`: the recovery key file (made the first time).
pub async fn key(State(app): State<AppState>) -> ApiResult<Response> {
    let text = app.engine.cloud_backup_key().await?;
    let disposition = format!("attachment; filename=\"{}\"", ferrocast_engine::CLOUD_BACKUP_KEY_FILE);
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        text,
    )
        .into_response())
}

/// `POST /pro/backups/key/saved`: the streamer saved the key; uploads may start.
pub async fn key_saved(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.cloud_backup_key_saved().await?))
}

#[derive(Debug, Deserialize)]
pub struct Passphrase {
    passphrase: String,
}

/// `PUT /pro/backups/passphrase {passphrase}` (12 characters or more).
pub async fn set_passphrase(State(app): State<AppState>, Body(b): Body<Passphrase>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.cloud_backup_passphrase(Some(b.passphrase)).await?))
}

/// `DELETE /pro/backups/passphrase`: new backups open with the key file only.
pub async fn clear_passphrase(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.cloud_backup_passphrase(None).await?))
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    auto: bool,
}

/// `PUT /pro/backups/settings {auto}`.
pub async fn settings(State(app): State<AppState>, Body(b): Body<Settings>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.cloud_backup_auto(b.auto).await?))
}

#[derive(Debug, Default, Deserialize)]
pub struct Restore {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    passphrase: Option<String>,
}

/// `POST /pro/backups/{id}/restore {key?, passphrase?}` → the import report.
pub async fn restore(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(b): Body<Restore>,
) -> ApiResult<Json<Value>> {
    if b.key.as_ref().is_some_and(|k| k.len() > 4096) || b.passphrase.as_ref().is_some_and(|p| p.len() > 1024) {
        return Err(ApiError::invalid("That key or passphrase is far too long."));
    }
    Ok(Json(app.engine.cloud_backup_restore(&id, b.key, b.passphrase).await?))
}

/// `DELETE /pro/backups/{id}` → 204.
pub async fn remove(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.cloud_backup_delete(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// End to end: dashboard API → engine → a real relay with a file store.
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
        let blobs = ferrocast_relay::blob::FsBlobStore::new(rdir.path().join("blobs")).unwrap();
        let state = ferrocast_relay::AppState::new(cfg, ferrocast_relay::Db::open(&rdir.path().join("r.db")).unwrap())
            .with_blobs(Some(Arc::new(blobs)));
        let relay_db = state.db.clone();
        let rapp = ferrocast_relay::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(rl, rapp).await });
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
        let apps = PlatformApps { relay_url: Some(relay), ..Default::default() };
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
        async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
            let mut r = self.http.request(method, format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token);
            if let Some(b) = body {
                r = r.json(&b);
            }
            let res = r.send().await.unwrap();
            let s = res.status();
            (s, res.json().await.unwrap_or(Value::Null))
        }

        async fn make_pro(&self) {
            // Registers with the relay (any Pro call does), then gives that install Pro.
            let _ = self.call(reqwest::Method::GET, "/pro/backups", None).await;
            let auth = self.secrets.get("relay-install").unwrap().expect("registered with the relay");
            let install = auth.split('.').next().unwrap().to_string();
            self.relay_db.add_pro_sub("sub_b", "monthly", "e", "created", &install).unwrap();
            let s = Synced {
                status: "active".into(),
                current_end: Some(ferrocast_relay::now_ms() + 30 * 86_400_000),
                cancel_at_end: None,
            };
            self.relay_db.sync_pro_sub(None, "sub_b", &s).unwrap();
            // Automatic uploads stay out of the test's way.
            let (s, _) = self.call(reqwest::Method::PUT, "/pro/backups/settings", Some(json!({ "auto": false }))).await;
            assert_eq!(s, 200);
        }
    }

    #[tokio::test]
    async fn back_up_restore_and_delete_through_the_dashboard_api() {
        let w = world().await;
        // No Pro: the relay's 402 and its words come through.
        let (s, v) = w.call(reqwest::Method::GET, "/pro/backups", None).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (402, Some("pro_required")), "{v}");
        w.make_pro().await;
        let (s, v) = w.call(reqwest::Method::GET, "/pro/backups", None).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["backups"], json!([]));
        assert_eq!(v["key"]["exists"], false);
        assert_eq!(v["limits"]["bytes"], 250_000_000);
        // Before the key is saved: 409 with what to do.
        let (s, v) = w.call(reqwest::Method::POST, "/pro/backups", None).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (409, Some("key_not_saved")), "{v}");
        let res = w.http.get(format!("{}/api/v1/pro/backups/key", w.base)).bearer_auth(&w.token).send().await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["cache-control"], "no-store");
        assert!(res.headers()["content-disposition"].to_str().unwrap().contains("ferrocast-recovery-key.txt"));
        let key = res.text().await.unwrap();
        assert!(key.contains("AGE-SECRET-KEY-1"));
        assert_eq!(w.call(reqwest::Method::POST, "/pro/backups/key/saved", None).await.0, 200);
        // Passphrase: too short is a 400 naming the field; a good one sticks; it can go again.
        let (s, v) =
            w.call(reqwest::Method::PUT, "/pro/backups/passphrase", Some(json!({ "passphrase": "short" }))).await;
        assert_eq!(s, 400);
        assert!(v["error"]["message"].as_str().unwrap().contains("passphrase"), "{v}");
        let (s, v) = w
            .call(
                reqwest::Method::PUT,
                "/pro/backups/passphrase",
                Some(json!({ "passphrase": "a long enough phrase" })),
            )
            .await;
        assert_eq!((s.as_u16(), v["key"]["passphrase"].as_bool()), (200, Some(true)), "{v}");
        let (_, v) = w.call(reqwest::Method::DELETE, "/pro/backups/passphrase", None).await;
        assert_eq!(v["key"]["passphrase"], false);
        // Back up now.
        w.engine.db(|s| ferrocast_store::repo::bot::bump_counter(s, "deaths", 4)).await.unwrap();
        let (s, v) = w.call(reqwest::Method::POST, "/pro/backups", None).await;
        assert_eq!(s, 201, "{v}");
        let id = v["backups"][0]["id"].as_str().unwrap().to_string();
        // Restore puts the data back.
        w.engine.db(|s| ferrocast_store::repo::bot::bump_counter(s, "deaths", 10)).await.unwrap();
        let (s, v) = w.call(reqwest::Method::POST, &format!("/pro/backups/{id}/restore"), Some(json!({}))).await;
        assert_eq!(s, 200, "{v}");
        assert!(v["imported"].is_object());
        assert_eq!(w.engine.db(|s| ferrocast_store::repo::bot::counter(s, "deaths")).await.unwrap(), 4);
        // A pasted key that isn't one: 400. An unknown id: 404.
        let (s, _) =
            w.call(reqwest::Method::POST, &format!("/pro/backups/{id}/restore"), Some(json!({ "key": "nope" }))).await;
        assert_eq!(s, 400);
        let (s, _) = w.call(reqwest::Method::DELETE, "/pro/backups/bk_000000000000000000000000", None).await;
        assert_eq!(s, 404);
        let (s, _) = w.call(reqwest::Method::DELETE, &format!("/pro/backups/{id}"), None).await;
        assert_eq!(s, 204);
        let (_, v) = w.call(reqwest::Method::GET, "/pro/backups", None).await;
        assert_eq!(v["backups"], json!([]));
    }
}
