//! Pro tip page branding and goal for the Tips screen (API.md §9.2). The engine passes each
//! call to the relay, which checks Pro and does the work; uploads are checked here first
//! (type, size) so a wrong file is refused at once, even offline.

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use serde_json::Value;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// The largest image: banners (avatars are 1 MB). The route's body limit sits just above.
pub const MAX_IMAGE: usize = 3_000_000;
pub const MAX_IMAGE_BODY: usize = MAX_IMAGE + 64 * 1024;

fn limit(kind: &str) -> Result<(usize, &'static str), ApiError> {
    match kind {
        "avatar" => Ok((1_000_000, "Avatars can be up to 1 MB.")),
        "banner" => Ok((MAX_IMAGE, "Banners can be up to 3 MB.")),
        _ => Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", "There's no such tip page image.")
            .hint("Use avatar or banner.")),
    }
}

fn object(v: &Value, shape: &str) -> ApiResult<()> {
    if v.is_object() {
        Ok(())
    } else {
        Err(ApiError::invalid("The body must be a JSON object.").hint(shape.to_string()))
    }
}

/// `GET /tips/brand`
pub async fn brand(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.tip_brand().await?))
}

/// `PUT /tips/brand {accent, background, text}` → the saved colours and any contrast fixes.
pub async fn save_brand(State(app): State<AppState>, Body(b): Body<Value>) -> ApiResult<Json<Value>> {
    object(&b, "Send { accent, background, text } as hex colours.")?;
    Ok(Json(app.engine.save_tip_brand(b).await?))
}

/// `DELETE /tips/brand`: the default colours again.
pub async fn reset_brand(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.engine.reset_tip_brand().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /tips/brand/{avatar|banner}`: the raw image with its `Content-Type`.
pub async fn save_image(
    State(app): State<AppState>,
    Path(kind): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<Json<Value>> {
    let (max, too_big) = limit(&kind)?;
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let ct = match ct.as_str() {
        "image/png" | "image/webp" => ct,
        "image/jpeg" | "image/jpg" => "image/jpeg".to_string(),
        _ => {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_type",
                "Tip page images can be PNG, JPEG or WebP.",
            )
            .hint("Save it as PNG, JPEG or WebP and try again (GIF, SVG and HEIC aren't accepted)."));
        }
    };
    let too_large = || {
        ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", too_big)
            .hint("Make the file smaller (or export it as JPEG) and try again.")
    };
    let bytes = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            too_large()
        } else {
            ApiError::invalid("The upload couldn't be read.")
        }
    })?;
    if bytes.len() > max {
        return Err(too_large());
    }
    if bytes.is_empty() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "image_unreadable", "The file is empty."));
    }
    Ok(Json(app.engine.save_tip_image(&kind, &ct, bytes.to_vec()).await?))
}

/// `DELETE /tips/brand/{avatar|banner}`
pub async fn delete_image(State(app): State<AppState>, Path(kind): Path<String>) -> ApiResult<StatusCode> {
    limit(&kind)?;
    app.engine.delete_tip_image(&kind).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /tips/goal` (404 `not_found` without one).
pub async fn goal(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.tip_goal().await?))
}

/// `PUT /tips/goal {title, target, currency, ends_at?}`
pub async fn save_goal(State(app): State<AppState>, Body(b): Body<Value>) -> ApiResult<Json<Value>> {
    object(&b, "Send { title, target, currency, ends_at? }.")?;
    Ok(Json(app.engine.save_tip_goal(b).await?))
}

/// `DELETE /tips/goal`
pub async fn delete_goal(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.engine.delete_tip_goal().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /pro/tips/domain`: the custom address and its status (404 `not_found` without one).
pub async fn domain(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.tip_domain().await?))
}

/// `PUT /pro/tips/domain {hostname}` → 201 with the DNS records to add, 200 when it's the
/// address the tip page already has, or 409 `domain_taken` with `proof` (the relay's status
/// and record, passed on).
pub async fn save_domain(State(app): State<AppState>, Body(b): Body<Value>) -> ApiResult<(StatusCode, Json<Value>)> {
    if !b["hostname"].is_string() {
        return Err(
            ApiError::invalid("Send { hostname }.").hint("For example { \"hostname\": \"tips.yourchannel.com\" }.")
        );
    }
    let (status, v) = app.engine.save_tip_domain(b).await?;
    // 409 here is `domain_taken` with the TXT record to add (`proof`), in the usual error shape.
    let status = match status {
        201 => StatusCode::CREATED,
        409 => StatusCode::CONFLICT,
        _ => StatusCode::OK,
    };
    Ok((status, Json(v)))
}

/// `DELETE /pro/tips/domain`
pub async fn delete_domain(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.engine.delete_tip_domain().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// End to end: dashboard API → engine → a real relay (with a file store).
#[cfg(test)]
mod tests {
    use ferrocast_engine::{Engine, PlatformApps};
    use ferrocast_relay::billing::store::Synced;
    use ferrocast_secrets::Secrets;
    use reqwest::Method as M;
    use serde_json::{Value, json};

    use crate::{ServeOptions, serve};

    /// A 4 × 4 orange PNG.
    const PNG: [u8; 73] = [
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 4, 0, 0, 0, 4, 8, 2, 0, 0, 0, 38, 147,
        9, 41, 0, 0, 0, 16, 73, 68, 65, 84, 120, 156, 99, 248, 223, 192, 0, 71, 12, 196, 113, 0, 126, 162, 23, 241, 93,
        161, 96, 5, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    struct World {
        base: String,
        token: String,
        http: reqwest::Client,
        relay: ferrocast_relay::AppState,
        cf: std::sync::Arc<ferrocast_relay::tips::domains::FakeHostnames>,
        secrets: std::sync::Arc<ferrocast_secrets::MemorySecrets>,
        _keep: (Engine, tempfile::TempDir, tempfile::TempDir),
    }

    async fn world(relay_up: bool) -> World {
        let rdir = tempfile::tempdir().unwrap();
        let rl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", rl.local_addr().unwrap());
        let cfg = ferrocast_relay::Config {
            public_url: url.clone(),
            tip_url: None,
            kick_client_id: None,
            kick_client_secret: None,
            kick_id_url: url.clone(),
            kick_api_url: url.clone(),
        };
        let blobs = ferrocast_relay::blob::FsBlobStore::new(rdir.path().join("blobs")).unwrap();
        let cf = std::sync::Arc::new(ferrocast_relay::tips::domains::FakeHostnames::new());
        let relay = ferrocast_relay::AppState::new(cfg, ferrocast_relay::Db::open(&rdir.path().join("r.db")).unwrap())
            .with_blobs(Some(std::sync::Arc::new(blobs)))
            .with_tip_domains(
                Some(cf.clone() as std::sync::Arc<dyn ferrocast_relay::tips::domains::CustomHostnames>),
                Some(std::sync::Arc::new(ferrocast_relay::tips::domains::FakeDns::new())),
                Some(&[7u8; 32]),
                100,
            );
        let app = ferrocast_relay::router(relay.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
        if relay_up {
            tokio::spawn(async move { axum::serve(rl, app).await });
        } else {
            drop(rl);
        }
        let dir = tempfile::tempdir().unwrap();
        let secrets = std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default());
        let apps = PlatformApps { relay_url: Some(url), ..Default::default() };
        let engine = Engine::start_with(dir.path(), secrets.clone(), apps).await.unwrap();
        let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
        World {
            base: server.base_url(),
            token: server.sessions.desktop_token().to_string(),
            http: reqwest::Client::new(),
            relay,
            cf,
            secrets,
            _keep: (engine, dir, rdir),
        }
    }

    impl World {
        async fn call(&self, m: M, path: &str, body: Option<Value>) -> (u16, Value) {
            let mut r = self.http.request(m, format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token);
            if let Some(b) = body {
                r = r.json(&b);
            }
            let res = r.send().await.unwrap();
            let s = res.status().as_u16();
            (s, res.json().await.unwrap_or(Value::Null))
        }

        async fn upload(&self, kind: &str, ct: &str, bytes: Vec<u8>) -> (u16, Value) {
            let res = self
                .http
                .put(format!("{}/api/v1/tips/brand/{kind}", self.base))
                .bearer_auth(&self.token)
                .header("content-type", ct)
                .body(bytes)
                .send()
                .await
                .unwrap();
            let s = res.status().as_u16();
            (s, res.json().await.unwrap_or(Value::Null))
        }

        /// Makes the engine's relay install Pro, straight in the relay's database.
        fn make_pro(&self) {
            let auth = self.secrets.get("relay-install").unwrap().expect("registered with the relay");
            let install = auth.split('.').next().unwrap();
            self.relay.db.add_pro_sub("sub_e2e", "monthly", "h", "active", install).unwrap();
            let s = Synced {
                status: "active".into(),
                current_end: Some(ferrocast_relay::now_ms() + 30 * 86_400_000),
                cancel_at_end: Some(false),
            };
            self.relay.db.sync_pro_sub(None, "sub_e2e", &s).unwrap();
        }
    }

    #[tokio::test]
    async fn branding_and_the_goal_go_through_to_the_relay() {
        let w = world(true).await;
        // Without Pro: the relay's 402 comes through.
        let (s, v) = w.call(M::GET, "/tips/brand", None).await;
        assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")), "{v}");
        // Wrong files are refused here first, with the limit or the allowed types.
        let (s, v) = w.upload("avatar", "image/gif", PNG.to_vec()).await;
        assert_eq!((s, v["error"]["code"].as_str()), (415, Some("unsupported_type")));
        let (s, v) = w.upload("avatar", "image/png", vec![0; 1_000_001]).await;
        assert_eq!((s, v["error"]["message"].as_str()), (413, Some("Avatars can be up to 1 MB.")));
        let (s, v) = w.upload("banner", "image/png", vec![0; 3_100_000]).await;
        assert_eq!(
            (s, v["error"]["message"].as_str()),
            (413, Some("Banners can be up to 3 MB.")),
            "over the route limit"
        );
        assert_eq!(w.upload("logo", "image/png", PNG.to_vec()).await.0, 404);
        assert_eq!(w.call(M::PUT, "/tips/brand", Some(json!([1]))).await.0, 400);

        w.make_pro();
        let (s, v) = w
            .call(M::PUT, "/tips/brand", Some(json!({ "accent": "#333", "background": "#222222", "text": "#eeeeee" })))
            .await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["changes"][0]["field"], "accent", "the dark accent was lifted: {v}");
        let (s, v) = w.upload("avatar", "image/png", PNG.to_vec()).await;
        assert_eq!(s, 200, "{v}");
        assert!(v["avatar_url"].as_str().unwrap().contains("/t/a/"));
        let (s, v) = w.upload("banner", "image/png", b"not an image".to_vec()).await;
        assert_eq!((s, v["error"]["code"].as_str()), (415, Some("unsupported_type")), "the relay sniffs: {v}");
        assert_eq!(w.call(M::DELETE, "/tips/brand/avatar", None).await.0, 204);
        assert_eq!(w.call(M::DELETE, "/tips/brand", None).await.0, 204);

        // The goal needs a tip page; then it saves and comes back with its links.
        let goal = json!({ "title": "New mic", "target": 500_000, "currency": "INR" });
        let (s, v) = w.call(M::PUT, "/tips/goal", Some(goal.clone())).await;
        assert_eq!((s, v["error"]["code"].as_str()), (422, Some("no_tip_page")), "{v}");
        let tips = json!({ "handle": "novae2e", "display_name": "Nova", "message": "",
                           "razorpay": { "page_url": "https://razorpay.me/@nova", "secret": "rzp-secret-123" } });
        assert_eq!(w.call(M::PUT, "/tips", Some(tips)).await.0, 200);
        let (s, v) = w.call(M::PUT, "/tips/goal", Some(goal)).await;
        assert_eq!(s, 200, "{v}");
        assert!(v["page_url"].as_str().unwrap().ends_with("/t/novae2e/goal"));
        assert_eq!(w.call(M::GET, "/tips/goal", None).await.1["raised"], 0);
        assert_eq!(w.call(M::DELETE, "/tips/goal", None).await.0, 204);
        let (s, v) = w.call(M::GET, "/tips/goal", None).await;
        assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")));
    }

    #[tokio::test]
    async fn the_own_address_goes_through_to_the_relay() {
        let w = world(true).await;
        let host = json!({ "hostname": "tips.mychannel.com" });
        let (s, v) = w.call(M::GET, "/pro/tips/domain", None).await;
        assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")), "{v}");
        assert_eq!(w.call(M::PUT, "/pro/tips/domain", Some(json!({ "host": 1 }))).await.0, 400);
        w.make_pro();
        let (s, v) = w.call(M::PUT, "/pro/tips/domain", Some(host.clone())).await;
        assert_eq!((s, v["error"]["code"].as_str()), (422, Some("no_tip_page")), "{v}");
        let tips = json!({ "handle": "novadomain", "display_name": "Nova", "message": "",
                           "razorpay": { "page_url": "https://razorpay.me/@nova", "secret": "rzp-secret-123" } });
        assert_eq!(w.call(M::PUT, "/tips", Some(tips)).await.0, 200);
        let (s, v) = w.call(M::PUT, "/pro/tips/domain", Some(json!({ "hostname": "mychannel.com" }))).await;
        assert_eq!((s, v["error"]["code"].as_str()), (422, Some("apex_not_supported")), "{v}");
        assert!(v["error"]["hint"].is_string());
        let (s, v) = w.call(M::PUT, "/pro/tips/domain", Some(host.clone())).await;
        assert_eq!(s, 201, "{v}");
        assert_eq!(v["dns"][0]["type"], "CNAME");
        assert_eq!(v["proof"]["type"], "TXT");
        assert_eq!(v["proof"]["name"], "_ferrocast.tips.mychannel.com");
        assert_eq!(v["status"], "pending");
        // Finding 11: the same address again is the relay's 200, passed on (not a second 201).
        let (s, v) = w.call(M::PUT, "/pro/tips/domain", Some(host)).await;
        assert_eq!((s, v["hostname"].as_str()), (200, Some("tips.mychannel.com")), "{v}");
        let (s, v) = w.call(M::GET, "/pro/tips/domain", None).await;
        assert_eq!((s, v["hostname"].as_str()), (200, Some("tips.mychannel.com")));
        assert_eq!(w.call(M::DELETE, "/pro/tips/domain", None).await.0, 204);
        // A hostname Cloudflare already has, without this PC's TXT proof: 409 with the record
        // to add, passed on whole.
        w.cf.insert("tips.elsewhere.com");
        let (s, v) = w.call(M::PUT, "/pro/tips/domain", Some(json!({ "hostname": "tips.elsewhere.com" }))).await;
        assert_eq!((s, v["error"]["code"].as_str()), (409, Some("domain_taken")), "{v}");
        assert!(v["error"]["hint"].as_str().unwrap().contains("TXT record"), "{v}");
        assert_eq!(v["proof"]["name"], "_ferrocast.tips.elsewhere.com");
        assert!(v["proof"]["value"].as_str().unwrap().starts_with("ferrocast-proof-"), "{v}");
        let (s, v) = w.call(M::GET, "/pro/tips/domain", None).await;
        assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")));
    }

    #[tokio::test]
    async fn with_the_relay_down_branding_says_why() {
        let w = world(false).await;
        let (s, v) = w.call(M::GET, "/tips/brand", None).await;
        assert_eq!((s, v["error"]["code"].as_str()), (502, Some("relay_unreachable")), "{v}");
        let (s, _) = w.upload("avatar", "image/png", PNG.to_vec()).await;
        assert_eq!(s, 502);
        let (s, v) = w.call(M::GET, "/pro/tips/domain", None).await;
        assert_eq!((s, v["error"]["code"].as_str()), (502, Some("relay_unreachable")), "{v}");
    }
}
