//! YouTube connected on the web without a PC, through the real `router()` with a fake
//! Google: Pro required, the single-use state, the hand-over to the web host, the finish
//! that binds only for the same signed-in account (anyone else's browser binds nothing and
//! the grant is given back), the cloud bot's switch, and disconnect.

use std::sync::Arc;

use reqwest::Method;
use serde_json::{Value, json};

use super::*;
use crate::accounts::{AccountsConfig, RemoteLogin, csrf_token};
use crate::billing::store::Synced;
use crate::bot::Runtime;
use crate::bot::tests::vault;
use crate::bot::youtube_tests::{FakeYoutube, yt_cfg};
use crate::remote::RemoteConfig;
use crate::{Config, Db, router};

const DAY: i64 = 86_400_000;
const CHANNEL: &str = "UCweb00000000000000000";

struct World {
    base: String,
    db: Arc<Db>,
    yt: Arc<FakeYoutube>,
    state: AppState,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

/// A signed-in browser: its cookie and CSRF token.
struct User {
    account: String,
    cookie: String,
    csrf: String,
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let yt = Arc::new(FakeYoutube::default());
    let rt = Runtime::new(None, None, None)
        .with_youtube(Some(yt_cfg(1_500, 9_000)), Some(yt.clone() as Arc<dyn super::super::YoutubeCloud>));
    let state = AppState::new(cfg, db)
        .with_vault(Some(vault()))
        .with_bot(rt)
        .with_remote(RemoteConfig::new(Some(&base), Some(b"pepper-pepper-pepper-pepper-32b!"), None))
        .with_accounts(AccountsConfig { google: Default::default(), remote_login: RemoteLogin::Required });
    let db = state.db.clone();
    let app = router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    World { base, db, yt, state, http, _dir: dir }
}

impl World {
    fn user(&self, email: &str, pro: bool) -> User {
        let (a, _) = self.db.account_sign_in(email, None, now_ms()).unwrap();
        let (token, _) = crate::accounts::new_session(&self.state, &a, "web", "Chrome", None).unwrap();
        if pro {
            let sub = format!("sub_{}", &a.id[3..]);
            self.db.add_pro_sub_unlinked(&sub, "monthly", "e", "created").unwrap();
            let s = Synced { status: "active".into(), current_end: Some(now_ms() + 30 * DAY), cancel_at_end: None };
            self.db.sync_pro_sub(None, &sub, &s).unwrap();
            self.db.account_attach_sub(&a.id, &sub, "checkout", now_ms()).unwrap();
        }
        let secret = token.split_once('.').unwrap().1.to_string();
        User {
            account: a.id,
            cookie: format!("{}={token}", crate::accounts::SESSION_COOKIE),
            csrf: csrf_token(&secret),
        }
    }

    async fn web(&self, u: Option<&User>, m: Method, path: &str, body: Option<Value>, csrf: bool) -> (u16, Value) {
        let mut r = self.http.request(m, format!("{}{path}", self.base));
        if let Some(u) = u {
            r = r.header("cookie", &u.cookie);
            if csrf {
                r = r.header("x-csrf-token", &u.csrf);
            }
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }

    /// Google says yes to `code`: the channel and a grant.
    fn google_code(&self, code: &str, scopes: &[&str]) {
        let t = GoogleTokens {
            access_token: format!("at-{code}"),
            refresh_token: format!("rt-{code}"),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            expires_in: 3_600,
        };
        self.yt.with(|y| {
            y.codes.insert(code.into(), t);
            y.channels.insert(format!("at-{code}"), ChannelInfo { id: CHANNEL.into(), title: "Asha Live".into() });
        });
    }

    /// Connect pressed, Google said yes: the finish link the browser is sent to.
    async fn until_finish(&self, u: &User, code: &str) -> String {
        let (s, v) = self.web(Some(u), Method::POST, "/r/v1/pro/youtube/connect", Some(json!({})), true).await;
        assert_eq!(s, 200, "{v}");
        let url = v["url"].as_str().unwrap();
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"), "{url}");
        let state = url::Url::parse(url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
        self.google_code(code, SCOPES);
        let res = self
            .http
            .get(format!("{}/v1/bot/youtube/callback", self.base))
            .query(&[("code", code), ("state", state.as_str())])
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 303);
        let to = res.headers()["location"].to_str().unwrap().to_string();
        assert!(to.starts_with(&format!("{}/r/v1/pro/youtube/finish?code=", self.base)), "{to}");
        to
    }

    async fn open(&self, u: Option<&User>, to: &str) -> reqwest::Response {
        let mut r = self.http.get(to);
        if let Some(u) = u {
            r = r.header("cookie", &u.cookie);
        }
        r.send().await.unwrap()
    }

    fn revoked(&self) -> Vec<String> {
        self.yt.with(|y| y.revoked.clone())
    }
}

#[tokio::test]
async fn signed_out_or_free_gets_a_clear_answer() {
    let w = world().await;
    let (s, v) = w.web(None, Method::GET, "/r/v1/pro/youtube", None, false).await;
    assert_eq!((s, v["error"]["code"].as_str()), (401, Some("signed_out")), "{v}");
    let free = w.user("free@example.com", false);
    let (s, v) = w.web(Some(&free), Method::GET, "/r/v1/pro/youtube", None, false).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["available"].as_bool(), v["pro"].as_bool()), (Some(true), Some(false)));
    let (s, v) = w.web(Some(&free), Method::POST, "/r/v1/pro/youtube/connect", None, true).await;
    assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")), "{v}");
    assert!(v["error"]["hint"].as_str().unwrap().contains("free"));
    let pro = w.user("pro@example.com", true);
    let (s, v) = w.web(Some(&pro), Method::POST, "/r/v1/pro/youtube/connect", None, false).await;
    assert_eq!((s, v["error"]["code"].as_str()), (403, Some("csrf")), "no CSRF token: {v}");
}

#[tokio::test]
async fn the_channel_binds_only_for_the_same_signed_in_pro_account() {
    let w = world().await;
    let u = w.user("asha@example.com", true);
    let other = w.user("other@example.com", true);
    // A signed-out browser: nothing bound, the grant given back.
    let to = w.until_finish(&u, "c1").await;
    let res = w.open(None, &to).await;
    assert_eq!(res.status(), 403);
    assert!(res.text().await.unwrap().contains("Nothing was connected"));
    assert_eq!(w.revoked(), ["rt-c1"]);
    // Someone else's browser: the same.
    let to = w.until_finish(&u, "c2").await;
    assert_eq!(w.open(Some(&other), &to).await.status(), 403);
    assert_eq!(w.revoked(), ["rt-c1", "rt-c2"]);
    let sub = format!("sub_{}", &u.account[3..]);
    assert!(w.db.bot_channel(&sub, "youtube").unwrap().is_none());
    // The same account: bound to its Pro subscription, the sign-in sealed.
    let to = w.until_finish(&u, "c3").await;
    let res = w.open(Some(&u), &to).await;
    assert_eq!(res.status(), 303);
    assert!(res.headers()["location"].to_str().unwrap().ends_with("/app/#/account?youtube=connected"));
    let ch = w.db.bot_channel(&sub, "youtube").unwrap().unwrap();
    assert_eq!((ch.channel_id.as_deref(), ch.connected_by.as_str()), (Some(CHANNEL), "web"));
    assert_eq!(crate::bot::open_token(&w.state, &sub, "youtube", "bot").as_deref(), Some("rt-c3"));
    assert_eq!(w.revoked().len(), 2, "the bound grant is kept");
    // The finish link works once.
    assert_eq!(w.open(Some(&u), &to).await.status(), 410);
    // The view shows it with today's quota.
    let (s, v) = w.web(Some(&u), Method::GET, "/r/v1/pro/youtube", None, false).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["channel"]["title"], "Asha Live");
    assert_eq!(v["channel"]["from_web"], true);
    assert_eq!(v["quota"]["limit"], 1_500);
    assert_eq!(v["bot_on"], false);
}

#[tokio::test]
async fn a_missing_scope_or_a_lapsed_pro_binds_nothing() {
    let w = world().await;
    let u = w.user("asha@example.com", true);
    let (_, v) = w.web(Some(&u), Method::POST, "/r/v1/pro/youtube/connect", None, true).await;
    let url = v["url"].as_str().unwrap();
    let state = url::Url::parse(url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    w.google_code("c1", &["openid"]);
    let res = w
        .http
        .get(format!("{}/v1/bot/youtube/callback", w.base))
        .query(&[("code", "c1"), ("state", state.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert!(res.text().await.unwrap().contains("tick every box"));
    assert_eq!(w.revoked(), ["rt-c1"]);
    // The state was single use.
    w.google_code("c2", SCOPES);
    let res = w
        .http
        .get(format!("{}/v1/bot/youtube/callback", w.base))
        .query(&[("code", "c2"), ("state", state.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    // Pro lapses between Google and the finish.
    let to = w.until_finish(&u, "c3").await;
    let sub = format!("sub_{}", &u.account[3..]);
    w.db.sync_pro_sub(None, &sub, &Synced { status: "halted".into(), current_end: None, cancel_at_end: None }).unwrap();
    assert_eq!(w.open(Some(&u), &to).await.status(), 402);
    assert!(w.revoked().contains(&"rt-c3".to_string()));
    assert!(w.db.bot_channel(&sub, "youtube").unwrap().is_none());
}

#[tokio::test]
async fn the_bot_switch_and_disconnect() {
    let w = world().await;
    let u = w.user("asha@example.com", true);
    let to = w.until_finish(&u, "c1").await;
    assert_eq!(w.open(Some(&u), &to).await.status(), 303);
    let (s, v) = w.web(Some(&u), Method::PUT, "/r/v1/pro/youtube/bot", Some(json!({ "enabled": true })), false).await;
    assert_eq!((s, v["error"]["code"].as_str()), (403, Some("csrf")));
    let (s, v) = w.web(Some(&u), Method::PUT, "/r/v1/pro/youtube/bot", Some(json!({})), true).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid")), "{v}");
    let (s, v) = w.web(Some(&u), Method::PUT, "/r/v1/pro/youtube/bot", Some(json!({ "enabled": true })), true).await;
    assert_eq!((s, v["bot_on"].as_bool()), (200, Some(true)), "{v}");
    let (s, v) = w.web(Some(&u), Method::DELETE, "/r/v1/pro/youtube", None, true).await;
    assert_eq!((s, v["channel"].clone()), (200, Value::Null), "{v}");
    assert_eq!(w.revoked(), ["rt-c1"], "revoked at Google");
    assert_eq!(v["bot_on"], false, "no platform left: the bot goes off");
}
