//! A fake Twitch: the ID server (device code, tokens, validate), Helix, and an EventSub
//! WebSocket the test drives with [`push`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::ws::{Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// The scopes the fake grants on a fresh sign-in (mirrors the app's list).
pub const SCOPES: &[&str] = &[
    "user:read:chat",
    "user:write:chat",
    "user:bot",
    "channel:bot",
    "moderator:read:followers",
    "moderator:manage:banned_users",
    "moderator:manage:chat_messages",
    "moderator:manage:chat_settings",
    "channel:read:subscriptions",
    "channel:read:redemptions",
    "channel:read:ads",
    "channel:moderate",
    "bits:read",
];

/// State of the fake Twitch, open for tests to inspect and steer.
#[derive(Debug, Default)]
pub struct Fake {
    pub polls: u32,
    pub refreshes: u32,
    pub fail_next_with_401: bool,
    pub subs: Vec<(String, String)>,
    pub chats: Vec<Value>,
    pub keepalive: u64,
    pub sessions: u32,
    pub sockets: HashMap<String, mpsc::UnboundedSender<String>>,
    pub base: String,
}

pub type St = Arc<Mutex<Fake>>;

/// Starts the fake. Returns its state and `(id, api, eventsub)` base URLs.
pub async fn fake() -> (St, (String, String, String)) {
    let st: St = Arc::new(Mutex::new(Fake { keepalive: 10, ..Default::default() }));
    let app = Router::new()
        .route("/oauth2/device", post(device))
        .route("/oauth2/token", post(token))
        .route("/oauth2/validate", get(validate))
        .route("/helix/users", get(users))
        .route("/helix/chat/messages", post(chat))
        .route("/helix/eventsub/subscriptions", post(subscribe))
        .route("/helix/moderation/bans", any(ok))
        .route("/helix/chat/settings", any(ok))
        .route("/ws", get(ws))
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    st.lock().unwrap().base = base.clone();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let ws = format!("{}/ws", base.replace("http", "ws"));
    (st, (base.clone(), format!("{base}/helix"), ws))
}

async fn device() -> Response {
    axum::Json(
        json!({ "device_code": "dev-1", "user_code": "ABCD-EFGH", "verification_uri": "https://www.twitch.tv/activate",
                       "expires_in": 1800, "interval": 1 }),
    )
    .into_response()
}

async fn token(State(st): State<St>, body: String) -> Response {
    let mut s = st.lock().unwrap();
    if body.contains("grant_type=refresh_token") {
        if body.contains("refresh_token=revoked") {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({ "status": 400, "message": "Invalid refresh token" })))
                .into_response();
        }
        s.refreshes += 1;
        let n = s.refreshes;
        return axum::Json(json!({ "access_token": format!("access-{n}"), "refresh_token": format!("refresh-{n}"),
                                  "expires_in": 14000, "scope": ["user:read:chat"], "token_type": "bearer" }))
        .into_response();
    }
    s.polls += 1;
    if s.polls == 1 {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "status": 400, "message": "authorization_pending" })))
            .into_response();
    }
    axum::Json(json!({ "access_token": "access-0", "refresh_token": "refresh-0", "expires_in": 14000,
                       "scope": SCOPES, "token_type": "bearer" }))
    .into_response()
}

fn check_auth(st: &St, headers: &axum::http::HeaderMap) -> bool {
    let mut s = st.lock().unwrap();
    if s.fail_next_with_401 && headers.get("authorization").and_then(|h| h.to_str().ok()) == Some("Bearer access-0") {
        s.fail_next_with_401 = false;
        return false;
    }
    true
}

async fn users(State(st): State<St>, headers: axum::http::HeaderMap) -> Response {
    if !check_auth(&st, &headers) {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({ "message": "Invalid OAuth token" }))).into_response();
    }
    axum::Json(json!({ "data": [{ "id": "111", "login": "streamer", "display_name": "Streamer", "profile_image_url": "https://x/p.png" }] }))
        .into_response()
}

async fn chat(State(st): State<St>, axum::Json(body): axum::Json<Value>) -> Response {
    let dropped = body["message"].as_str().unwrap_or("").contains("automod");
    st.lock().unwrap().chats.push(body);
    if dropped {
        return axum::Json(json!({ "data": [{ "message_id": "", "is_sent": false, "drop_reason": { "code": "x", "message": "AutoMod held it" } }] }))
            .into_response();
    }
    axum::Json(json!({ "data": [{ "message_id": "m", "is_sent": true }] })).into_response()
}

async fn subscribe(State(st): State<St>, axum::Json(body): axum::Json<Value>) -> Response {
    let session = body.pointer("/transport/session_id").and_then(Value::as_str).unwrap_or("").to_string();
    let kind = body["type"].as_str().unwrap_or("").to_string();
    if kind == "channel.ad_break.begin" {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({ "message": "subscription missing proper authorization: missing scope" })),
        )
            .into_response();
    }
    st.lock().unwrap().subs.push((session, kind));
    (StatusCode::ACCEPTED, axum::Json(json!({ "data": [] }))).into_response()
}

async fn ok() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn ws(State(st): State<St>, Query(_q): Query<HashMap<String, String>>, up: WebSocketUpgrade) -> Response {
    up.on_upgrade(move |socket| session(st, socket))
}

async fn session(st: St, mut socket: WebSocket) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (id, keepalive) = {
        let mut s = st.lock().unwrap();
        s.sessions += 1;
        let id = format!("session-{}", s.sessions);
        s.sockets.insert(id.clone(), tx);
        (id, s.keepalive)
    };
    let welcome = json!({ "metadata": { "message_id": "w", "message_type": "session_welcome" },
                          "payload": { "session": { "id": id, "keepalive_timeout_seconds": keepalive } } });
    if socket.send(AxMessage::Text(welcome.to_string().into())).await.is_err() {
        return;
    }
    while let Some(frame) = rx.recv().await {
        if socket.send(AxMessage::Text(frame.into())).await.is_err() {
            return;
        }
    }
}

pub fn push(st: &St, session: &str, frame: Value) {
    let tx = st.lock().unwrap().sockets.get(session).cloned().expect("no such session");
    tx.send(frame.to_string()).unwrap();
}

pub fn notification(kind: &str, id: &str, event: Value) -> Value {
    json!({ "metadata": { "message_id": id, "message_type": "notification", "subscription_type": kind },
            "payload": { "subscription": { "type": kind }, "event": event } })
}

async fn validate() -> Response {
    axum::Json(
        json!({ "client_id": "client", "login": "streamer", "user_id": "111", "scopes": SCOPES, "expires_in": 14000 }),
    )
    .into_response()
}
