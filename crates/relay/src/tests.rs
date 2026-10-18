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
    relay_with(kick, None).await
}

async fn relay_with(kick: &str, tip_url: Option<&str>) -> Relay {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        tip_url: tip_url.map(Into::into),
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
fn unused_installs_are_pruned_but_real_ones_stay() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("r.db")).unwrap();
    for id in ["flood", "fresh", "used", "kick", "tips"] {
        db.add_install(id, "h").unwrap();
    }
    let old = db::UNUSED_INSTALL_MS + 60_000;
    for id in ["flood", "used", "kick", "tips"] {
        db.backdate_install(id, old).unwrap();
    }
    db.touch_install("used").unwrap();
    db.set_kick_owner("42", "kick").unwrap();
    assert_eq!(db.set_tip_config("tips", "streamer", "{}", 0, 3).unwrap(), db::Saved::Yes);
    db.prune().unwrap();
    assert!(db.install_hash("flood").unwrap().is_none(), "signed up a week ago, never connected");
    for id in ["fresh", "used", "kick", "tips"] {
        assert!(db.install_hash(id).unwrap().is_some(), "{id} is kept");
    }
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
    // Behind the Cloudflare Tunnel container (a private address), Cloudflare's header wins.
    let tunnel: std::net::SocketAddr = "172.18.0.5:41000".parse().unwrap();
    let mut cf = HeaderMap::new();
    cf.insert("cf-connecting-ip", "203.0.113.9".parse().unwrap());
    assert_eq!(client_ip(tunnel, &cf).to_string(), "203.0.113.9");
    assert_eq!(client_ip(remote, &cf).to_string(), "192.0.2.1", "a public caller can't send it");
}

mod tip_tests {
    use super::*;
    use crate::tips::{
        Provider, TipConfig, clean_text, provider_digits, razorpay_signature_ok, sign_hex, stripe_signature_ok,
    };

    fn provider(url: &str, secret: &str) -> Option<Provider> {
        Some(Provider { page_url: url.into(), secret: secret.into(), ..Provider::default() })
    }

    fn cfg() -> TipConfig {
        let mut rzp = provider("https://razorpay.me/@novaplays", "rzp-webhook-secret");
        rzp.as_mut().unwrap().page_id = Some("plink_tips".into());
        TipConfig {
            handle: "nova".into(),
            display_name: "Nova <b>Plays</b>".into(),
            message: "Thanks for the support!".into(),
            razorpay: rzp,
            kofi: provider("https://ko-fi.com/novaplays", "kofi-token-123"),
            stripe: provider("https://buy.stripe.com/test_abc", "whsec_test_secret"),
        }
    }

    #[test]
    fn config_refuses_lookalike_payment_hosts_reserved_and_bad_handles() {
        assert!(cfg().prepare(None).is_ok());
        for bad in [
            "https://razorpay.me.evil.com/x",
            "http://razorpay.me/@x",
            "https://evil.com/razorpay.me",
            "https://user@razorpay.me/x",
            "javascript:alert(1)",
        ] {
            let mut c = cfg();
            c.razorpay.as_mut().unwrap().page_url = bad.into();
            assert!(c.prepare(None).is_err(), "{bad}");
        }
        for bad in [
            "ab",
            "no spaces",
            "x".repeat(31).as_str(),
            "../x",
            "ferrocast",
            "support",
            "twitch",
            "official-nova",
            "kick_staff",
        ] {
            let mut c = cfg();
            c.handle = bad.into();
            assert!(c.prepare(None).is_err(), "{bad}");
        }
        let mut c = cfg();
        c.handle = "  NoVa ".into();
        c.prepare(None).unwrap();
        assert_eq!(c.handle, "nova", "handles are trimmed and lower-cased");
        let mut c = cfg();
        c.kofi.as_mut().unwrap().secret = "short".into();
        assert!(c.prepare(None).is_err());
        let mut c = cfg();
        c.display_name = "\u{202E}\u{200B}  ".into();
        assert!(c.prepare(None).is_err(), "a name of only invisible characters is empty");
        let mut c = cfg();
        c.display_name = "n".repeat(41);
        assert!(c.prepare(None).is_err(), "too long is refused, not cut");
        for (secret, why) in
            [("sk_live_abcdefgh", "an API key"), ("rk_live_abcdefgh", "a restricted key"), ("abcdefghij", "no whsec_")]
        {
            let mut c = cfg();
            c.stripe.as_mut().unwrap().secret = secret.into();
            assert!(c.prepare(None).is_err(), "{why}");
        }
        for bad_id in ["pl_page", "plink_<x>", "anything"] {
            let mut c = cfg();
            c.razorpay.as_mut().unwrap().page_id = Some(bad_id.into());
            assert!(c.prepare(None).is_err(), "{bad_id}: an id webhooks don't carry would drop every tip");
        }
        let mut c = cfg();
        c.kofi.as_mut().unwrap().page_id = Some("plink_x".into());
        assert!(c.prepare(None).is_err(), "Ko-fi has no link ids");
    }

    #[test]
    fn a_blank_secret_keeps_the_stored_one() {
        let mut stored = cfg();
        stored.prepare(None).unwrap();
        let mut next = cfg();
        next.stripe.as_mut().unwrap().secret = "  ".into();
        next.prepare(Some(&stored)).unwrap();
        assert_eq!(next.stripe.unwrap().secret, "whsec_test_secret");
        let mut fresh = cfg();
        fresh.stripe.as_mut().unwrap().secret = String::new();
        assert!(fresh.prepare(None).is_err(), "with nothing stored a secret is required");
    }

    #[test]
    fn text_loses_direction_and_invisible_characters() {
        assert_eq!(clean_text("\u{202E}evil\u{200B}name\u{2066}\u{FEFF}", 40), "evilname");
        assert_eq!(clean_text("  Asha\n\t ", 40), "Asha");
        assert_eq!(clean_text("é".repeat(50).as_str(), 40).chars().count(), 40);
        assert_eq!(clean_text("👨\u{200D}👩\u{200D}👧", 40), "👨\u{200D}👩\u{200D}👧", "emoji joiners stay");
        assert_eq!(clean_text("a\u{2028}b\u{061C}c", 40), "abc");
    }

    #[test]
    fn provider_digit_counts() {
        assert_eq!(provider_digits("razorpay", "INR"), 2);
        assert_eq!(provider_digits("stripe", "JPY"), 0);
        assert_eq!(provider_digits("stripe", "ISK"), 2, "Stripe's special case");
        assert_eq!(provider_digits("stripe", "kwd"), 3);
    }

    #[test]
    fn signatures() {
        let body = br#"{"event":"payment.captured"}"#;
        assert!(razorpay_signature_ok("s3cret", body, &sign_hex("s3cret", body)));
        assert!(!razorpay_signature_ok("s3cret", body, &sign_hex("other", body)));
        assert!(!razorpay_signature_ok("s3cret", body, ""));
        let now = 1_700_000_000;
        let signed = format!("{now}.{}", String::from_utf8_lossy(body));
        let header = format!("t={now},v1={}", sign_hex("whsec", signed.as_bytes()));
        assert!(stripe_signature_ok("whsec", body, &header, now));
        assert!(!stripe_signature_ok("whsec", body, &header, now + 600), "too old");
        assert!(!stripe_signature_ok("whsec", b"{}", &header, now), "body changed");
        assert!(!stripe_signature_ok("whsec", body, "v1=abc", now), "no timestamp");
    }

    async fn link_items(r: &Relay, auth: &str, n: usize) -> Vec<Value> {
        let mut req = format!("{}/v1/link", r.base.replace("http", "ws")).into_client_request().unwrap();
        req.headers_mut().insert("authorization", format!("Bearer {auth}").parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let mut out = Vec::new();
        while out.len() < n {
            let TMessage::Text(t) =
                tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap()
            else {
                continue;
            };
            out.push(serde_json::from_str::<Value>(&t).unwrap()["payload"].clone());
        }
        // Nothing else is waiting: the next read times out.
        let more = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                if let Some(Ok(TMessage::Text(t))) = ws.next().await {
                    return t.to_string();
                }
            }
        })
        .await;
        assert!(more.is_err(), "unexpected extra item: {more:?}");
        out
    }

    async fn post_rzp(http: &reqwest::Client, url: &str, body: &Value) -> u16 {
        let b = body.to_string();
        http.post(url)
            .header("X-Razorpay-Signature", sign_hex("rzp-webhook-secret", b.as_bytes()))
            .body(b)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn post_stripe(http: &reqwest::Client, url: &str, body: &Value) -> u16 {
        let b = body.to_string();
        let t = crate::now_ms() / 1000;
        let sig = sign_hex("whsec_test_secret", format!("{t}.{b}").as_bytes());
        http.post(url)
            .header("Stripe-Signature", format!("t={t},v1={sig}"))
            .body(b)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn save(http: &reqwest::Client, r: &Relay, auth: &str, c: &TipConfig) -> reqwest::Response {
        http.put(format!("{}/v1/tips/config", r.base)).bearer_auth(auth).json(c).send().await.unwrap()
    }

    #[tokio::test]
    async fn tip_pages_live_on_the_tip_host_and_old_links_keep_working() {
        let kick = fake_kick().await;
        let r = relay_with(&kick, Some("https://tip.example")).await;
        let auth = install(&r).await;
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let saved: Value = save(&http, &r, &auth, &cfg()).await.json().await.unwrap();
        assert_eq!(saved["page_url"], "https://tip.example/nova");
        assert!(saved["webhooks"]["razorpay"].as_str().unwrap().starts_with(&r.base), "webhooks stay on the relay");

        let on_tip = |path: &str| http.get(format!("{}{path}", r.base)).header("host", "tip.example").send();
        let page = on_tip("/nova").await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.text().await.unwrap().contains("Nova &lt;b&gt;Plays&lt;/b&gt;"));
        assert_eq!(on_tip("/nobody").await.unwrap().status(), 404);
        assert_eq!(on_tip("/").await.unwrap().status(), 307, "the bare tip host points at Ferrocast");
        assert_eq!(http.get(format!("{}/nova", r.base)).send().await.unwrap().status(), 404, "only on the tip host");
        assert_eq!(http.get(format!("{}/t/nova", r.base)).send().await.unwrap().status(), 200, "old links still work");
        assert_eq!(http.get(format!("{}/health", r.base)).send().await.unwrap().status(), 200);
    }

    #[tokio::test]
    async fn a_tip_page_and_verified_payments_reach_the_desktop_without_personal_data() {
        let kick = fake_kick().await;
        let r = relay(&kick).await;
        let auth = install(&r).await;
        let other = install(&r).await;
        let http = reqwest::Client::new();
        let saved: Value = save(&http, &r, &auth, &cfg()).await.json().await.unwrap();
        let hooks = saved["webhooks"].clone();
        assert_eq!(saved["config"]["razorpay"]["has_secret"], true);
        assert!(!saved.to_string().contains("rzp-webhook-secret"), "secrets never come back");
        assert_eq!(save(&http, &r, &other, &cfg()).await.status(), 409, "handles are first come, first served");

        let page = http.get(format!("{}/t/nova", r.base)).send().await.unwrap();
        let csp = page.headers().get("content-security-policy").unwrap().to_str().unwrap().to_string();
        let html = page.text().await.unwrap();
        assert!(html.contains("Nova &lt;b&gt;Plays&lt;/b&gt;"), "names are escaped");
        assert!(html.contains("<title>Tip Nova &lt;b&gt;Plays&lt;/b&gt;</title>"), "the title too");
        assert!(html.contains("https://razorpay.me/@novaplays"));
        assert!(csp.contains("default-src 'none'"));
        assert_eq!(http.get(format!("{}/t/nobody", r.base)).send().await.unwrap().status(), 404);

        let rzp_url = hooks["razorpay"].as_str().unwrap();
        // Razorpay's documented shapes: the payment entity doesn't name its link, the
        // payment_link.paid event does; empty notes are [].
        let captured = json!({ "event": "payment_link.paid", "created_at": crate::now_ms() / 1000, "payload": {
            "payment_link": { "entity": { "id": "plink_tips", "notes": { "name": "Asha\u{202E}", "message": "Keep going!" } } },
            "payment": { "entity": { "id": "pay_1", "amount": 50000, "currency": "INR", "email": "secret@example.com",
                "contact": "+919999999999", "order_id": "order_1", "invoice_id": null, "notes": [] } } } });
        let plain_capture = json!({ "event": "payment.captured", "payload": { "payment": { "entity": {
            "id": "pay_1", "amount": 50000, "currency": "INR", "notes": [] } } } });
        assert_eq!(
            post_rzp(&http, rzp_url, &plain_capture).await,
            200,
            "with a pinned link, plain captures are skipped"
        );
        let mut stale = captured.clone();
        stale["created_at"] = json!(crate::now_ms() / 1000 - 8 * 86_400);
        stale["payload"]["payment"]["entity"]["id"] = json!("pay_old");
        assert_eq!(post_rzp(&http, rzp_url, &stale).await, 200, "a week-old body is ignored");
        assert_eq!(post_rzp(&http, rzp_url, &captured).await, 200);
        assert_eq!(post_rzp(&http, rzp_url, &captured).await, 200, "a retry is fine but queues nothing");
        let merch = json!({ "event": "payment_link.paid", "payload": {
            "payment_link": { "entity": { "id": "plink_shop" } },
            "payment": { "entity": { "id": "pay_merch", "amount": 99900, "currency": "INR", "notes": [] } } } });
        assert_eq!(post_rzp(&http, rzp_url, &merch).await, 200, "a sale on another page is ignored");
        let b = captured.to_string();
        let forged = http
            .post(rzp_url)
            .header("X-Razorpay-Signature", sign_hex("guess", b.as_bytes()))
            .body(b)
            .send()
            .await
            .unwrap();
        assert_eq!(forged.status(), 401);
        let partial = json!({ "event": "refund.processed", "payload": {
            "refund": { "entity": { "payment_id": "pay_1", "amount": 100 } },
            "payment": { "entity": { "id": "pay_1", "status": "captured" } } } });
        assert_eq!(post_rzp(&http, rzp_url, &partial).await, 200, "a partial refund leaves the tip");
        let full = json!({ "event": "refund.processed", "payload": {
            "refund": { "entity": { "payment_id": "pay_1", "amount": 50000 } },
            "payment": { "entity": { "id": "pay_1", "status": "refunded" } } } });
        assert_eq!(post_rzp(&http, rzp_url, &full).await, 200);
        let dispute = json!({ "event": "payment.dispute.created", "payload": { "dispute": { "entity": { "payment_id": "pay_1" } } } });
        assert_eq!(post_rzp(&http, rzp_url, &dispute).await, 200);

        let kofi = json!({ "verification_token": "kofi-token-123", "type": "Donation", "is_public": false, "from_name": "Hidden",
            "message": "private", "amount": "5.00", "currency": "USD", "kofi_transaction_id": "kofi-1" }).to_string();
        let k = http.post(hooks["kofi"].as_str().unwrap()).form(&[("data", kofi.as_str())]).send().await.unwrap();
        assert_eq!(k.status(), 200);
        let bad_kofi = json!({ "verification_token": "nope", "type": "Donation" }).to_string();
        let bad = http.post(hooks["kofi"].as_str().unwrap()).form(&[("data", bad_kofi.as_str())]).send().await.unwrap();
        assert_eq!(bad.status(), 401);
        let no_form = http.post(hooks["kofi"].as_str().unwrap()).body("x=1").send().await.unwrap();
        assert_eq!(no_form.status(), 400);
        assert!(no_form.text().await.unwrap().contains("\"code\""), "errors use the error shape");

        let stripe_url = hooks["stripe"].as_str().unwrap();
        let session = |ty: &str, intent: &str| {
            json!({ "type": ty, "livemode": true, "data": { "object": {
                "payment_status": "paid", "payment_intent": intent, "amount_total": 150_000, "currency": "isk",
                "customer_details": { "email": "x@y.z" },
                "custom_fields": [ { "key": "yourname", "text": { "value": "Bjork" } }, { "key": "message", "text": { "value": "takk" } } ] } } })
        };
        assert_eq!(post_stripe(&http, stripe_url, &session("checkout.session.completed", "pi_1")).await, 200);
        let mut test_mode = session("checkout.session.completed", "pi_test");
        test_mode["livemode"] = json!(false);
        assert_eq!(post_stripe(&http, stripe_url, &test_mode).await, 200, "test notices are ignored unless asked for");
        let mut no_intent = session("checkout.session.completed", "");
        no_intent["data"]["object"]["payment_intent"] = Value::Null;
        assert_eq!(post_stripe(&http, stripe_url, &no_intent).await, 200);
        let partial = json!({ "type": "charge.refunded", "livemode": true, "data": { "object": { "payment_intent": "pi_1", "refunded": false } } });
        assert_eq!(post_stripe(&http, stripe_url, &partial).await, 200);
        let full = json!({ "type": "charge.refunded", "livemode": true, "data": { "object": { "payment_intent": "pi_1", "refunded": true } } });
        assert_eq!(post_stripe(&http, stripe_url, &full).await, 200);
        let inquiry = json!({ "type": "charge.dispute.created", "livemode": true, "data": { "object": {
            "payment_intent": "pi_1", "status": "warning_needs_response" } } });
        assert_eq!(post_stripe(&http, stripe_url, &inquiry).await, 200, "an inquiry takes no money back");
        let kofi_test =
            json!({ "verification_token": "kofi-token-123", "type": "Donation", "amount": "3.00", "currency": "USD",
            "kofi_transaction_id": "00000000-1111-2222-3333-444444444444" })
            .to_string();
        let t = http.post(hooks["kofi"].as_str().unwrap()).form(&[("data", kofi_test.as_str())]).send().await.unwrap();
        assert_eq!(t.status(), 200, "Ko-fi's test notice is ignored outside test mode");
        let stale = http.post(stripe_url).header("Stripe-Signature", "t=1,v1=00").body("{}").send().await.unwrap();
        assert_eq!(stale.status(), 401);

        let items = link_items(&r, &auth, 6).await;
        assert_eq!(items[0]["amount_minor"], 50000);
        assert_eq!(items[0]["digits"], 2);
        assert_eq!(items[0]["name"], "Asha", "direction marks are stripped");
        assert_eq!(items[0]["message"], "Keep going!");
        let all = items.iter().map(|i| i.to_string()).collect::<String>();
        assert!(
            !all.contains("secret@example.com") && !all.contains("+9199") && !all.contains("x@y.z"),
            "no email or phone"
        );
        assert!(!all.contains("pay_merch") && !all.contains("pi_test") && !all.contains("pay_old"));
        assert_eq!((items[1]["status"].as_str(), items[1]["payment_id"].as_str()), (Some("refunded"), Some("pay_1")));
        assert_eq!(items[2]["status"], "disputed");
        assert_eq!(items[3]["name"], "Someone", "a private Ko-fi tip shows no name");
        assert_eq!(items[3]["message"], "");
        assert_eq!(items[3]["amount"], "5.00");
        assert_eq!(items[4]["payment_id"], "pi_1");
        assert_eq!((items[4]["amount_minor"].as_i64(), items[4]["digits"].as_u64()), (Some(150_000), Some(2)));
        assert_eq!(items[4]["currency"], "ISK");
        assert_eq!(items[4]["name"], "Bjork");
        assert_eq!((items[5]["status"].as_str(), items[5]["payment_id"].as_str()), (Some("refunded"), Some("pi_1")));
    }

    #[tokio::test]
    async fn handles_are_held_after_a_rename_and_changes_are_limited() {
        let kick = fake_kick().await;
        let r = relay(&kick).await;
        let a = install(&r).await;
        let b = install(&r).await;
        let http = reqwest::Client::new();
        assert_eq!(save(&http, &r, &a, &cfg()).await.status(), 200);
        let mut renamed = cfg();
        renamed.handle = "nova-two".into();
        assert_eq!(save(&http, &r, &a, &renamed).await.status(), 200);
        assert_eq!(save(&http, &r, &b, &cfg()).await.status(), 409, "the old name stays with its owner for a while");
        let mut edited = renamed.clone();
        edited.message = "new message".into();
        edited.kofi.as_mut().unwrap().secret = String::new();
        assert_eq!(save(&http, &r, &a, &edited).await.status(), 200, "same name, blank secret: kept");
        assert_eq!(save(&http, &r, &a, &cfg()).await.status(), 200, "the owner can take it back");
        let mut third = cfg();
        third.handle = "nova-three".into();
        let limited = save(&http, &r, &a, &third).await;
        assert_eq!(limited.status(), 429, "three renames a day");
        let gone = http.delete(format!("{}/v1/tips/config", r.base)).bearer_auth(&a).send().await.unwrap();
        assert_eq!(gone.status(), 204);
        assert_eq!(http.get(format!("{}/t/nova", r.base)).send().await.unwrap().status(), 404);
        assert_eq!(save(&http, &r, &b, &cfg()).await.status(), 409, "a deleted page's name is held too");
        assert_eq!(save(&http, &r, &a, &cfg()).await.status(), 200, "putting your own page back isn't a fourth change");
        let bad = http
            .put(format!("{}/v1/tips/config", r.base))
            .bearer_auth(&a)
            .header("content-type", "application/json")
            .body("[1,2]")
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);
        let v: Value = bad.json().await.unwrap();
        assert_eq!(v["error"]["code"], "invalid");
        let nobody = http.put(format!("{}/v1/tips/config", r.base)).json(&cfg()).send().await.unwrap();
        assert_eq!(nobody.status(), 401);
    }

    #[test]
    fn a_tip_flood_survives_queue_trimming() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::Db::open(&dir.path().join("r.db")).unwrap();
        db.enqueue("i", "tip", "{}").unwrap();
        for _ in 0..crate::db::MAX_QUEUE + 10 {
            db.enqueue("i", "kick_event", "{}").unwrap();
        }
        let items = db.pending("i", 0, 1).unwrap();
        assert_eq!(items[0].kind, "tip", "chat floods push out chat, not tips");
    }
}

mod tip_isolation {
    use super::*;
    use crate::tips::{Provider, TipConfig, sign_hex};

    #[tokio::test]
    async fn one_install_cant_mark_anothers_notices_as_seen() {
        let kick = fake_kick().await;
        let r = relay(&kick).await;
        let http = reqwest::Client::new();
        let mut hooks = Vec::new();
        let mut auths = Vec::new();
        for (i, handle) in ["victim", "attacker"].iter().enumerate() {
            let auth = install(&r).await;
            let cfg = TipConfig {
                handle: (*handle).into(),
                display_name: (*handle).into(),
                razorpay: Some(Provider {
                    page_url: "https://razorpay.me/@x".into(),
                    secret: format!("secret-number-{i}"),
                    ..Provider::default()
                }),
                ..TipConfig::default()
            };
            let v: Value = http
                .put(format!("{}/v1/tips/config", r.base))
                .bearer_auth(&auth)
                .json(&cfg)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            hooks.push(v["webhooks"]["razorpay"].as_str().unwrap().to_string());
            auths.push(auth);
        }
        let dispute = json!({ "event": "payment.dispute.created", "payload": { "dispute": { "entity": { "payment_id": "pay_X" } } } }).to_string();
        for (i, url) in [(1, &hooks[1]), (0, &hooks[0])] {
            let res = http
                .post(url)
                .header("X-Razorpay-Signature", sign_hex(&format!("secret-number-{i}"), dispute.as_bytes()))
                .body(dispute.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200);
        }
        let mut req = format!("{}/v1/link", r.base.replace("http", "ws")).into_client_request().unwrap();
        req.headers_mut().insert("authorization", format!("Bearer {}", auths[0]).parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Ok(TMessage::Text(t))) = ws.next().await {
                    return serde_json::from_str::<Value>(&t).unwrap();
                }
            }
        })
        .await
        .expect("the victim still gets their own dispute");
        assert_eq!(got["payload"]["status"], "disputed");
    }
}

#[tokio::test]
async fn pro_parts_without_a_key_or_a_store_say_so_and_work_once_set() {
    use axum::response::IntoResponse;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let cfg = Config {
        public_url: "http://127.0.0.1:1".into(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: "http://127.0.0.1:1".into(),
        kick_api_url: "http://127.0.0.1:1".into(),
    };
    let st = AppState::new(cfg, db);
    let res = st.vault().unwrap_err().into_response();
    assert_eq!(res.status(), 503);
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(v["error"]["code"], "vault_unavailable");
    assert!(v["error"]["hint"].is_string());
    let res = st.blobs().unwrap_err().into_response();
    assert_eq!(res.status(), 503);
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(v["error"]["code"], "storage_unavailable");

    let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    let store = blob::FsBlobStore::new(dir.path().join("blobs")).unwrap();
    let st = st.with_vault(Some(vault::Vault::from_base64(&key).unwrap())).with_blobs(Some(std::sync::Arc::new(store)));
    let sealed = st.vault().unwrap().seal("i", "p", b"x").unwrap();
    assert_eq!(st.vault().unwrap().open("i", "p", &sealed).unwrap(), b"x");
    st.blobs().unwrap().put("a/b", axum::body::Bytes::from_static(b"y")).await.unwrap();
    assert_eq!(st.blobs().unwrap().size("a/b").await.unwrap(), Some(1));
}
