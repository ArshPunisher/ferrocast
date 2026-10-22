//! Accounts through the real `router()`: email and Google sign-in (web and app), every
//! refusal (bad state, replays, expiry, other browser, wrong codes, disabled accounts,
//! revoked sessions, CSRF, rate limits, Google off), Pro on accounts and the web dashboard's
//! login rule.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine as _;
use reqwest::Method;
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::traits::PublicKeyParts;
use serde_json::{Value, json};

use super::*;
use crate::billing::store::Synced;
use crate::mail::FakeMailer;
use crate::remote::RemoteConfig;
use crate::{Config, Db, router};

const PEPPER: &[u8] = b"test pepper, at least sixteen bytes";
const CLIENT: &str = "1234-test.apps.googleusercontent.com";
// Built in pieces so nothing here looks like a real key.
fn email_key() -> String {
    format!("{}{}", "k".repeat(20), "e".repeat(20))
}

fn admin_token() -> String {
    "a".repeat(40)
}

fn rsa_key() -> &'static RsaPrivateKey {
    static KEY: OnceLock<RsaPrivateKey> = OnceLock::new();
    KEY.get_or_init(|| RsaPrivateKey::new(&mut rand_core::OsRng, 1024).unwrap())
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn id_token(claims: &Value, kid: &str, alg: &str) -> String {
    let h = b64(json!({ "alg": alg, "kid": kid, "typ": "JWT" }).to_string().as_bytes());
    let p = b64(claims.to_string().as_bytes());
    let sig = SigningKey::<rsa::sha2::Sha256>::new(rsa_key().clone()).sign(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", b64(&sig.to_bytes()))
}

/// A fake Google: `/token` answers the id_token the test set, `/certs` the test key.
#[derive(Default)]
struct FakeGoogle {
    next_token: Mutex<Option<String>>,
    token_calls: Mutex<Vec<HashMap<String, String>>>,
}

async fn fake_google() -> (String, Arc<FakeGoogle>) {
    use axum::routing::{get, post};
    let g = Arc::new(FakeGoogle::default());
    let gg = g.clone();
    let g2 = g.clone();
    let app = axum::Router::new()
        .route(
            "/token",
            post(move |body: String| {
                let g = gg.clone();
                async move {
                    let f: HashMap<String, String> =
                        url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
                    g.token_calls.lock().unwrap().push(f);
                    match g.next_token.lock().unwrap().take() {
                        Some(t) => axum::Json(json!({ "id_token": t, "access_token": "x" })).into_response(),
                        None => StatusCode::BAD_REQUEST.into_response(),
                    }
                }
            }),
        )
        .route(
            "/certs",
            get(move || {
                let _ = &g2;
                async move {
                    let k = rsa_key().to_public_key();
                    axum::Json(json!({ "keys": [{ "kty": "RSA", "alg": "RS256", "kid": "k1",
                        "n": b64(&k.n().to_bytes_be()), "e": b64(&k.e().to_bytes_be()) }] }))
                }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, g)
}

struct World {
    base: String,
    db: Arc<Db>,
    http: reqwest::Client,
    mail: Arc<FakeMailer>,
    google: Arc<FakeGoogle>,
    state: AppState,
    _dir: tempfile::TempDir,
}

async fn world_with(google_on: bool, login: RemoteLogin) -> World {
    world_cfg(google_on, login, None).await
}

/// The website that serves sign-in and the dashboard in the web tests.
const WEB: &str = "https://ferrocast.example";

async fn world_cfg(google_on: bool, login: RemoteLogin, web: Option<&str>) -> World {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let (gbase, google) = fake_google().await;
    let cfg = Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let mail = Arc::new(FakeMailer::new());
    let gcfg = GoogleConfig {
        client_id: google_on.then(|| CLIENT.to_string()),
        client_secret: google_on.then(|| format!("GOC{}", "SPX-test")),
        auth_url: format!("{gbase}/auth"),
        token_url: format!("{gbase}/token"),
        jwks_url: format!("{gbase}/certs"),
    };
    let billing = crate::billing::BillingConfig { email_key: Some(email_key()), ..Default::default() };
    let state = AppState::new(cfg, db)
        .with_remote(RemoteConfig::new(Some(&base), Some(PEPPER), None).with_web(web, &[]))
        .with_mail(Some(mail.clone()), crate::mail::MailConfig::new(None, None))
        .with_billing(billing)
        .with_accounts(AccountsConfig { google: gcfg, remote_login: login })
        .with_admin(crate::admin::AdminConfig::new(Some(&admin_token()), None));
    let db = state.db.clone();
    let app = router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    World { base, db, http, mail, google, state, _dir: dir }
}

async fn world() -> World {
    world_with(true, RemoteLogin::Optional).await
}

/// A browser: its cookies, kept by hand.
#[derive(Default, Clone)]
struct Browser {
    cookies: HashMap<String, String>,
}

struct Res {
    status: u16,
    location: Option<String>,
    body: String,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

impl World {
    fn install(&self, id: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        format!("{id}.secret")
    }

    async fn req(
        &self,
        b: &mut Browser,
        method: Method,
        path: &str,
        form: Option<&[(&str, &str)]>,
        extra: &[(&str, &str)],
    ) -> Res {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{}{path}", self.base) };
        let mut r = self.http.request(method, url);
        if !b.cookies.is_empty() {
            let c: Vec<String> = b.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
            r = r.header("cookie", c.join("; "));
        }
        for (k, v) in extra {
            r = r.header(*k, *v);
        }
        if let Some(f) = form {
            r = r.form(f);
        }
        let res = r.send().await.unwrap();
        for sc in res.headers().get_all("set-cookie") {
            let s = sc.to_str().unwrap();
            let (kv, _) = s.split_once(';').unwrap();
            let (k, v) = kv.split_once('=').unwrap();
            if v.is_empty() || s.contains("Max-Age=0") {
                b.cookies.remove(k);
            } else {
                b.cookies.insert(k.to_string(), v.to_string());
            }
        }
        let status = res.status().as_u16();
        let location = res.headers().get("location").map(|l| l.to_str().unwrap().to_string());
        Res { status, location, body: res.text().await.unwrap() }
    }

    async fn api(&self, method: Method, path: &str, auth: Option<&str>, body: Option<Value>) -> (u16, Value) {
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

    /// A JSON request from the website (its Origin and `Sec-Fetch-Site: same-site`), with the
    /// browser's cookies and, for writes, its login form token.
    async fn web_json(&self, b: &mut Browser, method: Method, path: &str, body: Option<Value>) -> (Res, HeaderMap) {
        let mut r = self.http.request(method.clone(), format!("{}{path}", self.base));
        if !b.cookies.is_empty() {
            let c: Vec<String> = b.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
            r = r.header("cookie", c.join("; "));
        }
        r = r.header("origin", WEB).header("sec-fetch-site", "same-site");
        if method != Method::GET
            && let Some(c) = b.cookies.get(LOGIN_COOKIE)
        {
            r = r.header("x-csrf-token", form_token(c));
        }
        if let Some(v) = body {
            r = r.json(&v);
        }
        let res = r.send().await.unwrap();
        let headers = res.headers().clone();
        for sc in headers.get_all("set-cookie") {
            let s = sc.to_str().unwrap();
            let (kv, _) = s.split_once(';').unwrap();
            let (k, v) = kv.split_once('=').unwrap();
            if v.is_empty() || s.contains("Max-Age=0") {
                b.cookies.remove(k);
            } else {
                b.cookies.insert(k.to_string(), v.to_string());
            }
        }
        let status = res.status().as_u16();
        (Res { status, location: None, body: res.text().await.unwrap() }, headers)
    }

    /// Opens /login (`query` after `?`): the form token.
    async fn open_login(&self, b: &mut Browser, query: &str) -> (Res, String) {
        let r = self.req(b, Method::GET, &format!("/login?{query}"), None, &[]).await;
        let token = b.cookies.get(LOGIN_COOKIE).map(|c| form_token(c)).unwrap_or_default();
        (r, token)
    }

    /// Asks for an email sign-in: (page, magic id, link token, code).
    async fn ask_email(&self, b: &mut Browser, email: &str, flow: &str) -> (Res, String, String, String) {
        let (_, token) = self.open_login(b, &format!("flow={flow}")).await;
        let r = self
            .req(b, Method::POST, "/login/email", Some(&[("csrf", &token), ("email", email), ("flow", flow)]), &[])
            .await;
        let Some(m) = self.mail.last() else { return (r, String::new(), String::new(), String::new()) };
        let t = m.text.split("t=").nth(1).unwrap().split_whitespace().next().unwrap().to_string();
        let code = m.text.split("page: ").nth(1).unwrap().split_whitespace().next().unwrap().replace('-', "");
        let mid = r
            .body
            .split("name=m value=\"")
            .nth(1)
            .map(|x| x.split('"').next().unwrap().to_string())
            .unwrap_or_default();
        (r, mid, t, code)
    }

    /// Signs a browser in on the web by email: the CSRF token.
    async fn web_sign_in(&self, b: &mut Browser, email: &str) -> String {
        self.state.accounts.limits.lock().unwrap().clear();
        let (_, mid, _, code) = self.ask_email(b, email, "").await;
        let token = form_token(b.cookies.get(LOGIN_COOKIE).unwrap());
        let r = self
            .req(b, Method::POST, "/login/code", Some(&[("csrf", &token), ("m", &mid), ("code", &code)]), &[])
            .await;
        assert_eq!(r.status, 303, "{}", r.body);
        assert_eq!(r.location.as_deref(), Some("/app/"));
        let me = self.req(b, Method::GET, "/v1/account", None, &[]).await;
        assert_eq!(me.status, 200, "{}", me.body);
        me.json()["csrf_token"].as_str().unwrap().to_string()
    }

    /// Starts an app sign-in for `auth`: (flow id, verifier, state).
    async fn app_start(&self, auth: &str) -> (String, String, String) {
        let verifier = crate::random(40);
        let challenge = b64(&rsa::sha2::Sha256::digest(verifier.as_bytes()));
        let state = "state-12345".to_string();
        let (s, v) = self
            .api(
                Method::POST,
                "/v1/account/app/start",
                Some(auth),
                Some(json!({ "redirect_uri": "http://127.0.0.1:7878/auth/callback", "code_challenge": challenge,
                             "code_challenge_method": "S256", "state": state, "label": "Studio PC" })),
            )
            .await;
        assert_eq!(s, 201, "{v}");
        assert!(v["url"].as_str().unwrap().ends_with(&format!("/login?flow={}", v["flow_id"].as_str().unwrap())));
        (v["flow_id"].as_str().unwrap().to_string(), verifier, state)
    }

    /// The whole app sign-in by email: the session token.
    async fn app_sign_in(&self, auth: &str, email: &str) -> String {
        // Tests sign the same address in many times; the email limits are tested on their own.
        self.state.accounts.limits.lock().unwrap().clear();
        let (flow, verifier, _) = self.app_start(auth).await;
        let mut b = Browser::default();
        let (_, mid, _, code) = self.ask_email(&mut b, email, &flow).await;
        let token = form_token(b.cookies.get(LOGIN_COOKIE).unwrap());
        let r = self
            .req(&mut b, Method::POST, "/login/code", Some(&[("csrf", &token), ("m", &mid), ("code", &code)]), &[])
            .await;
        assert_eq!(r.status, 303, "{}", r.body);
        let loc = url::Url::parse(r.location.as_deref().unwrap()).unwrap();
        let q: HashMap<String, String> = loc.query_pairs().into_owned().collect();
        let (s, v) = self
            .api(
                Method::POST,
                "/v1/account/app/token",
                Some(auth),
                Some(json!({ "code": q["code"], "code_verifier": verifier })),
            )
            .await;
        assert_eq!(s, 201, "{v}");
        v["session_token"].as_str().unwrap().to_string()
    }

    fn set_sub(&self, sub: &str, status: &str, end: Option<i64>) {
        let s = Synced { status: status.into(), current_end: end, cancel_at_end: None };
        self.db.sync_pro_sub(None, sub, &s).unwrap();
    }
}

use rsa::sha2::Digest as _;

// ── App sign-in ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_app_signs_in_by_email_and_out_again() {
    let w = world().await;
    let auth = w.install("i1");
    let tok = w.app_sign_in(&auth, "Asha@Example.com").await;
    assert!(tok.starts_with("fca_se_"));
    let (s, me) = w.api(Method::GET, "/v1/account", Some(&tok), None).await;
    assert_eq!(s, 200, "{me}");
    assert_eq!(me["account"]["email"], "asha@example.com");
    assert_eq!(me["plan"], "free");
    assert_eq!(me["session"]["kind"], "app");
    assert_eq!(me["session"]["label"], "Studio PC");
    assert_eq!(me["pcs"], 1);
    assert!(me.get("csrf_token").is_none(), "no CSRF token for the app");
    let (s, list) = w.api(Method::GET, "/v1/account/sessions", Some(&tok), None).await;
    assert_eq!(s, 200);
    assert_eq!(list["sessions"][0]["current"], true);
    let (s, _) = w.api(Method::POST, "/v1/account/logout", Some(&tok), Some(json!({}))).await;
    assert_eq!(s, 204);
    let (s, v) = w.api(Method::GET, "/v1/account", Some(&tok), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (401, Some("signed_out")));
    // An install secret isn't a session; a made-up session isn't either.
    let (s, _) = w.api(Method::GET, "/v1/account", Some(&auth), None).await;
    assert_eq!(s, 401);
    let (s, _) = w.api(Method::GET, "/v1/account", Some("fca_se_nope.aaaaaaaaaaaaaaaaaaaaaaaa"), None).await;
    assert_eq!(s, 401);
}

#[tokio::test]
async fn app_start_refuses_bad_input() {
    let w = world().await;
    let auth = w.install("i1");
    let good = "x".repeat(43);
    for (redirect, challenge, method, state, code) in [
        ("https://evil.example/cb", good.as_str(), "S256", "state-1234", "invalid_redirect"),
        ("http://evil.example:80/cb", good.as_str(), "S256", "state-1234", "invalid_redirect"),
        ("http://127.0.0.1/cb", good.as_str(), "S256", "state-1234", "invalid_redirect"),
        ("http://user:pw@127.0.0.1:9/cb", good.as_str(), "S256", "state-1234", "invalid_redirect"),
        ("http://127.0.0.1:9/cb", "short", "S256", "state-1234", "invalid_challenge"),
        ("http://127.0.0.1:9/cb", good.as_str(), "plain", "state-1234", "invalid_challenge"),
        ("http://127.0.0.1:9/cb", good.as_str(), "S256", "no", "invalid"),
    ] {
        let (s, v) = w
            .api(
                Method::POST,
                "/v1/account/app/start",
                Some(&auth),
                Some(json!({ "redirect_uri": redirect, "code_challenge": challenge, "code_challenge_method": method, "state": state })),
            )
            .await;
        assert_eq!((s, v["error"]["code"].as_str()), (422, Some(code)), "{redirect} {challenge} {method} {state}");
    }
    let (s, _) = w.api(Method::POST, "/v1/account/app/start", None, Some(json!({}))).await;
    assert_eq!(s, 401, "install auth needed");
}

#[tokio::test]
async fn the_app_code_works_once_for_its_install_and_verifier_only() {
    let w = world().await;
    let auth = w.install("i1");
    let other = w.install("i2");
    let (flow, verifier, state) = w.app_start(&auth).await;
    let mut b = Browser::default();
    let (_, mid, _, code) = w.ask_email(&mut b, "a@b.co", &flow).await;
    let token = form_token(b.cookies.get(LOGIN_COOKIE).unwrap());
    let r =
        w.req(&mut b, Method::POST, "/login/code", Some(&[("csrf", &token), ("m", &mid), ("code", &code)]), &[]).await;
    let loc = url::Url::parse(r.location.as_deref().unwrap()).unwrap();
    assert_eq!(loc.host_str(), Some("127.0.0.1"));
    let q: HashMap<String, String> = loc.query_pairs().into_owned().collect();
    assert_eq!(q["state"], state);
    let body = |v: &str| json!({ "code": q["code"], "code_verifier": v });
    let (s, v) = w.api(Method::POST, "/v1/account/app/token", Some(&other), Some(body(&verifier))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (410, Some("flow_expired")), "another install");
    let (s, v) = w.api(Method::POST, "/v1/account/app/token", Some(&auth), Some(body(&"w".repeat(43)))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid_challenge")), "wrong verifier");
    let (s, _) = w.api(Method::POST, "/v1/account/app/token", Some(&auth), Some(body(&verifier))).await;
    assert_eq!(s, 201);
    let (s, v) = w.api(Method::POST, "/v1/account/app/token", Some(&auth), Some(body(&verifier))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (410, Some("flow_expired")), "replayed");
    // The flow is done: its login page says so.
    let r = w.req(&mut Browser::default(), Method::GET, &format!("/login?flow={flow}"), None, &[]).await;
    assert_eq!(r.status, 410);
}

#[tokio::test]
async fn an_expired_app_code_is_refused() {
    let w = world().await;
    let auth = w.install("i1");
    let (flow, verifier, _) = w.app_start(&auth).await;
    let mut b = Browser::default();
    let (_, mid, _, code) = w.ask_email(&mut b, "a@b.co", &flow).await;
    let token = form_token(b.cookies.get(LOGIN_COOKIE).unwrap());
    let r =
        w.req(&mut b, Method::POST, "/login/code", Some(&[("csrf", &token), ("m", &mid), ("code", &code)]), &[]).await;
    let loc = url::Url::parse(r.location.as_deref().unwrap()).unwrap();
    let q: HashMap<String, String> = loc.query_pairs().into_owned().collect();
    w.db.c().execute("UPDATE account_app_flows SET code_expires_at = 1", []).unwrap();
    let (s, _) = w
        .api(
            Method::POST,
            "/v1/account/app/token",
            Some(&auth),
            Some(json!({ "code": q["code"], "code_verifier": verifier })),
        )
        .await;
    assert_eq!(s, 410);
}

// ── Email sign-in ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_web_signs_in_with_the_link_and_it_works_once() {
    let w = world().await;
    let mut b = Browser::default();
    let (r, _, t, _) = w.ask_email(&mut b, "a@b.co", "").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let mail = w.mail.last().unwrap();
    assert_eq!(mail.subject, "Your Ferrocast sign-in link");
    assert!(!mail.text.contains('—'));
    // Opening the link doesn't use it (mail scanners); the button does.
    for _ in 0..2 {
        let r = w.req(&mut b, Method::GET, &format!("/login/link?t={t}"), None, &[]).await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("a@b.co"));
    }
    let token = form_token(b.cookies.get(LOGIN_COOKIE).unwrap());
    let r = w.req(&mut b, Method::POST, "/login/link", Some(&[("csrf", &token), ("t", &t)]), &[]).await;
    assert_eq!(r.status, 303, "{}", r.body);
    assert!(b.cookies.contains_key(SESSION_COOKIE));
    let me = w.req(&mut b, Method::GET, "/v1/account", None, &[]).await;
    assert_eq!(me.json()["session"]["kind"], "web");
    // Replayed in a fresh browser session: refused.
    let mut again = Browser::default();
    again.cookies.insert(LOGIN_COOKIE.into(), b.cookies.get(LOGIN_COOKIE).cloned().unwrap_or_else(|| "x".repeat(30)));
    let tok2 = form_token(again.cookies.get(LOGIN_COOKIE).unwrap());
    let r = w.req(&mut again, Method::POST, "/login/link", Some(&[("csrf", &tok2), ("t", &t)]), &[]).await;
    assert_eq!(r.status, 410, "a used link is gone");
}

#[tokio::test]
async fn a_link_or_code_only_works_in_the_browser_that_asked() {
    let w = world().await;
    let mut asker = Browser::default();
    let (_, mid, t, code) = w.ask_email(&mut asker, "a@b.co", "").await;
    let mut other = Browser::default();
    let (_, token) = w.open_login(&mut other, "").await;
    let r = w.req(&mut other, Method::GET, &format!("/login/link?t={t}"), None, &[]).await;
    assert_eq!(r.status, 403);
    let r = w.req(&mut other, Method::POST, "/login/link", Some(&[("csrf", &token), ("t", &t)]), &[]).await;
    assert_eq!(r.status, 410);
    let r = w
        .req(&mut other, Method::POST, "/login/code", Some(&[("csrf", &token), ("m", &mid), ("code", &code)]), &[])
        .await;
    assert_eq!(r.status, 410);
    assert!(!other.cookies.contains_key(SESSION_COOKIE));
}

#[tokio::test]
async fn five_wrong_codes_close_the_sign_in() {
    let w = world().await;
    let mut b = Browser::default();
    let (_, mid, _, code) = w.ask_email(&mut b, "a@b.co", "").await;
    let token = form_token(b.cookies.get(LOGIN_COOKIE).unwrap());
    for i in 1..=MAGIC_TRIES {
        let r = w
            .req(&mut b, Method::POST, "/login/code", Some(&[("csrf", &token), ("m", &mid), ("code", "WRONGCOD")]), &[])
            .await;
        assert_eq!(r.status, if i < MAGIC_TRIES { 422 } else { 410 }, "try {i}");
    }
    let r =
        w.req(&mut b, Method::POST, "/login/code", Some(&[("csrf", &token), ("m", &mid), ("code", &code)]), &[]).await;
    assert_eq!(r.status, 410, "even the right code now");
}

#[tokio::test]
async fn an_expired_link_is_refused() {
    let w = world().await;
    let mut b = Browser::default();
    let (_, mid, t, code) = w.ask_email(&mut b, "a@b.co", "").await;
    w.db.c().execute("UPDATE account_magic SET expires_at = 1", []).unwrap();
    let token = form_token(b.cookies.get(LOGIN_COOKIE).unwrap());
    let r = w.req(&mut b, Method::GET, &format!("/login/link?t={t}"), None, &[]).await;
    assert_eq!(r.status, 410);
    let r =
        w.req(&mut b, Method::POST, "/login/code", Some(&[("csrf", &token), ("m", &mid), ("code", &code)]), &[]).await;
    assert_eq!(r.status, 410);
}

#[tokio::test]
async fn forms_need_the_cookie_and_token_and_the_same_origin() {
    let w = world().await;
    let mut b = Browser::default();
    let (_, token) = w.open_login(&mut b, "").await;
    let r = w.req(&mut b, Method::POST, "/login/email", Some(&[("csrf", "nope"), ("email", "a@b.co")]), &[]).await;
    assert_eq!(r.status, 403, "wrong token");
    let r = w
        .req(
            &mut b,
            Method::POST,
            "/login/email",
            Some(&[("csrf", &token), ("email", "a@b.co")]),
            &[("origin", "https://evil.example")],
        )
        .await;
    assert_eq!(r.status, 403, "another site");
    let r = w
        .req(
            &mut b,
            Method::POST,
            "/login/email",
            Some(&[("csrf", &token), ("email", "a@b.co")]),
            &[("origin", "null"), ("sec-fetch-site", "cross-site")],
        )
        .await;
    assert_eq!(r.status, 403, "a null origin from another site");
    let r = w
        .req(
            &mut b,
            Method::POST,
            "/login/email",
            Some(&[("csrf", &token), ("email", "a@b.co")]),
            &[("origin", "null")],
        )
        .await;
    assert_eq!(r.status, 403, "a null origin nothing vouches for");
    let r = w
        .req(
            &mut b,
            Method::POST,
            "/login/email",
            Some(&[("csrf", &token), ("email", "a@b.co")]),
            &[("origin", "null"), ("sec-fetch-site", "same-origin")],
        )
        .await;
    assert_eq!(r.status, 200, "our own page under no-referrer: Chrome posts Origin: null");
    assert_eq!(w.mail.tried(), 1);
    let mut fresh = Browser::default();
    let r = w.req(&mut fresh, Method::POST, "/login/email", Some(&[("csrf", &token), ("email", "a@b.co")]), &[]).await;
    assert_eq!(r.status, 400, "no cookie");
    assert_eq!(w.mail.tried(), 1, "only the good one sent");
}

#[tokio::test]
async fn sign_in_emails_are_rate_limited_per_address() {
    let w = world().await;
    let mut b = Browser::default();
    for _ in 0..3 {
        let (r, ..) = w.ask_email(&mut b, "a@b.co", "").await;
        assert_eq!(r.status, 200);
    }
    let (r, ..) = w.ask_email(&mut b, "A@B.co", "").await;
    assert_eq!(r.status, 429, "the 4th in 15 minutes, any case");
    let (r, ..) = w.ask_email(&mut b, "c@d.co", "").await;
    assert_eq!(r.status, 200, "another address is fine");
    assert_eq!(w.mail.sent().len(), 4);
}

#[tokio::test]
async fn a_bad_email_is_refused_and_mail_off_says_so() {
    let w = world().await;
    let mut b = Browser::default();
    let (r, ..) = w.ask_email(&mut b, "not-an-email", "").await;
    assert_eq!(r.status, 422);
    w.mail.fail_with(Some(crate::mail::MailError::Misconfigured("x".into())));
    let (r, ..) = w.ask_email(&mut b, "a@b.co", "").await;
    assert_eq!(r.status, 503);
}

// ── Google ───────────────────────────────────────────────────────────────────────────

fn claims(nonce: &str, email: &str, verified: bool) -> Value {
    let now = now_ms() / 1000;
    json!({ "iss": "https://accounts.google.com", "aud": CLIENT, "sub": "g-123", "email": email,
            "email_verified": verified, "name": "Asha", "picture": "https://lh3.example/p.png",
            "iat": now, "exp": now + 3600, "nonce": nonce })
}

/// Starts Google sign-in: (state, nonce).
async fn google_start(w: &World, b: &mut Browser, query: &str) -> (String, String) {
    let (_, token) = w.open_login(b, query).await;
    let flow = query.strip_prefix("flow=").unwrap_or("");
    let r = w.req(b, Method::POST, "/login/google", Some(&[("csrf", &token), ("flow", flow)]), &[]).await;
    assert_eq!(r.status, 303, "{}", r.body);
    let u = url::Url::parse(r.location.as_deref().unwrap()).unwrap();
    let q: HashMap<String, String> = u.query_pairs().into_owned().collect();
    assert_eq!(q["client_id"], CLIENT);
    assert_eq!(q["scope"], "openid email profile");
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["redirect_uri"].ends_with("/login/google/callback"));
    (q["state"].clone(), q["nonce"].clone())
}

#[tokio::test]
async fn google_signs_in_and_joins_the_email_account() {
    let w = world().await;
    let mut first = Browser::default();
    w.web_sign_in(&mut first, "asha@example.com").await;
    let mut b = Browser::default();
    let (state, nonce) = google_start(&w, &mut b, "").await;
    *w.google.next_token.lock().unwrap() = Some(id_token(&claims(&nonce, "Asha@Example.com", true), "k1", "RS256"));
    let r = w.req(&mut b, Method::GET, &format!("/login/google/callback?code=c1&state={state}"), None, &[]).await;
    assert_eq!(r.status, 303, "{}", r.body);
    let call = w.google.token_calls.lock().unwrap()[0].clone();
    assert_eq!(call["grant_type"], "authorization_code");
    assert!(call["code_verifier"].len() >= 43, "PKCE");
    let me = w.req(&mut b, Method::GET, "/v1/account", None, &[]).await.json();
    assert_eq!(me["account"]["name"], "Asha");
    assert_eq!(me["account"]["providers"], json!(["google", "email"]));
    let n: i64 = w.db.c().query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "one account per email");
    // The state is gone now: a replay is refused.
    let r = w.req(&mut b, Method::GET, &format!("/login/google/callback?code=c1&state={state}"), None, &[]).await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn google_refuses_bad_state_nonce_signature_and_unverified_email() {
    let w = world().await;
    let mut b = Browser::default();
    let r = w.req(&mut b, Method::GET, "/login/google/callback?code=c&state=made-up", None, &[]).await;
    assert_eq!(r.status, 400, "unknown state");
    // Another browser can't finish someone's round trip.
    let (state, _) = google_start(&w, &mut b, "").await;
    let r = w
        .req(&mut Browser::default(), Method::GET, &format!("/login/google/callback?code=c&state={state}"), None, &[])
        .await;
    assert_eq!(r.status, 400, "other browser");
    for (tok_for, want) in
        [("nonce", 401u16), ("alg", 401), ("kid", 401), ("aud", 401), ("expired", 401), ("unverified", 403)]
    {
        let (state, nonce) = google_start(&w, &mut b, "").await;
        let mut c = claims(&nonce, "a@b.co", tok_for != "unverified");
        let (mut kid, mut alg) = ("k1", "RS256");
        match tok_for {
            "nonce" => c["nonce"] = json!("other"),
            "alg" => alg = "HS256",
            "kid" => kid = "k9",
            "aud" => c["aud"] = json!("someone-else"),
            "expired" => c["exp"] = json!(now_ms() / 1000 - 3600),
            _ => {}
        }
        *w.google.next_token.lock().unwrap() = Some(id_token(&c, kid, alg));
        let r = w.req(&mut b, Method::GET, &format!("/login/google/callback?code=c&state={state}"), None, &[]).await;
        assert_eq!(r.status, want, "{tok_for}: {}", r.body);
        assert!(!b.cookies.contains_key(SESSION_COOKIE), "{tok_for}");
    }
    // A forged signature (signed, then the payload swapped).
    let (state, nonce) = google_start(&w, &mut b, "").await;
    let good = id_token(&claims(&nonce, "a@b.co", true), "k1", "RS256");
    let parts: Vec<&str> = good.split('.').collect();
    let forged = format!("{}.{}.{}", parts[0], b64(claims(&nonce, "evil@b.co", true).to_string().as_bytes()), parts[2]);
    *w.google.next_token.lock().unwrap() = Some(forged);
    let r = w.req(&mut b, Method::GET, &format!("/login/google/callback?code=c&state={state}"), None, &[]).await;
    assert_eq!(r.status, 401);
}

#[tokio::test]
async fn google_off_says_so_and_email_still_works() {
    let w = world_with(false, RemoteLogin::Optional).await;
    let mut b = Browser::default();
    let (page, token) = w.open_login(&mut b, "").await;
    assert!(!page.body.contains("Continue with Google"));
    let r = w.req(&mut b, Method::POST, "/login/google", Some(&[("csrf", &token)]), &[]).await;
    assert_eq!(r.status, 503);
    assert!(r.body.contains("Google sign-in isn"));
    w.web_sign_in(&mut b, "a@b.co").await;
}

#[tokio::test]
async fn the_app_signs_in_with_google_through_the_loopback() {
    let w = world().await;
    let auth = w.install("i1");
    let (flow, verifier, state) = w.app_start(&auth).await;
    let mut b = Browser::default();
    let (gstate, nonce) = google_start(&w, &mut b, &format!("flow={flow}")).await;
    *w.google.next_token.lock().unwrap() = Some(id_token(&claims(&nonce, "a@b.co", true), "k1", "RS256"));
    let r = w.req(&mut b, Method::GET, &format!("/login/google/callback?code=c&state={gstate}"), None, &[]).await;
    assert_eq!(r.status, 303, "{}", r.body);
    let loc = url::Url::parse(r.location.as_deref().unwrap()).unwrap();
    assert_eq!((loc.host_str(), loc.port()), (Some("127.0.0.1"), Some(7878)));
    let q: HashMap<String, String> = loc.query_pairs().into_owned().collect();
    assert_eq!(q["state"], state);
    assert!(!b.cookies.contains_key(SESSION_COOKIE), "the app's sign-in makes no web session");
    let (s, v) = w
        .api(
            Method::POST,
            "/v1/account/app/token",
            Some(&auth),
            Some(json!({ "code": q["code"], "code_verifier": verifier })),
        )
        .await;
    assert_eq!(s, 201, "{v}");
    // Cancelled at Google: the app hears it.
    let (flow, _, state) = w.app_start(&auth).await;
    let (gstate, _) = google_start(&w, &mut b, &format!("flow={flow}")).await;
    let r = w
        .req(&mut b, Method::GET, &format!("/login/google/callback?error=access_denied&state={gstate}"), None, &[])
        .await;
    assert_eq!(r.status, 303);
    assert!(r.location.unwrap().contains(&format!("error=access_denied&state={state}")));
}

// ── Sessions ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn web_writes_need_the_csrf_token() {
    let w = world().await;
    let mut b = Browser::default();
    let csrf = w.web_sign_in(&mut b, "a@b.co").await;
    let r = w.req(&mut b, Method::POST, "/v1/account/logout", None, &[("content-type", "application/json")]).await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str()), (403, Some("csrf")));
    let r = w
        .req(
            &mut b,
            Method::POST,
            "/v1/account/logout",
            None,
            &[("x-csrf-token", &csrf), ("origin", "https://evil.example")],
        )
        .await;
    assert_eq!(r.status, 403, "another origin");
    let r = w.req(&mut b, Method::POST, "/v1/account/logout", None, &[("x-csrf-token", &csrf)]).await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert!(!b.cookies.contains_key(SESSION_COOKIE));
}

#[tokio::test]
async fn a_revoked_session_stops_at_once_and_only_your_own_can_be_revoked() {
    let w = world().await;
    let auth = w.install("i1");
    let app = w.app_sign_in(&auth, "a@b.co").await;
    let mut b = Browser::default();
    let csrf = w.web_sign_in(&mut b, "a@b.co").await;
    let other = w.app_sign_in(&w.install("i2"), "z@b.co").await;
    let (_, list) = w.api(Method::GET, "/v1/account/sessions", Some(&other), None).await;
    let theirs = list["sessions"][0]["id"].as_str().unwrap().to_string();
    let r = w
        .req(&mut b, Method::DELETE, &format!("/v1/account/sessions/{theirs}"), None, &[("x-csrf-token", &csrf)])
        .await;
    assert_eq!(r.status, 404, "not yours");
    let (_, list) = w.api(Method::GET, "/v1/account/sessions", Some(&app), None).await;
    let app_id = list["sessions"].as_array().unwrap().iter().find(|s| s["kind"] == "app").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = w
        .req(&mut b, Method::DELETE, &format!("/v1/account/sessions/{app_id}"), None, &[("x-csrf-token", &csrf)])
        .await;
    assert_eq!(r.status, 204);
    let (s, _) = w.api(Method::GET, "/v1/account", Some(&app), None).await;
    assert_eq!(s, 401);
    assert_eq!(w.db.account_for_install("i1", now_ms()).unwrap(), None, "the PC is unlinked");
    // Sign out everywhere.
    let r = w
        .req(
            &mut b,
            Method::POST,
            "/v1/account/logout",
            None,
            &[("x-csrf-token", &csrf), ("content-type", "application/json")],
        )
        .await;
    assert_eq!(r.status, 204);
    let (s, _) = w.api(Method::GET, "/v1/account", Some(&other), None).await;
    assert_eq!(s, 200, "another account is untouched");
}

#[tokio::test]
async fn a_disabled_account_is_refused_everywhere() {
    let w = world().await;
    let auth = w.install("i1");
    let app = w.app_sign_in(&auth, "a@b.co").await;
    let (_, me) = w.api(Method::GET, "/v1/account", Some(&app), None).await;
    let id = me["account"]["id"].as_str().unwrap().to_string();
    w.db.c().execute("UPDATE accounts SET disabled_at = 1 WHERE id = ?1", [&id]).unwrap();
    let (s, v) = w.api(Method::GET, "/v1/account", Some(&app), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (403, Some("account_disabled")));
    assert_eq!(w.db.account_for_install("i1", now_ms()).unwrap(), None, "no account Pro either");
    let mut b = Browser::default();
    let (_, mid, _, code) = w.ask_email(&mut b, "a@b.co", "").await;
    let token = form_token(b.cookies.get(LOGIN_COOKIE).unwrap());
    let r =
        w.req(&mut b, Method::POST, "/login/code", Some(&[("csrf", &token), ("m", &mid), ("code", &code)]), &[]).await;
    assert_eq!(r.status, 403);
    assert!(!b.cookies.contains_key(SESSION_COOKIE));
}

#[tokio::test]
async fn an_expired_session_is_signed_out() {
    let w = world().await;
    let app = w.app_sign_in(&w.install("i1"), "a@b.co").await;
    w.db.c().execute("UPDATE account_sessions SET expires_at = 1", []).unwrap();
    let (s, _) = w.api(Method::GET, "/v1/account", Some(&app), None).await;
    assert_eq!(s, 401);
}

// ── Pro on accounts ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn subscriptions_bought_with_the_email_join_the_account_and_reach_its_pcs() {
    let w = world().await;
    let buyer = w.install("old");
    let _ = buyer;
    let h = crate::billing::email_hash(&email_key(), "Asha@example.com");
    w.db.add_pro_sub("sub_Paid0000000001", "monthly", &h, "created", "old").unwrap();
    w.set_sub("sub_Paid0000000001", "active", Some(now_ms() + 20 * DAY));
    let auth = w.install("new");
    assert!(w.db.pro_sub_for_install("new").unwrap().is_none(), "nothing before signing in");
    let tok = w.app_sign_in(&auth, "asha@example.com").await;
    let (_, me) = w.api(Method::GET, "/v1/account", Some(&tok), None).await;
    assert_eq!(me["plan"], "pro");
    assert_eq!(me["pro"]["subscription_id"], "sub_Paid0000000001");
    assert_eq!(me["pro"]["renews"], true);
    let s = w.db.pro_sub_for_install("new").unwrap().unwrap();
    assert_eq!(s.id, "sub_Paid0000000001", "the signed-in PC has the account's Pro");
    let (s, v) = w.api(Method::GET, "/v1/pro/status", Some(&auth), None).await;
    assert_eq!((s, v["pro"].as_bool()), (200, Some(true)));
    // Only the 3 most recently signed-in PCs.
    for i in ["p2", "p3", "p4"] {
        w.app_sign_in(&w.install(i), "asha@example.com").await;
    }
    assert!(w.db.pro_sub_for_install("new").unwrap().is_none(), "the oldest of four drops off");
    assert!(w.db.pro_sub_for_install("p4").unwrap().is_some());
    // Signing out takes it away again.
    let p4 = w.app_sign_in(&w.install("p5"), "asha@example.com").await;
    w.api(Method::POST, "/v1/account/logout", Some(&p4), Some(json!({}))).await;
    assert!(w.db.pro_sub_for_install("p5").unwrap().is_none());
    // A comp grant (no email) is never matched; another person's email neither.
    let other = w.app_sign_in(&w.install("x"), "someone@else.co").await;
    let (_, me) = w.api(Method::GET, "/v1/account", Some(&other), None).await;
    assert_eq!(me["plan"], "free");
}

#[tokio::test]
async fn an_installs_own_pro_wins_and_never_signing_in_changes_nothing() {
    let w = world().await;
    w.install("i1");
    w.db.add_pro_sub("sub_Own00000000001", "yearly", "e", "created", "i1").unwrap();
    w.set_sub("sub_Own00000000001", "active", Some(now_ms() + 300 * DAY));
    assert_eq!(w.db.pro_sub_for_install("i1").unwrap().unwrap().id, "sub_Own00000000001");
    w.db.add_pro_sub("sub_Lapsed00000001", "monthly", "e", "created", "i1").unwrap();
    w.set_sub("sub_Lapsed00000001", "expired", None);
    assert_eq!(w.db.pro_sub_for_install("i1").unwrap().unwrap().id, "sub_Lapsed00000001", "own, even lapsed");
}

// ── The web dashboard's login rule ───────────────────────────────────────────────────

#[tokio::test]
async fn the_web_dashboard_needs_the_account_and_free_accounts_get_it() {
    let w = world_with(true, RemoteLogin::Required).await;
    let auth = w.install("i1");
    // A free install that isn't signed in can't make a link.
    let (s, v) = w.api(Method::POST, "/v1/pro/remote/codes", Some(&auth), Some(json!({ "role": "owner" }))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")));
    w.app_sign_in(&auth, "a@b.co").await;
    let (s, v) = w.api(Method::POST, "/v1/pro/remote/codes", Some(&auth), Some(json!({ "role": "owner" }))).await;
    assert_eq!(s, 201, "a signed-in free PC can: {v}");
    let code = v["code"].as_str().unwrap().to_string();
    // Pairing without signing in: refused.
    let mut anon = Browser::default();
    let r = w.req(&mut anon, Method::POST, "/r/v1/pair", None, &[("content-type", "application/json")]).await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str()), (403, Some("login_required")));
    let mut b = Browser::default();
    let csrf = w.web_sign_in(&mut b, "a@b.co").await;
    let pair = w
        .http
        .post(format!("{}/r/v1/pair", w.base))
        .header("cookie", format!("{SESSION_COOKIE}={}", b.cookies[SESSION_COOKIE]))
        .header("x-csrf-token", &csrf)
        .json(&json!({ "code": code }))
        .send()
        .await
        .unwrap();
    assert_eq!(pair.status().as_u16(), 200);
    let token = pair.json::<Value>().await.unwrap()["token"].as_str().unwrap().to_string();
    let me = |cookie: Option<String>| {
        let mut r = w.http.get(format!("{}/r/v1/me", w.base)).bearer_auth(&token);
        if let Some(c) = cookie {
            r = r.header("cookie", c);
        }
        r.send()
    };
    let r = me(Some(format!("{SESSION_COOKIE}={}", b.cookies[SESSION_COOKIE]))).await.unwrap();
    assert_eq!(r.status().as_u16(), 200, "{}", r.text().await.unwrap());
    let r = me(None).await.unwrap();
    assert_eq!(r.status().as_u16(), 403, "the token alone isn't enough any more");
    let mut eve = Browser::default();
    w.web_sign_in(&mut eve, "eve@b.co").await;
    let r = me(Some(format!("{SESSION_COOKIE}={}", eve.cookies[SESSION_COOKIE]))).await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["error"]["code"], "wrong_account");
    // The account's PCs, and opening one without a code.
    let pcs = w.req(&mut b, Method::GET, "/r/v1/pcs", None, &[]).await.json();
    let pc = pcs["pcs"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(pcs["pcs"][0]["pro"], false);
    let r = w
        .req(
            &mut b,
            Method::POST,
            &format!("/r/v1/pcs/{pc}/connect"),
            None,
            &[("x-csrf-token", &csrf), ("content-type", "application/json")],
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.json()["role"], "owner");
    let r = w
        .req(&mut eve, Method::POST, &format!("/r/v1/pcs/{pc}/connect"), None, &[("content-type", "application/json")])
        .await;
    assert_eq!(r.status, 403, "no CSRF token");
    let queued = w.db.pending("i1", 0, 10).unwrap();
    assert!(queued.iter().any(|i| i.kind == "remote_wanted"));
}

#[test]
fn next_stays_on_the_dashboard() {
    for (raw, want) in [
        (Some("/app/settings"), "/app/settings"),
        (Some("https://evil.example"), "/app/"),
        (Some("//evil.example"), "/app/"),
        (Some("/app//evil.example"), "/app/"),
        (Some("/login"), "/app/"),
        (Some("/app/\\evil"), "/app/"),
        (None, "/app/"),
    ] {
        assert_eq!(pages::safe_next(raw), want, "{raw:?}");
    }
}

#[test]
fn loopback_only() {
    assert!(loopback_ok("http://127.0.0.1:7878/cb"));
    assert!(loopback_ok("http://localhost:1/x?y=1"));
    for bad in [
        "http://127.0.0.1/cb",
        "https://127.0.0.1:1/cb",
        "http://127.0.0.2:1/",
        "http://[::1]:1/",
        "http://127.0.0.1:1/#f",
        "ferrocast://x",
    ] {
        assert!(!loopback_ok(bad), "{bad}");
    }
}

#[test]
fn user_agents_become_short_labels() {
    let mut h = HeaderMap::new();
    h.insert(
        header::USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/605 Chrome/129.0 Safari/605",
        ),
    );
    assert_eq!(ua_label(&h), "Chrome on macOS");
    h.insert(header::USER_AGENT, HeaderValue::from_static("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0) Safari/604"));
    assert_eq!(ua_label(&h), "Safari on iPhone");
}

// ── Admin ────────────────────────────────────────────────────────────────────────────

impl World {
    async fn admin(&self, method: Method, path: &str, body: Option<Value>, who: Option<&str>) -> (u16, Value) {
        let mut r = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(admin_token());
        if let Some(w) = who {
            r = r.header("x-admin-email", w);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn the_control_center_lists_and_manages_accounts() {
    let w = world().await;
    let auth = w.install("i1");
    let app = w.app_sign_in(&auth, "asha@example.com").await;
    w.app_sign_in(&w.install("i2"), "ravi@example.com").await;
    let (s, v) = w.admin(Method::GET, "/admin/v1/accounts?q=asha", None, None).await;
    assert_eq!((s, v["total"].as_i64()), (200, Some(1)), "{v}");
    let id = v["items"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(v["items"][0]["plan"], "free");
    let (_, v) = w.admin(Method::GET, "/admin/v1/accounts?plan=pro", None, None).await;
    assert_eq!(v["total"], 0);
    // Writes need X-Admin-Email.
    let (s, v) =
        w.admin(Method::POST, &format!("/admin/v1/accounts/{id}/grant"), Some(json!({ "days": 7 })), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (400, Some("admin_email_required")));
    for bad in [0, 91] {
        let (s, _) = w
            .admin(
                Method::POST,
                &format!("/admin/v1/accounts/{id}/grant"),
                Some(json!({ "days": bad })),
                Some("o@x.co"),
            )
            .await;
        assert_eq!(s, 422, "{bad} days");
    }
    let (s, v) = w
        .admin(
            Method::POST,
            &format!("/admin/v1/accounts/{id}/grant"),
            Some(json!({ "days": 7, "note": "tester" })),
            Some("o@x.co"),
        )
        .await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["extended"], false);
    assert!(w.db.pro_sub_for_install("i1").unwrap().is_some(), "the account's PC has Pro");
    let (_, me) = w.api(Method::GET, "/v1/account", Some(&app), None).await;
    assert_eq!((me["plan"].as_str(), me["pro"]["source"].as_str()), (Some("pro"), Some("grant")));
    let (_, v) = w
        .admin(Method::POST, &format!("/admin/v1/accounts/{id}/grant"), Some(json!({ "days": 3 })), Some("o@x.co"))
        .await;
    assert_eq!(v["extended"], true);
    let (_, v) = w.admin(Method::GET, &format!("/admin/v1/accounts/{id}"), None, None).await;
    assert_eq!(v["plan"], "pro");
    assert_eq!(v["grants"].as_array().unwrap().len(), 1);
    assert_eq!(v["grants"][0]["history"].as_array().unwrap().len(), 2);
    assert_eq!(v["identities"][0]["provider"], "email");
    assert_eq!(v["installs"][0]["signed_in"], true);
    assert!(v["installs"][0]["install"].as_str().unwrap().ends_with('…'), "masked");
    let (s, _) = w.admin(Method::POST, &format!("/admin/v1/accounts/{id}/revoke"), None, Some("o@x.co")).await;
    assert_eq!(s, 200);
    let (s, v) = w.admin(Method::POST, &format!("/admin/v1/accounts/{id}/revoke"), None, Some("o@x.co")).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("already_ended")));
    assert!(w.db.pro_sub_for_install("i1").unwrap().is_none());
    // Sign out everywhere, disable, enable.
    let (s, v) = w.admin(Method::POST, &format!("/admin/v1/accounts/{id}/sign-out"), None, Some("o@x.co")).await;
    assert_eq!((s, v["sessions_ended"].as_i64()), (200, Some(1)));
    let (s, _) = w.api(Method::GET, "/v1/account", Some(&app), None).await;
    assert_eq!(s, 401);
    let (s, v) = w
        .admin(
            Method::POST,
            &format!("/admin/v1/accounts/{id}/disable"),
            Some(json!({ "reason": "spam" })),
            Some("o@x.co"),
        )
        .await;
    assert_eq!((s, v["disabled"].as_bool()), (200, Some(true)));
    let (_, v) = w.admin(Method::GET, "/admin/v1/accounts?status=disabled", None, None).await;
    assert_eq!(v["total"], 1);
    let (s, v) = w.admin(Method::POST, &format!("/admin/v1/accounts/{id}/enable"), None, Some("o@x.co")).await;
    assert_eq!((s, v["disabled"].as_bool()), (200, Some(false)));
    let audited: i64 =
        w.db.c().query_row("SELECT COUNT(*) FROM admin_audit WHERE action LIKE 'account.%'", [], |r| r.get(0)).unwrap();
    assert_eq!(audited, 6, "grant twice, revoke, sign-out, disable, enable");
    let (s, _) = w.admin(Method::GET, "/admin/v1/accounts/ac_nope", None, None).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn a_paying_account_gets_no_free_grant_and_stats_count_conversion() {
    let w = world().await;
    let h = crate::billing::email_hash(&email_key(), "payer@example.com");
    w.install("buyer");
    w.db.add_pro_sub("sub_Pay00000000001", "monthly", &h, "created", "buyer").unwrap();
    w.set_sub("sub_Pay00000000001", "active", Some(now_ms() + 10 * DAY));
    w.db.c()
        .execute(
            "INSERT INTO pro_payments(payment_id, sub_id, amount, at) VALUES ('pay_1', 'sub_Pay00000000001', 19900, 1)",
            [],
        )
        .unwrap();
    w.app_sign_in(&w.install("i1"), "payer@example.com").await;
    w.app_sign_in(&w.install("i2"), "free@example.com").await;
    let (_, v) = w.admin(Method::GET, "/admin/v1/accounts?plan=pro", None, None).await;
    assert_eq!(v["total"], 1);
    let id = v["items"][0]["id"].as_str().unwrap().to_string();
    let (s, v) = w
        .admin(Method::POST, &format!("/admin/v1/accounts/{id}/grant"), Some(json!({ "days": 7 })), Some("o@x.co"))
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("has_subscription")));
    let (s, v) = w.admin(Method::GET, "/admin/v1/accounts/stats?days=7", None, None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["accounts"]["total"], 2);
    assert_eq!(v["accounts"]["active_7d"], 2);
    assert_eq!(v["signups"]["per_day"].as_array().unwrap().len(), 7);
    assert_eq!(v["signups"]["per_day"][6]["count"], 2);
    assert_eq!(v["pro"]["accounts_paying_now"], 1);
    assert_eq!(v["conversion"]["paid_ever"], 1);
    assert_eq!(v["conversion"]["rate_all_time"], 0.5);
    let (s, _) = w.admin(Method::GET, "/admin/v1/accounts/stats?days=0", None, None).await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn login_pages_have_the_board_look_and_resend_works() {
    let w = world().await;
    let mut b = Browser::default();
    let (page, _) = w.open_login(&mut b, "next=/app/").await;
    assert_eq!(page.status, 200);
    assert!(page.body.contains("Continue with Google") && page.body.contains("Email me a link"));
    assert!(page.body.contains("class=\"btn primary\"") && page.body.contains("prefers-color-scheme:light"));
    // The age line and the legal links under the form.
    assert!(page.body.contains("you’re 18 or older") && page.body.contains("/terms/\">Terms</a>"));
    let (sent, _, _, _) = w.ask_email(&mut b, "kabir@example.com", "").await;
    assert_eq!(sent.status, 200);
    assert!(sent.body.contains("Check your email") && sent.body.contains("Use another email"));
    // Resend posts the same form again, with this browser's token and the address.
    assert!(sent.body.contains("Resend link") && sent.body.contains("name=email value=\"kabir@example.com\""));
    let token = b.cookies.get(LOGIN_COOKIE).map(|c| form_token(c)).unwrap();
    let again = w
        .req(
            &mut b,
            Method::POST,
            "/login/email",
            Some(&[("csrf", &token), ("email", "kabir@example.com"), ("flow", "")]),
            &[],
        )
        .await;
    assert_eq!(again.status, 200);
    if let Ok(dir) = std::env::var("FC_DUMP_LOGIN_PAGES") {
        std::fs::write(format!("{dir}/login.html"), &page.body).unwrap();
        std::fs::write(format!("{dir}/check.html"), &sent.body).unwrap();
        let gone = w.req(&mut b, Method::GET, "/login/link?t=nope", None, &[]).await;
        std::fs::write(format!("{dir}/expired.html"), &gone.body).unwrap();
    }
}

// ── Sign-in from the website (JSON, plan §9) ─────────────────────────────────────────

/// The last email's link token and code, for the website's link format.
fn web_link_and_code(w: &World) -> (String, String) {
    let m = w.mail.last().unwrap();
    assert!(m.text.contains(&format!("{WEB}/login/link/#t=")), "{}", m.text);
    let t = m.text.split("#t=").nth(1).unwrap().split_whitespace().next().unwrap().to_string();
    let code = m.text.split("page: ").nth(1).unwrap().split_whitespace().next().unwrap().replace('-', "");
    (t, code)
}

#[tokio::test]
async fn the_website_signs_in_by_code_and_by_link() {
    let w = world_cfg(true, RemoteLogin::Optional, Some(WEB)).await;
    let mut b = Browser::default();
    let (r, h) = w.web_json(&mut b, Method::GET, "/v1/login?next=/app/%23/home", None).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let v = r.json();
    assert_eq!(
        (v["google"].as_bool(), v["app"].as_bool(), v["signed_in"].as_bool()),
        (Some(true), Some(false), Some(false))
    );
    assert_eq!(v["csrf"].as_str(), b.cookies.get(LOGIN_COOKIE).map(|c| form_token(c)).as_deref());
    assert_eq!(h["access-control-allow-origin"], WEB);
    assert_eq!(h["access-control-allow-credentials"], "true");
    assert_eq!(h["cache-control"], "no-store");
    let (r, _) = w
        .web_json(
            &mut b,
            Method::POST,
            "/v1/login/email",
            Some(json!({ "email": " Ana@Example.com ", "next": "/app/" })),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let mid = r.json()["magic_id"].as_str().unwrap().to_string();
    let (_, code) = web_link_and_code(&w);
    // A wrong code says how many tries are left; the right one signs in.
    let (r, _) =
        w.web_json(&mut b, Method::POST, "/v1/login/code", Some(json!({ "magic_id": mid, "code": "ZZZZZZZZ" }))).await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str()), (422, Some("wrong_code")), "{}", r.body);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("4 tries left"));
    let (r, _) =
        w.web_json(&mut b, Method::POST, "/v1/login/code", Some(json!({ "magic_id": mid, "code": code }))).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!((r.json()["done"].as_str(), r.json()["next"].as_str()), (Some("web"), Some("/app/")));
    assert!(b.cookies.contains_key(SESSION_COOKIE) && !b.cookies.contains_key(LOGIN_COOKIE));
    let (me, h) = w.web_json(&mut b, Method::GET, "/v1/account", None).await;
    assert_eq!((me.status, me.json()["account"]["email"].as_str()), (200, Some("ana@example.com")), "{}", me.body);
    assert_eq!(h["access-control-allow-origin"], WEB);
    // Signed in: the start says so.
    let (r, _) = w.web_json(&mut b, Method::GET, "/v1/login", None).await;
    assert_eq!(r.json()["signed_in"].as_bool(), Some(true));

    // The link, in a second browser, works once.
    w.state.accounts.limits.lock().unwrap().clear();
    let mut b2 = Browser::default();
    w.web_json(&mut b2, Method::GET, "/v1/login", None).await;
    let (r, _) = w.web_json(&mut b2, Method::POST, "/v1/login/email", Some(json!({ "email": "bo@example.com" }))).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let (t, _) = web_link_and_code(&w);
    // Another browser (no login cookie of its own) can't use it.
    let mut b3 = Browser::default();
    w.web_json(&mut b3, Method::GET, "/v1/login", None).await;
    let (r, _) = w.web_json(&mut b3, Method::POST, "/v1/login/link", Some(json!({ "t": t }))).await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str()), (403, Some("other_browser")), "{}", r.body);
    let (r, _) = w.web_json(&mut b2, Method::POST, "/v1/login/link", Some(json!({ "t": t }))).await;
    assert_eq!((r.status, r.json()["done"].as_str()), (200, Some("web")), "{}", r.body);
    let (r, _) = w.web_json(&mut b2, Method::POST, "/v1/login/link", Some(json!({ "t": t }))).await;
    assert_eq!(r.status, 400, "the login cookie is gone after signing in: {}", r.body);
}

#[tokio::test]
async fn the_website_sign_in_needs_its_origin_the_cookie_and_the_token() {
    let w = world_cfg(true, RemoteLogin::Optional, Some(WEB)).await;
    let mut b = Browser::default();
    w.web_json(&mut b, Method::GET, "/v1/login", None).await;
    let c = b.cookies.get(LOGIN_COOKIE).unwrap().clone();
    let post = |origin: &'static str, token: Option<String>, site: &'static str| {
        let mut r = w
            .http
            .post(format!("{}/v1/login/email", w.base))
            .header("cookie", format!("{LOGIN_COOKIE}={c}"))
            .header("origin", origin)
            .header("sec-fetch-site", site)
            .json(&json!({ "email": "x@example.com" }));
        if let Some(t) = token {
            r = r.header("x-csrf-token", t);
        }
        r.send()
    };
    let r = post(WEB, None, "same-site").await.unwrap();
    assert_eq!(r.status(), 403);
    let r = post("https://evil.example", Some(form_token(&c)), "cross-site").await.unwrap();
    assert_eq!(r.status(), 403);
    assert!(r.headers().get("access-control-allow-origin").is_none(), "no CORS for other sites");
    let r = post(WEB, Some(form_token(&c)), "same-site").await.unwrap();
    assert_eq!(r.status(), 200);
    // Preflights: answered for the website only.
    let pre = |origin: &'static str| {
        w.http
            .request(Method::OPTIONS, format!("{}/v1/login/email", w.base))
            .header("origin", origin)
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type,x-csrf-token")
            .send()
    };
    let r = pre(WEB).await.unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(r.headers()["access-control-allow-origin"], WEB);
    assert!(r.headers()["access-control-allow-headers"].to_str().unwrap().contains("X-CSRF-Token"));
    let r = pre("https://evil.example").await.unwrap();
    assert!(r.headers().get("access-control-allow-origin").is_none());
    // Without a website set, a same-site caller isn't trusted.
    let plain = world().await;
    let mut b = Browser::default();
    let (r, h) = plain.web_json(&mut b, Method::GET, "/v1/login", None).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(h.get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn the_app_signs_in_through_the_website_and_lands_on_its_loopback() {
    let w = world_cfg(true, RemoteLogin::Optional, Some(WEB)).await;
    let auth = w.install("in_web1");
    let verifier = crate::random(40);
    let challenge = b64(&rsa::sha2::Sha256::digest(verifier.as_bytes()));
    let (s, v) = w
        .api(
            Method::POST,
            "/v1/account/app/start",
            Some(&auth),
            Some(json!({ "redirect_uri": "http://127.0.0.1:7878/auth/callback", "code_challenge": challenge,
                         "code_challenge_method": "S256", "state": "state-12345", "label": "Studio PC" })),
        )
        .await;
    assert_eq!(s, 201, "{v}");
    let flow = v["flow_id"].as_str().unwrap().to_string();
    assert_eq!(v["url"].as_str().unwrap(), format!("{WEB}/login/?flow={flow}"), "the website's sign-in page");
    let mut b = Browser::default();
    let (r, _) = w.web_json(&mut b, Method::GET, &format!("/v1/login?flow={flow}"), None).await;
    assert_eq!((r.json()["app"].as_bool(), r.json()["signed_in"].as_bool()), (Some(true), Some(false)));
    let (r, _) = w
        .web_json(&mut b, Method::POST, "/v1/login/email", Some(json!({ "email": "cy@example.com", "flow": flow })))
        .await;
    let mid = r.json()["magic_id"].as_str().unwrap().to_string();
    let (_, code) = web_link_and_code(&w);
    let (r, _) =
        w.web_json(&mut b, Method::POST, "/v1/login/code", Some(json!({ "magic_id": mid, "code": code }))).await;
    assert_eq!((r.status, r.json()["done"].as_str()), (200, Some("app")), "{}", r.body);
    let next = r.json()["next"].as_str().unwrap().to_string();
    assert!(next.starts_with("http://127.0.0.1:7878/auth/callback?code="), "{next}");
    assert!(!b.cookies.contains_key(SESSION_COOKIE), "no web session for an app sign-in");
    // An expired or unknown flow is a plain 410.
    let (r, _) = w.web_json(&mut b, Method::GET, "/v1/login?flow=nope", None).await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str()), (410, Some("flow_expired")));
}

#[tokio::test]
async fn google_from_the_website_comes_back_to_its_dashboard() {
    let w = world_cfg(true, RemoteLogin::Optional, Some(WEB)).await;
    let mut b = Browser::default();
    w.web_json(&mut b, Method::GET, "/v1/login", None).await;
    let (r, _) = w.web_json(&mut b, Method::POST, "/v1/login/google", Some(json!({ "next": "/app/#/insights" }))).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let url = url::Url::parse(r.json()["url"].as_str().unwrap()).unwrap();
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(q["redirect_uri"], format!("{}/login/google/callback", w.base), "Google comes back to the API host");
    *w.google.next_token.lock().unwrap() = Some(id_token(&claims(&q["nonce"], "dee@example.com", true), "k1", "RS256"));
    let cb =
        w.req(&mut b, Method::GET, &format!("/login/google/callback?state={}&code=c1", q["state"]), None, &[]).await;
    assert_eq!(cb.status, 303, "{}", cb.body);
    assert_eq!(cb.location.as_deref(), Some(&*format!("{WEB}/app/#/insights")));
    assert!(b.cookies.contains_key(SESSION_COOKIE));
    // A stale state goes back to the website's sign-in page with the reason.
    let cb =
        w.req(&mut b, Method::GET, &format!("/login/google/callback?state={}&code=c1", q["state"]), None, &[]).await;
    assert_eq!(cb.status, 303);
    assert!(cb.location.as_deref().unwrap().starts_with(&format!("{WEB}/login/?error=")), "{:?}", cb.location);
}
