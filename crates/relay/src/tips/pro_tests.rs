//! Pro tip page branding and goal, through the relay's real `router()`: every status the
//! routes answer, the public pages with and without Pro, goal counting from the webhooks
//! the relay already verifies, and the hourly sweep.

use std::io::Cursor;
use std::sync::Arc;

use reqwest::StatusCode;
use serde_json::{Value, json};

use super::{Provider, TipConfig, sign_hex};
use crate::billing::store::Synced;
use crate::{AppState, Config, Db, now_ms, router};

const RZP: &str = "rzp-webhook-secret";
const KOFI: &str = "kofi-token-123";
const STRIPE: &str = "whsec_test_secret";
const DAY: i64 = 86_400_000;

struct World {
    base: String,
    st: AppState,
    http: reqwest::Client,
    blob_dir: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn world(blobs: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        tip_url: Some("https://tip.example".into()),
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let blob_dir = dir.path().join("blobs");
    let store: Option<Arc<dyn crate::blob::BlobStore>> =
        blobs.then(|| Arc::new(crate::blob::FsBlobStore::new(&blob_dir).unwrap()) as Arc<dyn crate::blob::BlobStore>);
    let st = AppState::new(cfg, db).with_blobs(store);
    let app = router(st.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    World { base, st, http: reqwest::Client::new(), blob_dir, _dir: dir }
}

fn provider(url: &str, secret: &str) -> Option<Provider> {
    Some(Provider { page_url: url.into(), secret: secret.into(), ..Provider::default() })
}

impl World {
    async fn install(&self) -> (String, String) {
        let v: Value = self.http.post(format!("{}/v1/installs", self.base)).send().await.unwrap().json().await.unwrap();
        let id = v["install_id"].as_str().unwrap().to_string();
        (format!("{id}.{}", v["install_secret"].as_str().unwrap()), id)
    }

    /// An install with an active subscription, straight in the database.
    async fn pro(&self, sub: &str) -> (String, String) {
        let (auth, id) = self.install().await;
        self.st.db.add_pro_sub(sub, "monthly", "h", "active", &id).unwrap();
        self.set_sub(sub, "active", now_ms() + 30 * DAY);
        (auth, id)
    }

    fn set_sub(&self, sub: &str, status: &str, end: i64) {
        let s = Synced { status: status.into(), current_end: Some(end), cancel_at_end: Some(false) };
        self.st.db.sync_pro_sub(None, sub, &s).unwrap();
    }

    async fn tip_page(&self, auth: &str, handle: &str) -> Value {
        let c = TipConfig {
            handle: handle.into(),
            display_name: "Nova <b>Plays</b>".into(),
            message: "Thanks!".into(),
            razorpay: provider("https://razorpay.me/@novaplays", RZP),
            kofi: provider("https://ko-fi.com/novaplays", KOFI),
            stripe: provider("https://buy.stripe.com/test_abc", STRIPE),
        };
        let res =
            self.http.put(format!("{}/v1/tips/config", self.base)).bearer_auth(auth).json(&c).send().await.unwrap();
        assert_eq!(res.status(), 200);
        res.json().await.unwrap()
    }

    async fn call(&self, method: reqwest::Method, path: &str, auth: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let mut r = self.http.request(method, format!("{}{path}", self.base));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }

    async fn upload(&self, auth: &str, kind: &str, ct: &str, bytes: Vec<u8>) -> (u16, Value) {
        let res = self
            .http
            .put(format!("{}/v1/pro/tips/brand/{kind}", self.base))
            .bearer_auth(auth)
            .header("content-type", ct)
            .body(bytes)
            .send()
            .await
            .unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }

    async fn html(&self, path: &str, host: Option<&str>) -> (u16, String, reqwest::header::HeaderMap) {
        let mut r = self.http.get(format!("{}{path}", self.base));
        if let Some(h) = host {
            r = r.header("host", h);
        }
        let res = r.send().await.unwrap();
        let (s, h) = (res.status().as_u16(), res.headers().clone());
        (s, res.text().await.unwrap(), h)
    }

    async fn rzp(&self, install: &str, body: Value) -> u16 {
        let b = body.to_string();
        self.http
            .post(format!("{}/v1/tips/razorpay/{install}", self.base))
            .header("X-Razorpay-Signature", sign_hex(RZP, b.as_bytes()))
            .body(b)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn stripe(&self, install: &str, body: Value) -> u16 {
        let b = body.to_string();
        let t = now_ms() / 1000;
        let sig = sign_hex(STRIPE, format!("{t}.{b}").as_bytes());
        self.http
            .post(format!("{}/v1/tips/stripe/{install}", self.base))
            .header("Stripe-Signature", format!("t={t},v1={sig}"))
            .body(b)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn kofi(&self, install: &str, id: &str, amount: &str, currency: &str) -> u16 {
        let data = json!({ "verification_token": KOFI, "type": "Donation", "amount": amount, "currency": currency,
                           "kofi_transaction_id": id, "from_name": "K", "message": "" })
        .to_string();
        self.http
            .post(format!("{}/v1/tips/kofi/{install}", self.base))
            .form(&[("data", data.as_str())])
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn raised(&self, auth: &str) -> (i64, i64) {
        let (s, v) = self.call(reqwest::Method::GET, "/v1/pro/tips/goal", Some(auth), None).await;
        assert_eq!(s, 200, "{v}");
        (v["raised"].as_i64().unwrap(), v["tips"].as_i64().unwrap())
    }
}

fn rzp_captured(id: &str, amount: i64, currency: &str) -> Value {
    json!({ "event": "payment.captured", "created_at": now_ms() / 1000, "payload": { "payment": { "entity": {
        "id": id, "amount": amount, "currency": currency, "notes": { "name": "Asha" } } } } })
}

fn rzp_refunded(id: &str) -> Value {
    json!({ "event": "refund.processed", "created_at": now_ms() / 1000, "payload": {
        "refund": { "entity": { "payment_id": id } },
        "payment": { "entity": { "id": id, "status": "refunded", "currency": "INR" } } } })
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 90, 255]));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// A valid PNG header claiming a size it doesn't have (CRC fixed, so only the size is wrong).
fn png_claiming(w: u32, h: u32) -> Vec<u8> {
    let mut p = png(1, 1);
    p[16..20].copy_from_slice(&w.to_be_bytes());
    p[20..24].copy_from_slice(&h.to_be_bytes());
    let crc = crc32(&p[12..29]);
    p[29..33].copy_from_slice(&crc.to_be_bytes());
    p
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for b in data {
        c ^= u32::from(*b);
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

#[tokio::test]
async fn every_pro_tip_route_is_behind_the_gate() {
    let w = world(true).await;
    let (free, _) = w.install().await;
    use reqwest::Method as M;
    let routes = [
        (M::GET, "/v1/pro/tips/brand"),
        (M::PUT, "/v1/pro/tips/brand"),
        (M::DELETE, "/v1/pro/tips/brand"),
        (M::PUT, "/v1/pro/tips/brand/avatar"),
        (M::DELETE, "/v1/pro/tips/brand/banner"),
        (M::GET, "/v1/pro/tips/goal"),
        (M::PUT, "/v1/pro/tips/goal"),
        (M::DELETE, "/v1/pro/tips/goal"),
    ];
    for (m, path) in routes {
        let body = (m == M::PUT && !path.contains("avatar")).then(|| json!({}));
        let (s, v) = w.call(m.clone(), path, None, body.clone()).await;
        assert_eq!((s, v["error"]["code"].as_str()), (401, Some("unauthorized")), "{m} {path}");
        let (s, v) = w.call(m.clone(), path, Some("nobody.nothing"), body.clone()).await;
        assert_eq!(s, 401, "{m} {path}: {v}");
        let (s, v) = w.call(m.clone(), path, Some(&free), body).await;
        assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")), "{m} {path}");
        assert!(v["error"]["hint"].as_str().unwrap().contains("Settings › Pro"));
    }
}

#[tokio::test]
async fn status_says_branding_is_here_only_with_a_file_store() {
    let w = world(true).await;
    let (auth, _) = w.pro("sub_status").await;
    let (_, v) = w.call(reqwest::Method::GET, "/v1/pro/status", Some(&auth), None).await;
    assert_eq!(v["features"]["tip_brand"], true);
    assert_eq!(v["quotas"]["tip_images"]["limit"], 30);
    assert_eq!(v["quotas"]["tip_images"]["period"], "day");
    let none = world(false).await;
    let (auth, _) = none.pro("sub_status2").await;
    let (_, v) = none.call(reqwest::Method::GET, "/v1/pro/status", Some(&auth), None).await;
    assert_eq!(v["features"]["tip_brand"], false, "no store, no avatars: the panel stays hidden");
}

#[tokio::test]
async fn colours_save_fix_their_contrast_and_say_what_changed() {
    let w = world(true).await;
    let (auth, id) = w.pro("sub_colours").await;
    w.tip_page(&auth, "nova").await;
    use reqwest::Method as M;
    // Validation and body errors.
    let (s, v) = w
        .call(
            M::PUT,
            "/v1/pro/tips/brand",
            Some(&auth),
            Some(json!({ "accent": "red", "background": "#000", "text": "#fff" })),
        )
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (400, Some("invalid")));
    assert!(v["error"]["message"].as_str().unwrap().contains("accent"), "{v}");
    let (s, _) = w.call(M::PUT, "/v1/pro/tips/brand", Some(&auth), Some(json!({ "accent": "#fff" }))).await;
    assert_eq!(s, 400, "missing fields");
    let res = w.http.put(format!("{}/v1/pro/tips/brand", w.base)).bearer_auth(&auth).body("x").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE, "not JSON");
    let big = "x".repeat(300 * 1024);
    let res = w
        .http
        .put(format!("{}/v1/pro/tips/brand", w.base))
        .bearer_auth(&auth)
        .header("content-type", "application/json")
        .body(format!("{{\"accent\":\"{big}\"}}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // Grey on grey: the text is lifted to 4.5:1 and the answer says so.
    let (s, v) = w
        .call(
            M::PUT,
            "/v1/pro/tips/brand",
            Some(&auth),
            Some(json!({ "accent": "#2B2B2B", "background": "#202020", "text": "#555555" })),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["background"], "#202020", "the background stays as picked");
    assert_ne!(v["text"], "#555555");
    let changes = v["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 2, "{v}");
    assert_eq!(changes[0]["field"], "text");
    assert!(changes[0]["contrast_after"].as_f64().unwrap() >= 4.5);
    assert!(changes[1]["message"].as_str().unwrap().contains("accent"));
    assert_eq!(v["applied"], true);

    // Readable colours are kept exactly.
    let (s, v) = w
        .call(
            M::PUT,
            "/v1/pro/tips/brand",
            Some(&auth),
            Some(json!({ "accent": "#FFA424", "background": "#0a0b0d", "text": "#eef0f4" })),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!((v["accent"].as_str(), v["changes"].as_array().unwrap().len()), (Some("#ffa424"), 0));
    let (_, got) = w.call(M::GET, "/v1/pro/tips/brand", Some(&auth), None).await;
    assert_eq!(got["text"], "#eef0f4");

    // The public page uses them, with the same strict CSP.
    let (s, html, h) = w.html("/t/nova", None).await;
    assert_eq!(s, 200);
    assert!(html.contains("--accent:#ffa424") && html.contains("--bg:#0a0b0d"), "{html}");
    assert!(!html.contains("<script"), "script-free");
    let csp = h["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("default-src 'none'") && csp.contains("img-src 'self' data:") && !csp.contains("script-src"));
    assert!(html.contains("Nova &lt;b&gt;Plays&lt;/b&gt;"), "still escaped");

    // Pro lapses: the page is the default look again (the colours are kept).
    w.set_sub("sub_colours", "halted", now_ms() + 30 * DAY);
    let (_, html, _) = w.html("/t/nova", None).await;
    assert!(html.contains("--accent:#3461f5") && !html.contains("#ffa424"), "default look without Pro");
    assert!(w.st.db.tip_brand(&id).unwrap().unwrap().colours.is_some(), "data kept");
    w.set_sub("sub_colours", "active", now_ms() + 30 * DAY);

    // DELETE: back to the default colours.
    let (s, _) = w.call(M::DELETE, "/v1/pro/tips/brand", Some(&auth), None).await;
    assert_eq!(s, 204);
    let (_, got) = w.call(M::GET, "/v1/pro/tips/brand", Some(&auth), None).await;
    assert_eq!(got["accent"], Value::Null);
}

#[tokio::test]
async fn avatars_and_banners_are_checked_reencoded_and_served_for_good() {
    let w = world(true).await;
    let (auth, id) = w.pro("sub_images").await;
    w.tip_page(&auth, "nova").await;
    // Unknown image, wrong declared type, wrong real type, too big, broken.
    assert_eq!(w.upload(&auth, "logo", "image/png", png(4, 4)).await.0, 404);
    let (s, v) = w.upload(&auth, "avatar", "image/gif", b"GIF89a".to_vec()).await;
    assert_eq!((s, v["error"]["code"].as_str()), (415, Some("unsupported_type")));
    assert!(v["error"]["message"].as_str().unwrap().contains("PNG, JPEG or WebP"), "{v}");
    let (s, _) = w.upload(&auth, "avatar", "image/png", b"GIF89a\x01\x00\x01\x00\x00\x00".to_vec()).await;
    assert_eq!(s, 415, "a GIF sent as PNG");
    let (s, v) = w.upload(&auth, "avatar", "image/png", vec![0u8; 1_000_001]).await;
    assert_eq!((s, v["error"]["code"].as_str()), (413, Some("too_large")));
    assert_eq!(v["error"]["message"], "Avatars can be up to 1 MB.");
    let (s, v) = w.upload(&auth, "banner", "image/jpeg", vec![0u8; 3_000_001]).await;
    assert_eq!((s, v["error"]["message"].as_str()), (413, Some("Banners can be up to 3 MB.")));
    let used = || w.st.db.quota_used("sub_images", "tip_images", crate::quota::Period::Day).unwrap();
    let before = used();
    let mut broken = png(64, 64);
    broken.truncate(70);
    let (s, v) = w.upload(&auth, "avatar", "image/png", broken).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("image_unreadable")));
    let (s, v) = w.upload(&auth, "avatar", "image/png", png_claiming(20_000, 20_000)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (413, Some("too_large")), "too many pixels: {v}");
    assert_eq!(used(), before, "a refused image gives its upload back");
    let (s, _) = w.upload(&auth, "avatar", "image/png", Vec::new()).await;
    assert_eq!(s, 422, "an empty body");

    // A good one: stored as WebP by its hash, served immutable on both hosts.
    let (s, v) = w.upload(&auth, "avatar", "image/png; charset=binary", png(800, 800)).await;
    assert_eq!(s, 200, "{v}");
    let url = v["avatar_url"].as_str().unwrap().to_string();
    assert!(url.starts_with(&format!("{}/t/a/", w.base)) && url.ends_with(".webp"), "{url}");
    let res = w.http.get(&url).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "image/webp");
    assert_eq!(res.headers()["cache-control"], "public, max-age=31536000, immutable");
    let back = image::load_from_memory(&res.bytes().await.unwrap()).unwrap();
    assert_eq!((back.width(), back.height()), (512, 512));
    let path = url.trim_start_matches(&w.base).to_string();
    let on_tip = w.http.get(format!("{}{path}", w.base)).header("host", "tip.example").send().await.unwrap();
    assert_eq!(on_tip.status(), 200, "the tip host serves it too");
    assert_eq!(w.html("/t/a/nothex.webp", None).await.0, 404);
    assert_eq!(w.html(&format!("/t/a/{}.webp", "0".repeat(64)), None).await.0, 404);

    let (s, v) = w.upload(&auth, "banner", "image/png", png(3000, 1000)).await;
    assert_eq!(s, 200);
    assert!(v["banner_url"].is_string() && v["avatar_url"] == url.as_str());

    // The page shows both while Pro.
    let (_, html, _) = w.html("/nova", Some("tip.example")).await;
    assert!(html.contains(&path) && html.contains("class=banner"), "{html}");

    // Replacing the avatar removes the old file (nothing else uses it).
    let old_file = w.blob_dir.join(path.trim_start_matches('/'));
    assert!(old_file.exists());
    let (s, _) = w.upload(&auth, "avatar", "image/png", png(300, 300)).await;
    assert_eq!(s, 200);
    assert!(!old_file.exists(), "the replaced image is gone");
    // Removing it.
    let (s, _) = w.call(reqwest::Method::DELETE, "/v1/pro/tips/brand/avatar", Some(&auth), None).await;
    assert_eq!(s, 204);
    let b = w.st.db.tip_brand(&id).unwrap().unwrap();
    assert!(b.avatar.is_none() && b.banner.is_some());
    assert_eq!(w.call(reqwest::Method::DELETE, "/v1/pro/tips/brand/logo", Some(&auth), None).await.0, 404);

    // The daily quota: 429 with when it starts again.
    let sub = "sub_images";
    let used = w.st.db.quota_used(sub, "tip_images", crate::quota::Period::Day).unwrap();
    w.st.db.try_take(sub, "tip_images", crate::quota::Period::Day, 30 - used, 30).unwrap();
    let res = w
        .http
        .put(format!("{}/v1/pro/tips/brand/banner", w.base))
        .bearer_auth(&auth)
        .header("content-type", "image/png")
        .body(png(10, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429);
    assert!(res.headers().contains_key("x-quota-reset") && res.headers().contains_key("retry-after"));
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["error"]["code"], "tip_image_quota");
}

#[tokio::test]
async fn without_a_file_store_uploads_say_storage_is_unavailable() {
    let w = world(false).await;
    let (auth, _) = w.pro("sub_nostore").await;
    let (s, v) = w.upload(&auth, "avatar", "image/png", png(4, 4)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("storage_unavailable")));
    assert_eq!(w.html(&format!("/t/a/{}.webp", "a".repeat(64)), None).await.0, 503);
}

#[tokio::test]
async fn the_goal_counts_verified_tips_in_its_currency_and_refunds_come_off() {
    let w = world(true).await;
    let (auth, id) = w.pro("sub_goal").await;
    use reqwest::Method as M;
    let goal = json!({ "title": "New mic", "target": 1_000_000, "currency": "inr" });
    let (s, v) = w.call(M::PUT, "/v1/pro/tips/goal", Some(&auth), Some(goal.clone())).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("no_tip_page")));
    w.tip_page(&auth, "nova").await;
    for (bad, what) in [
        (json!({ "title": "", "target": 100, "currency": "INR" }), "title"),
        (json!({ "title": "x".repeat(61), "target": 100, "currency": "INR" }), "title"),
        (json!({ "title": "Mic", "target": 0, "currency": "INR" }), "target"),
        (json!({ "title": "Mic", "target": 100, "currency": "RUPEES" }), "currency"),
        (json!({ "title": "Mic", "target": 100, "currency": "INR", "ends_at": now_ms() - 1 }), "ends_at"),
        (json!({ "title": "Mic", "target": 100, "currency": "INR", "ends_at": now_ms() + 400 * DAY }), "ends_at"),
    ] {
        let (s, v) = w.call(M::PUT, "/v1/pro/tips/goal", Some(&auth), Some(bad)).await;
        assert_eq!(s, 400, "{what}: {v}");
        assert!(v["error"]["message"].as_str().unwrap().contains(what.trim_end_matches("_at")), "{what}: {v}");
    }
    let (s, v) = w.call(M::GET, "/v1/pro/tips/goal", Some(&auth), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")));
    let (s, v) = w.call(M::PUT, "/v1/pro/tips/goal", Some(&auth), Some(goal)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["currency"].as_str(), v["raised"].as_i64()), (Some("INR"), Some(0)));
    assert_eq!(v["page_url"], "https://tip.example/nova/goal");
    assert_eq!(v["overlay_url"], "https://tip.example/nova/goal?overlay=1");

    // ₹500 counts; a replay doesn't count twice; USD isn't counted.
    assert_eq!(w.rzp(&id, rzp_captured("pay_a", 50_000, "INR")).await, 200);
    assert_eq!(w.rzp(&id, rzp_captured("pay_a", 50_000, "INR")).await, 200);
    assert_eq!(w.rzp(&id, rzp_captured("pay_usd", 1_000, "USD")).await, 200);
    assert_eq!(w.raised(&auth).await, (50_000, 1));
    // Ko-fi's decimal amounts, in INR too.
    assert_eq!(w.kofi(&id, "kofi-goal-1", "250.50", "INR").await, 200);
    assert_eq!(w.raised(&auth).await, (75_050, 2));
    // Stripe in INR.
    let paid = json!({ "type": "checkout.session.completed", "livemode": true, "data": { "object": {
        "payment_status": "paid", "payment_intent": "pi_goal", "amount_total": 20_000, "currency": "inr" } } });
    assert_eq!(w.stripe(&id, paid).await, 200);
    assert_eq!(w.raised(&auth).await, (95_050, 3));
    // A full refund and a dispute come off (R-32).
    assert_eq!(w.rzp(&id, rzp_refunded("pay_a")).await, 200);
    assert_eq!(w.raised(&auth).await, (45_050, 2));
    let dispute = json!({ "type": "charge.dispute.created", "livemode": true, "data": { "object": {
        "payment_intent": "pi_goal", "status": "needs_response" } } });
    assert_eq!(w.stripe(&id, dispute).await, 200);
    assert_eq!(w.raised(&auth).await, (25_050, 1));
    // A refund that arrives before its payment: the payment is never counted.
    assert_eq!(w.rzp(&id, rzp_refunded("pay_early")).await, 200);
    assert_eq!(w.rzp(&id, rzp_captured("pay_early", 70_000, "INR")).await, 200);
    assert_eq!(w.raised(&auth).await, (25_050, 1));

    // The public page, on both hosts, script-free.
    let (s, html, h) = w.html("/t/nova/goal", None).await;
    assert_eq!(s, 200);
    assert!(html.contains("New mic") && html.contains("₹250.50") && html.contains("₹10,000"), "{html}");
    assert!(html.contains("aria-valuenow=2") && html.contains("width:2%"), "2% of the target");
    assert!(html.contains("Tips in other currencies aren't counted"));
    assert!(!html.contains("<script") && h["content-security-policy"].to_str().unwrap().contains("default-src 'none'"));
    let (s, tip_host, _) = w.html("/nova/goal", Some("tip.example")).await;
    assert_eq!(s, 200);
    assert!(tip_host.contains("href=\"../nova\""), "the tip button links back to the page");
    assert_eq!(w.html("/nova/goal", None).await.0, 404, "only on the tip host");
    // OBS variant: transparent, reloads itself, nothing but the goal.
    let (_, ov, _) = w.html("/t/nova/goal?overlay=1", None).await;
    assert!(ov.contains("background:transparent") && ov.contains("http-equiv=refresh content=30"), "{ov}");
    assert!(!ov.contains("class=fine") && !ov.contains("btn primary"));
    // The tip page shows the goal too.
    let (_, tip, _) = w.html("/t/nova", None).await;
    assert!(tip.contains("href=\"nova/goal\"") && tip.contains("New mic"));

    // Editing in the same currency keeps the total; another currency starts over.
    let (_, v) = w
        .call(
            M::PUT,
            "/v1/pro/tips/goal",
            Some(&auth),
            Some(json!({ "title": "Better mic", "target": 2_000_000, "currency": "INR" })),
        )
        .await;
    assert_eq!((v["raised"].as_i64(), v["title"].as_str()), (Some(25_050), Some("Better mic")));
    let (_, v) = w
        .call(
            M::PUT,
            "/v1/pro/tips/goal",
            Some(&auth),
            Some(json!({ "title": "Mic", "target": 50_000, "currency": "USD" })),
        )
        .await;
    assert_eq!(v["raised"], 0);
    assert_eq!(w.rzp(&id, rzp_captured("pay_usd2", 1_000, "USD")).await, 200);
    assert_eq!(w.raised(&auth).await, (1_000, 1));

    // Not Pro: the goal page is gone and tips stop counting; Pro again, it's back.
    w.set_sub("sub_goal", "halted", now_ms() + 30 * DAY);
    assert_eq!(w.html("/t/nova/goal", None).await.0, 404);
    assert_eq!(w.rzp(&id, rzp_captured("pay_usd3", 1_000, "USD")).await, 200);
    let (_, tip, _) = w.html("/t/nova", None).await;
    assert!(!tip.contains("/goal"), "no goal on the free page");
    w.set_sub("sub_goal", "active", now_ms() + 30 * DAY);
    assert_eq!(w.raised(&auth).await, (1_000, 1), "the lapsed tip wasn't counted");

    // DELETE: gone.
    assert_eq!(w.call(M::DELETE, "/v1/pro/tips/goal", Some(&auth), None).await.0, 204);
    assert_eq!(w.call(M::GET, "/v1/pro/tips/goal", Some(&auth), None).await.0, 404);
    assert_eq!(w.html("/t/nova/goal", None).await.0, 404);
    let n: i64 = w.st.db.c().query_row("SELECT COUNT(*) FROM tip_goal_payments", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "its rows went with it");
}

#[tokio::test]
async fn the_sweep_drops_ended_goals_after_60_days_and_lapsed_branding_after_30() {
    let w = world(true).await;
    let (auth, id) = w.pro("sub_sweep").await;
    w.tip_page(&auth, "nova").await;
    let (s, v) = w.upload(&auth, "avatar", "image/png", png(40, 40)).await;
    assert_eq!(s, 200, "{v}");
    let file = w.blob_dir.join(v["avatar_url"].as_str().unwrap().trim_start_matches(&format!("{}/", w.base)));
    assert!(file.exists());
    let ends = now_ms() + DAY;
    let (s, _) = w
        .call(
            reqwest::Method::PUT,
            "/v1/pro/tips/goal",
            Some(&auth),
            Some(json!({ "title": "Mic", "target": 100, "currency": "INR", "ends_at": ends })),
        )
        .await;
    assert_eq!(s, 200);
    // Still Pro: nothing goes.
    super::sweep(&w.st).await;
    assert!(w.st.db.tip_goal(&id).unwrap().is_some() && w.st.db.tip_brand(&id).unwrap().is_some());
    // The goal ended 61 days ago.
    w.st.db.c().execute("UPDATE tip_goals SET ends_at = ?1", [now_ms() - 61 * DAY]).unwrap();
    super::sweep(&w.st).await;
    assert!(w.st.db.tip_goal(&id).unwrap().is_none(), "ended over 60 days ago");
    // Lapsed: marked first, kept 30 days, then deleted with its image.
    w.set_sub("sub_sweep", "halted", now_ms() + 30 * DAY);
    super::sweep(&w.st).await;
    assert!(w.st.db.tip_brand(&id).unwrap().is_some(), "kept for 30 days");
    let marked: Option<i64> = w.st.db.c().query_row("SELECT lapsed_at FROM tip_brand", [], |r| r.get(0)).unwrap();
    assert!(marked.is_some());
    w.st.db.c().execute("UPDATE tip_brand SET lapsed_at = ?1", [now_ms() - 31 * DAY]).unwrap();
    super::sweep(&w.st).await;
    assert!(w.st.db.tip_brand(&id).unwrap().is_none(), "gone after 30 days");
    assert!(!file.exists(), "its image too");
}

#[tokio::test]
async fn pro_again_before_30_days_clears_the_lapse_mark() {
    let w = world(true).await;
    let (auth, id) = w.pro("sub_back").await;
    w.tip_page(&auth, "nova").await;
    let body = json!({ "accent": "#ffa424", "background": "#0a0b0d", "text": "#eef0f4" });
    assert_eq!(w.call(reqwest::Method::PUT, "/v1/pro/tips/brand", Some(&auth), Some(body)).await.0, 200);
    w.set_sub("sub_back", "halted", now_ms() + 30 * DAY);
    super::sweep(&w.st).await;
    w.st.db.c().execute("UPDATE tip_brand SET lapsed_at = ?1", [now_ms() - 20 * DAY]).unwrap();
    w.set_sub("sub_back", "active", now_ms() + 30 * DAY);
    super::sweep(&w.st).await;
    let marked: Option<i64> = w.st.db.c().query_row("SELECT lapsed_at FROM tip_brand", [], |r| r.get(0)).unwrap();
    assert_eq!(marked, None, "Pro again: the clock is reset");
    assert!(w.st.db.tip_brand(&id).unwrap().is_some());
}
