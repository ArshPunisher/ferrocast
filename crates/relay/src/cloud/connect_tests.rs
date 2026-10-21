//! Connecting Twitch and Kick from the web (docs/CLOUD.md §4) through the real router: the
//! single-use state, the hand-over to the web host, the finish that binds only for the same
//! signed-in account, the subscriptions made and deleted, and a channel moving to its newer,
//! proven owner.

use reqwest::Method;
use serde_json::{Value, json};

use super::connect::{KICK_EVENTS, TWITCH_KINDS, TWITCH_SCOPES};
use super::tests::{KICK_CHANNEL, User, World, world};
use crate::bot::tests::STREAMER_ID;
use crate::bot::{TokenInfo, UserTokens};

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

fn state_of(url: &str) -> String {
    url::Url::parse(url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string()
}

impl World {
    /// Twitch takes `code` for the streamer's account with these scopes.
    fn twitch_code(&self, code: &str, refresh: &str, scopes: &[&str]) {
        let info = TokenInfo {
            user_id: STREAMER_ID.into(),
            login: "asha_live".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            client_id: "cloud-client".into(),
        };
        let tokens = UserTokens {
            access_token: format!("at-{code}"),
            refresh_token: refresh.into(),
            scopes: info.scopes.clone(),
        };
        self.tw.with(|t| t.codes.insert(code.into(), (tokens, info)));
    }

    /// Connect pressed, Twitch said yes: where the browser is sent next (the finish URL).
    async fn twitch_until_finish(&self, u: &User, code: &str) -> String {
        let (s, v) = self.web(u, Method::POST, "/c/api/v1/accounts/twitch/connect", None).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["flow"], "browser");
        let url = v["url"].as_str().unwrap();
        assert!(url.starts_with("https://id.twitch.tv/oauth2/authorize?"), "{url}");
        assert!(url.contains("moderator%3Aread%3Afollowers"), "{url}");
        let state = state_of(url);
        assert!(state.starts_with("cl."));
        self.twitch_code(code, &format!("rt-{code}"), TWITCH_SCOPES);
        let res = self
            .http
            .get(format!("{}/v1/bot/twitch/callback", self.base))
            .query(&[("code", code), ("state", state.as_str())])
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 303);
        let to = res.headers()["location"].to_str().unwrap().to_string();
        assert!(to.starts_with(&format!("{}/c/v1/connect/finish?code=", self.base)), "{to}");
        to
    }

    /// Opens the finish link in `u`'s browser (or a signed-out one).
    async fn open_finish(&self, u: Option<&User>, to: &str) -> reqwest::Response {
        let mut r = self.http.get(to);
        if let Some(u) = u {
            r = r.header("cookie", &u.cookie);
        }
        r.send().await.unwrap()
    }

    async fn connect_twitch_for(&self, u: &User, code: &str) {
        let to = self.twitch_until_finish(u, code).await;
        let res = self.open_finish(Some(u), &to).await;
        assert_eq!(res.status(), 303);
        assert_eq!(res.headers()["location"], "/app/#/cloud?connected=twitch");
    }
}

#[tokio::test]
async fn twitch_connects_for_the_same_signed_in_account_and_subscribes() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    w.connect_twitch_for(&u, "c1").await;

    let ch = w.db.cloud_account_channel(&u.account.id, "twitch").unwrap().unwrap();
    assert_eq!((ch.channel_id.as_str(), ch.login.as_str()), (STREAMER_ID, "asha_live"));
    assert!(ch.error.is_none(), "{:?}", ch.error);
    assert_eq!(ch.event_subs.len(), TWITCH_KINDS.len());
    let kinds: Vec<String> = w.tw.with(|t| t.subscribed.iter().map(|s| s.0.clone()).collect());
    for k in TWITCH_KINDS {
        assert!(kinds.iter().any(|x| x == k), "{k} not subscribed");
    }
    let follow = w.tw.with(|t| t.subscribed.iter().find(|s| s.0 == "channel.follow").unwrap().1.clone());
    assert_eq!(follow["moderator_user_id"], STREAMER_ID);
    let raid = w.tw.with(|t| t.subscribed.iter().find(|s| s.0 == "channel.raid").unwrap().1.clone());
    assert_eq!(raid["to_broadcaster_user_id"], STREAMER_ID);
    // The cloud bot's channel: the streamer's own account speaks, the sign-in sealed.
    let bot = w.db.bot_channel(&u.account.id, "twitch").unwrap().unwrap();
    assert_eq!(bot.bot_id.as_deref(), Some(STREAMER_ID));
    assert_eq!(bot.channel_id.as_deref(), Some(STREAMER_ID));
    assert_eq!(crate::bot::open_token(&w.state, &u.account.id, "twitch", "bot").as_deref(), Some("rt-c1"));
    let raw: String =
        w.db.c().query_row("SELECT hex(ct) FROM bot_tokens WHERE sub_id = ?1", [&u.account.id], |r| r.get(0)).unwrap();
    assert!(!raw.contains(&hex("rt-c1")), "the refresh token is sealed");

    let (s, v) = w.web(&u, Method::GET, "/c/api/v1/accounts", None).await;
    assert_eq!(s, 200);
    assert_eq!(v[0]["platform"], "twitch");
    assert_eq!(v[0]["name"], "asha_live");
    assert_eq!(v[0]["scopes_ok"], true);
    let (_, st) = w.web(&u, Method::GET, "/c/api/v1/status", None).await;
    assert_eq!(st["connectors"]["twitch"]["state"], "connected");

    // A finish link can't be used twice.
    let to = w.twitch_until_finish(&u, "c2").await;
    assert_eq!(w.open_finish(Some(&u), &to).await.status(), 303);
    let again = w.open_finish(Some(&u), &to).await;
    assert_eq!(again.status(), 400);

    // Disconnect: subscriptions deleted, sign-in revoked at Twitch, rows gone.
    let subs = w.db.cloud_account_channel(&u.account.id, "twitch").unwrap().unwrap().event_subs;
    let (s, _) = w.web(&u, Method::DELETE, "/c/api/v1/accounts/twitch", None).await;
    assert_eq!(s, 204);
    let gone = w.tw.with(|t| t.unsubscribed.clone());
    for id in &subs {
        assert!(gone.contains(id), "{id} not deleted");
    }
    assert!(w.tw.with(|t| !t.revoked.is_empty()));
    assert!(w.db.cloud_account_channel(&u.account.id, "twitch").unwrap().is_none());
    assert!(w.db.bot_channel(&u.account.id, "twitch").unwrap().is_none());
    let (s, v) = w.web(&u, Method::DELETE, "/c/api/v1/accounts/twitch", None).await;
    assert_eq!((s, code(&v)), (404, "not_found"));
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02X}")).collect()
}

#[tokio::test]
async fn a_finish_in_another_browser_binds_nothing_and_revokes() {
    let w = world().await;
    let a = w.cloud_user("a@example.com").await;
    let b = w.cloud_user("b@example.com").await;

    // Signed in to another account: nothing bound for either, the sign-in revoked.
    let to = w.twitch_until_finish(&a, "c1").await;
    let res = w.open_finish(Some(&b), &to).await;
    assert_eq!(res.status(), 403);
    assert!(res.text().await.unwrap().contains("another Ferrocast account"));
    assert!(w.db.cloud_account_channel(&a.account.id, "twitch").unwrap().is_none());
    assert!(w.db.cloud_account_channel(&b.account.id, "twitch").unwrap().is_none());
    assert!(w.tw.with(|t| t.revoked.contains(&"at-c1".to_string())));
    // And the link is spent: the right browser can't use it after.
    assert_eq!(w.open_finish(Some(&a), &to).await.status(), 400);

    // Signed out: the same.
    let to = w.twitch_until_finish(&a, "c2").await;
    assert_eq!(w.open_finish(None, &to).await.status(), 401);
    assert!(w.db.cloud_account_channel(&a.account.id, "twitch").unwrap().is_none());
    assert!(w.tw.with(|t| t.revoked.contains(&"at-c2".to_string())));
}

#[tokio::test]
async fn refusals_scopes_states_and_youtube() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    // Missing scopes: refused, revoked, nothing pending.
    let (_, v) = w.web(&u, Method::POST, "/c/api/v1/accounts/twitch/connect", None).await;
    let state = state_of(v["url"].as_str().unwrap());
    w.twitch_code("few", "rt-few", &["user:read:chat"]);
    let res = w
        .http
        .get(format!("{}/v1/bot/twitch/callback", w.base))
        .query(&[("code", "few"), ("state", state.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert!(res.text().await.unwrap().contains("allow user:write:chat"));
    assert!(w.tw.with(|t| t.revoked.contains(&"at-few".to_string())));
    // The state was single use.
    let res = w
        .http
        .get(format!("{}/v1/bot/twitch/callback", w.base))
        .query(&[("code", "few"), ("state", state.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    // A made-up state.
    let res = w
        .http
        .get(format!("{}/v1/bot/twitch/callback", w.base))
        .query(&[("code", "x"), ("state", "cl.made-up")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    // YouTube in the cloud is Pro; unknown platforms don't exist.
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/accounts/youtube/connect", None).await;
    assert_eq!((s, code(&v)), (402, "pro_required"));
    assert!(v["error"]["hint"].is_string());
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/accounts/tiktok/connect", None).await;
    assert_eq!((s, code(&v)), (404, "not_found"));
    // Cloud mode off: connect is refused.
    let off = w.user("off@example.com");
    let (s, v) = w.web(&off, Method::POST, "/c/api/v1/accounts/twitch/connect", None).await;
    assert_eq!((s, code(&v)), (409, "cloud_off"));
    // A finish with a made-up code.
    let res = w.open_finish(Some(&u), &format!("{}/c/v1/connect/finish?code=nope", w.base)).await;
    assert_eq!(res.status(), 400);
}

#[tokio::test]
async fn a_channel_moves_to_its_newer_proven_account() {
    let w = world().await;
    let a = w.cloud_user("a@example.com").await;
    let b = w.cloud_user("b@example.com").await;
    w.connect_twitch_for(&a, "c1").await;
    let old_subs = w.db.cloud_account_channel(&a.account.id, "twitch").unwrap().unwrap().event_subs;
    w.connect_twitch_for(&b, "c2").await;
    assert!(w.db.cloud_account_channel(&a.account.id, "twitch").unwrap().is_none());
    assert!(w.db.bot_channel(&a.account.id, "twitch").unwrap().is_none());
    let ch = w.db.cloud_account_channel(&b.account.id, "twitch").unwrap().unwrap();
    assert_eq!(ch.channel_id, STREAMER_ID);
    let gone = w.tw.with(|t| t.unsubscribed.clone());
    assert!(old_subs.iter().all(|id| gone.contains(id)));
    // Forgotten, not revoked: it's the same person's Twitch, and a revoke could end B's.
    assert!(!w.tw.with(|t| t.revoked.contains(&"at-c2".to_string())));
}

#[tokio::test]
async fn kick_connects_and_subscribes_with_the_streamers_sign_in() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/accounts/kick/connect", None).await;
    assert_eq!(s, 200, "{v}");
    let url = v["url"].as_str().unwrap();
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    let state = state_of(url);
    let res = w
        .http
        .get(format!("{}/v1/kick/callback", w.base))
        .query(&[("code", "k1"), ("state", state.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 303);
    let to = res.headers()["location"].to_str().unwrap().to_string();
    let res = w.open_finish(Some(&u), &to).await;
    assert_eq!(res.status(), 303, "{}", res.text().await.unwrap_or_default());

    let ch = w.db.cloud_account_channel(&u.account.id, "kick").unwrap().unwrap();
    assert_eq!((ch.channel_id.as_str(), ch.login.as_str()), (KICK_CHANNEL, "asha_kick"));
    assert_eq!(ch.avatar_url.as_deref(), Some("https://kick.test/a.png"));
    assert_eq!(ch.event_subs.len(), KICK_EVENTS.len());
    let made = w.kick_web.subscribed.lock().unwrap().clone();
    assert_eq!(made.len(), 1);
    assert_eq!(made[0]["broadcaster_user_id"], 668);
    assert_eq!(made[0]["method"], "webhook");
    let names: Vec<&str> = made[0]["events"].as_array().unwrap().iter().filter_map(|e| e["name"].as_str()).collect();
    assert_eq!(names, KICK_EVENTS);
    assert_eq!(crate::bot::open_token(&w.state, &u.account.id, "kick", "bot").as_deref(), Some("kick-rt"));
    let (_, list) = w.web(&u, Method::GET, "/c/api/v1/accounts", None).await;
    assert_eq!(list[0]["platform"], "kick");
    assert_eq!(list[0]["scopes_ok"], true);

    let (s, _) = w.web(&u, Method::DELETE, "/c/api/v1/accounts/kick", None).await;
    assert_eq!(s, 204);
    assert_eq!(w.kick_web.deleted.lock().unwrap().len(), KICK_EVENTS.len());
    assert!(w.kk.with(|k| k.revoked.contains(&"kick-rt".to_string())));
    assert!(w.db.cloud_account_channel(&u.account.id, "kick").unwrap().is_none());
    let _ = json!(null);
}

#[tokio::test]
async fn an_unfinished_hand_over_is_revoked_by_the_sweep() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let _to = w.twitch_until_finish(&u, "c1").await;
    w.db.c().execute("UPDATE cloud_pending SET expires_at = 0", []).unwrap();
    super::sweep(&w.state).await;
    assert!(w.tw.with(|t| t.revoked.contains(&"at-c1".to_string())));
    let left: i64 = w.db.c().query_row("SELECT COUNT(*) FROM cloud_pending", [], |r| r.get(0)).unwrap();
    assert_eq!(left, 0);
}
