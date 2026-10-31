//! Guest cameras through the real `router()`: invites (made, capped, removed), a guest's offer
//! reaching a fake PC on a real link socket and its answer coming back, and every refusal.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::remote::RemoteConfig;
use crate::{AppState, Config, Db, router};

struct World {
    base: String,
    db: std::sync::Arc<Db>,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
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
    let rc = RemoteConfig::new(Some(&base), Some(b"test pepper, at least sixteen bytes"), None);
    let state = AppState::new(cfg, db).with_remote(rc);
    let db = state.db.clone();
    let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    World { base, db, http: reqwest::Client::new(), _dir: dir }
}

impl World {
    fn install(&self, id: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        format!("{id}.secret")
    }

    async fn post(&self, path: &str, auth: Option<&str>, body: Value) -> (u16, Value) {
        let mut r = self.http.post(format!("{}{path}", self.base)).json(&body);
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        let res = r.send().await.unwrap();
        (res.status().as_u16(), res.json().await.unwrap_or(Value::Null))
    }

    async fn invite(&self, auth: &str) -> String {
        let (s, v) = self.post("/v1/guests/invites", Some(auth), json!({})).await;
        assert_eq!(s, 201, "{v}");
        v["token"].as_str().unwrap().to_string()
    }

    /// A fake PC's link: answers each guest's offer with `answer(frame)`.
    async fn pc(&self, auth: &str, answer: fn(&Value) -> Value) {
        let mut req = format!("{}/v1/link", self.base.replace("http", "ws")).into_client_request().unwrap();
        req.headers_mut().insert("authorization", format!("Bearer {auth}").parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let (mut tx, mut rx) = ws.split();
        tokio::spawn(async move {
            // Pings and queued items pass by; guests' offers are answered.
            while let Some(Ok(m)) = rx.next().await {
                let WsMsg::Text(t) = m else { continue };
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "guest_offer" {
                    let _ = tx.send(WsMsg::text(answer(&v).to_string())).await;
                }
            }
        });
        // The link registers as the socket opens.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn offer(token: &str) -> Value {
    json!({ "token": token, "name": "  Asha\u{7} ", "offer": { "type": "offer", "sdp": "v=0\r\n" } })
}

#[tokio::test]
async fn a_guest_reaches_the_pc_and_gets_its_answer() {
    let w = world().await;
    let auth = w.install("pc1");
    let (s, v) = w.post("/v1/guests/invites", Some(&auth), json!({})).await;
    assert_eq!(s, 201);
    let token = v["token"].as_str().unwrap();
    assert_eq!(v["url"], format!("{}/app/guest.html#{token}", w.base), "the token rides after #");
    w.pc(&auth, |f| {
        // The PC sees the guest's name (cleaned), its own token and the offer.
        assert_eq!(f["name"], "Asha");
        assert_eq!(f["offer"]["sdp"], "v=0\r\n");
        json!({ "type": "guest_answer", "req": f["req"], "answer": { "type": "answer", "sdp": format!("v=0 for {}", f["token"].as_str().unwrap()) } })
    })
    .await;
    let (s, v) = w.post("/g/v1/offer", None, offer(token)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["answer"], json!({ "type": "answer", "sdp": format!("v=0 for {token}") }));
}

#[tokio::test]
async fn a_pc_that_says_no_is_heard_and_one_that_isnt_open_is_named() {
    let w = world().await;
    let auth = w.install("pc1");
    let token = w.invite(&auth).await;
    let (s, v) = w.post("/g/v1/offer", None, offer(&token)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("streamer_offline")));
    w.pc(&auth, |f| json!({ "type": "guest_answer", "req": f["req"], "error": "Guests are turned off." })).await;
    let (s, v) = w.post("/g/v1/offer", None, offer(&token)).await;
    assert_eq!((s, v["error"]["message"].as_str()), (409, Some("Guests are turned off.")));
}

#[tokio::test]
async fn only_the_inviting_pc_may_answer() {
    let w = world().await;
    let (a, b) = (w.install("pc1"), w.install("pc2"));
    let token = w.invite(&a).await;
    // pc2 can't see pc1's guests; pc1 is connected but stays silent.
    w.pc(&a, |_| json!({ "type": "nothing" })).await;
    w.pc(&b, |f| json!({ "type": "guest_answer", "req": f["req"], "answer": { "type": "answer", "sdp": "x" } })).await;
    let r = w.post("/g/v1/offer", None, offer(&token));
    let (s, v) = tokio::time::timeout(Duration::from_secs(25), r).await.unwrap();
    assert_eq!((s, v["error"]["code"].as_str()), (504, Some("no_answer")));
}

#[tokio::test]
async fn ended_and_broken_requests_are_refused_plainly() {
    let w = world().await;
    let auth = w.install("pc1");
    let token = w.invite(&auth).await;
    for (body, want) in [
        (json!({ "token": token, "name": "", "offer": { "type": "offer", "sdp": "v=0" } }), "Enter your name."),
        (
            json!({ "token": token, "name": "A", "offer": { "type": "answer", "sdp": "v=0" } }),
            "Your browser's connection details were missing or too long.",
        ),
        (
            json!({ "token": token, "name": "A", "offer": { "type": "offer", "sdp": "x".repeat(40_000) } }),
            "Your browser's connection details were missing or too long.",
        ),
        (json!({ "name": "A" }), "The link is incomplete."),
    ] {
        let (s, v) = w.post("/g/v1/offer", None, body).await;
        assert_eq!((s, v["error"]["message"].as_str()), (400, Some(want)));
    }
    let (s, v) = w.post("/g/v1/offer", None, offer("not-a-real-token")).await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("invite_ended")));
    let res = w.http.delete(format!("{}/v1/guests/invites/{token}", w.base)).bearer_auth(&auth).send().await.unwrap();
    assert_eq!(res.status().as_u16(), 204);
    let (s, _) = w.post("/g/v1/offer", None, offer(&token)).await;
    assert_eq!(s, 404, "a removed link has ended");
    let (s, _) = w.post("/v1/guests/invites", None, json!({})).await;
    assert_eq!(s, 401, "only an install makes invites");
}

#[tokio::test]
async fn invites_are_capped_and_offers_rate_limited() {
    let w = world().await;
    let auth = w.install("pc1");
    for _ in 0..crate::guests::MAX_INVITES {
        w.invite(&auth).await;
    }
    let (s, v) = w.post("/v1/guests/invites", Some(&auth), json!({})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("too_many_invites")));
    let mut last = 0;
    for _ in 0..11 {
        last = w.post("/g/v1/offer", None, offer("nobody")).await.0;
    }
    assert_eq!(last, 429, "the 11th try in a minute from one address waits");
}
