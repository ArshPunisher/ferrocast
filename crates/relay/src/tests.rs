use std::sync::OnceLock;

use futures_util::{SinkExt, StreamExt};
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{EncodePublicKey, LineEnding};
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as TMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::*;

/// One small key for every test (generating RSA keys is slow in debug builds).
fn key() -> &'static RsaPrivateKey {
    static KEY: OnceLock<RsaPrivateKey> = OnceLock::new();
    KEY.get_or_init(|| RsaPrivateKey::new(&mut rand_core::OsRng, 1024).unwrap())
}

fn sign(id: &str, ts: &str, body: &str) -> String {
    let signer = SigningKey::<Sha256>::new(key().clone());
    let sig = signer.sign(format!("{id}.{ts}.{body}").as_bytes());
    base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
}

fn ts_now() -> String {
    let secs = now_ms() / 1000;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[test]
fn timestamps_parse_exactly() {
    assert_eq!(kick::rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(kick::rfc3339_ms("2025-01-14T16:08:06Z"), Some(1_736_870_886_000));
    assert_eq!(kick::rfc3339_ms("2025-01-01T11:00:00+11:00"), Some(1_735_689_600_000));
    assert_eq!(kick::rfc3339_ms("2025-01-14T16:08:06.123Z"), Some(1_736_870_886_000));
    assert!((kick::rfc3339_ms(&ts_now()).unwrap() - now_ms()).abs() < 2_000);
    for bad in
        ["", "yesterday", "2025-13-01T00:00:00Z", "2025-01-01 00:00:00Z", "2025-01-01T25:00:00Z", "2025-01-01T00:00:00"]
    {
        assert_eq!(kick::rfc3339_ms(bad), None, "{bad}");
    }
}

#[test]
fn signatures_check_every_part() {
    let pubkey = key().to_public_key();
    let s = sign("id1", "ts", "{\"a\":1}");
    assert!(kick::signature_ok(&pubkey, "id1", "ts", b"{\"a\":1}", &s));
    assert!(!kick::signature_ok(&pubkey, "id1", "ts", b"{\"a\":2}", &s), "body changed");
    assert!(!kick::signature_ok(&pubkey, "id2", "ts", b"{\"a\":1}", &s), "id changed");
    assert!(!kick::signature_ok(&pubkey, "id1", "ts", b"{\"a\":1}", "not base64!"), "garbage signature");
    assert!(!kick::signature_ok(&pubkey, "id1", "ts", b"{\"a\":1}", ""), "empty signature");
}

/// Fake Kick: public key, token endpoint and the users endpoint.
async fn fake_kick() -> String {
    let pem = key().to_public_key().to_public_key_pem(LineEnding::LF).unwrap();
    let app = axum::Router::new()
        .route("/public/v1/public-key", get(move || {
            let pem = pem.clone();
            async move { axum::Json(json!({ "data": { "public_key": pem }, "message": "OK" })) }
        }))
        .route("/oauth/token", post(|body: String| async move {
            if body.contains("refresh_token=revoked") {
                return (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": "invalid" })));
            }
            (StatusCode::OK, axum::Json(json!({ "access_token": "kick-access", "refresh_token": "kick-refresh", "expires_in": "7200", "token_type": "Bearer", "scope": "" })))
        }))
        .route("/public/v1/users", get(|| async { axum::Json(json!({ "data": [{ "user_id": 4242, "name": "kicker", "profile_picture": "" }] })) }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    base
}

struct Relay {
    base: String,
    _dir: tempfile::TempDir,
}

async fn relay(kick: &str) -> Relay {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        kick_client_id: Some("cid".into()),
        kick_client_secret: Some("csecret".into()),
        kick_id_url: kick.into(),
        kick_api_url: kick.into(),
    };
    let app = router(AppState::new(cfg, db)).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    Relay { base, _dir: dir }
}

async fn install(r: &Relay) -> String {
    let v: Value =
        reqwest::Client::new().post(format!("{}/v1/installs", r.base)).send().await.unwrap().json().await.unwrap();
    format!("{}.{}", v["install_id"].as_str().unwrap(), v["install_secret"].as_str().unwrap())
}

async fn webhook(r: &Relay, id: &str, ts: &str, body: &Value, sig: Option<String>) -> u16 {
    let body = body.to_string();
    let sig = sig.unwrap_or_else(|| sign(id, ts, &body));
    reqwest::Client::new()
        .post(format!("{}/v1/kick/webhook", r.base))
        .header("Kick-Event-Message-Id", id)
        .header("Kick-Event-Message-Timestamp", ts)
        .header("Kick-Event-Signature", sig)
        .header("Kick-Event-Type", "channel.followed")
        .header("Kick-Event-Version", "1")
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn kick_sign_in_then_verified_webhooks_reach_the_desktop_once() {
    let kick = fake_kick().await;
    let r = relay(&kick).await;
    let auth = install(&r).await;
    let http = reqwest::Client::new();

    let start: Value =
        http.post(format!("{}/v1/kick/oauth", r.base)).bearer_auth(&auth).send().await.unwrap().json().await.unwrap();
    let url = url::Url::parse(start["url"].as_str().unwrap()).unwrap();
    let state = url.query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
    assert!(url.as_str().contains("code_challenge_method=S256"));
    let page = http.get(format!("{}/v1/kick/callback?code=abc&state={state}", r.base)).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let again = http.get(format!("{}/v1/kick/callback?code=abc&state={state}", r.base)).send().await.unwrap();
    assert_eq!(again.status(), 400, "a state works once");

    let follow = json!({ "broadcaster": { "user_id": 4242 }, "follower": { "user_id": 1, "username": "fan" } });
    let ts = ts_now();
    assert_eq!(webhook(&r, "m-1", &ts, &follow, None).await, 200);
    assert_eq!(webhook(&r, "m-1", &ts, &follow, None).await, 200, "a retry is accepted but not queued twice");
    assert_eq!(webhook(&r, "m-2", &ts, &follow, Some(sign("m-2", &ts, "{}"))).await, 401, "signature for another body");
    assert_eq!(webhook(&r, "m-3", "2020-01-01T00:00:00Z", &follow, None).await, 401, "stale timestamp");
    let stranger = json!({ "broadcaster": { "user_id": 999 } });
    assert_eq!(webhook(&r, "m-4", &ts, &stranger, None).await, 200, "a channel nobody owns is dropped quietly");

    let mut req = format!("{}/v1/link", r.base.replace("http", "ws")).into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {auth}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let mut kinds = Vec::new();
    while kinds.len() < 2 {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
        let TMessage::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        kinds.push(v["kind"].as_str().unwrap().to_string());
        if v["kind"] == "kick_connected" {
            assert_eq!(v["payload"]["user"]["id"], "4242");
            assert_eq!(v["payload"]["tokens"]["refresh_token"], "kick-refresh");
        } else {
            assert_eq!(v["payload"]["message_id"], "m-1");
        }
        ws.send(TMessage::text(json!({ "type": "ack", "id": v["id"] }).to_string())).await.unwrap();
    }
    assert_eq!(kinds, vec!["kick_connected", "kick_event"]);

    // A live event arrives on the open socket too.
    assert_eq!(webhook(&r, "m-5", &ts_now(), &follow, None).await, 200);
    let live = loop {
        let TMessage::Text(t) =
            tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap()
        else {
            continue;
        };
        break serde_json::from_str::<Value>(&t).unwrap();
    };
    assert_eq!(live["payload"]["message_id"], "m-5");
}

#[tokio::test]
async fn refresh_goes_through_the_relay_and_strangers_are_refused() {
    let kick = fake_kick().await;
    let r = relay(&kick).await;
    let auth = install(&r).await;
    let http = reqwest::Client::new();
    let ok: Value = http
        .post(format!("{}/v1/kick/refresh", r.base))
        .bearer_auth(&auth)
        .json(&json!({ "refresh_token": "r" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ok["access_token"], "kick-access");
    let revoked = http
        .post(format!("{}/v1/kick/refresh", r.base))
        .bearer_auth(&auth)
        .json(&json!({ "refresh_token": "revoked" }))
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), 401);
    let (id, _) = auth.split_once('.').unwrap();
    for bad in ["".to_string(), "nodot".into(), format!("{id}.wrong-secret"), "ghost.secret".into()] {
        let res = http
            .post(format!("{}/v1/kick/refresh", r.base))
            .bearer_auth(&bad)
            .json(&json!({ "refresh_token": "r" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "{bad}");
    }
    let mut req = format!("{}/v1/link", r.base.replace("http", "ws")).into_client_request().unwrap();
    req.headers_mut().insert("authorization", "Bearer ghost.secret".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err(), "no link for a stranger");
    let missing = http.post(format!("{}/v1/kick/webhook", r.base)).body("{}").send().await.unwrap();
    assert_eq!(missing.status(), 400);
}

#[tokio::test]
async fn signups_are_rate_limited_per_address() {
    let kick = fake_kick().await;
    let r = relay(&kick).await;
    let http = reqwest::Client::new();
    let mut last = 0;
    for _ in 0..12 {
        last = http.post(format!("{}/v1/installs", r.base)).send().await.unwrap().status().as_u16();
    }
    assert_eq!(last, 429);
}

#[test]
fn the_queue_is_capped_and_acks_are_per_install() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("r.db")).unwrap();
    db.add_install("a", "h").unwrap();
    db.add_install("b", "h").unwrap();
    for i in 0..(db::MAX_QUEUE + 10) {
        db.enqueue("a", "k", &i.to_string()).unwrap();
    }
    let first = db.pending("a", 0, 1).unwrap()[0].clone();
    assert_eq!(first.payload, "10", "the oldest beyond the cap were dropped");
    assert!(!db.ack("b", first.id).unwrap(), "another install can't ack it");
    assert!(db.ack("a", first.id).unwrap());
    assert!(db.first_time("x").unwrap());
    assert!(!db.first_time("x").unwrap());
}

#[test]
fn client_ip_trusts_forwarding_only_from_the_local_proxy() {
    let mut h = HeaderMap::new();
    h.insert("x-forwarded-for", "203.0.113.9, 198.51.100.7".parse().unwrap());
    let local: std::net::SocketAddr = "127.0.0.1:5000".parse().unwrap();
    let remote: std::net::SocketAddr = "192.0.2.1:5000".parse().unwrap();
    assert_eq!(client_ip(local, &h).to_string(), "198.51.100.7");
    assert_eq!(client_ip(remote, &h).to_string(), "192.0.2.1", "a direct caller can't pick its own address");
    assert_eq!(client_ip(local, &HeaderMap::new()).to_string(), "127.0.0.1");
    h.insert("x-forwarded-for", "garbage".parse().unwrap());
    assert_eq!(client_ip(local, &h).to_string(), "127.0.0.1");
}
