//! A fake StreamElements API (`/kappa/v2`) for importer tests. It answers from the
//! fixtures in `fixtures/streamelements/` (shapes from StreamElements' docs and a real,
//! sanitized fresh account), checks the bearer token, records every request, and can be
//! told to fail a path with a status or to send broken JSON.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// A made-up JWT the fake accepts. Not a real token.
pub const TOKEN: &str =
    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJjaGFubmVsIjoiZml4dHVyZSJ9.ZmFrZS1zaWduYXR1cmUtZm9yLXRlc3Rz";
pub const CHANNEL: &str = "5b2e2007760aeb7729487dab";

pub const CHANNELS_ME: &str = include_str!("../fixtures/streamelements/channels_me.json");
pub const COMMANDS: &str = include_str!("../fixtures/streamelements/commands.json");
pub const TIMERS: &str = include_str!("../fixtures/streamelements/timers.json");
pub const LOYALTY: &str = include_str!("../fixtures/streamelements/loyalty.json");
pub const TIPS: &str = include_str!("../fixtures/streamelements/tips.json");
pub const EMPTY_ACCOUNT: &str = include_str!("../fixtures/streamelements/empty_account.json");

#[derive(Debug, Default)]
pub struct FakeState {
    /// An account with nothing in it (like a fresh one).
    pub empty: bool,
    /// How many points balances to serve (generated: `viewer0`… with 10, 20, 30…).
    pub points: usize,
    /// Paths starting with a key answer that status instead.
    pub fail: HashMap<String, u16>,
    /// Paths starting with this answer `{not json`.
    pub garbage: Option<String>,
    /// Every request: method and path with query.
    pub requests: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Fake {
    pub base: String,
    pub state: Arc<Mutex<FakeState>>,
}

impl Fake {
    pub fn set(&self, f: impl FnOnce(&mut FakeState)) {
        f(&mut self.state.lock().unwrap());
    }

    pub fn requests(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().requests.clone()
    }
}

fn json_res(status: u16, v: Value) -> Response {
    (StatusCode::from_u16(status).unwrap(), axum::Json(v)).into_response()
}

fn query(uri: &Uri, key: &str) -> Option<usize> {
    uri.query()?.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key)?.1.parse().ok())
}

async fn handle(State(st): State<Arc<Mutex<FakeState>>>, method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches("/kappa/v2").to_string();
    let full = format!("{path}{}", uri.query().map(|q| format!("?{q}")).unwrap_or_default());
    let mut s = st.lock().unwrap();
    s.requests.push((method.to_string(), full));
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    if auth.is_empty() {
        return json_res(
            401,
            json!({ "statusCode": 401, "error": "Unauthorized", "message": "No authorization token was found" }),
        );
    }
    if auth != format!("Bearer {TOKEN}") {
        return json_res(
            401,
            json!({ "statusCode": 401, "error": "Unauthorized", "message": "Not allowed to perform this operation" }),
        );
    }
    if method != Method::GET {
        return json_res(405, json!({ "statusCode": 405, "error": "Method Not Allowed" }));
    }
    if let Some((_, code)) = s.fail.iter().find(|(p, _)| path.starts_with(p.as_str())) {
        let mut res = json_res(*code, json!({ "statusCode": code, "error": "Fake failure" }));
        if *code == 429 {
            res.headers_mut().insert("retry-after", "7".parse().unwrap());
        }
        return res;
    }
    if s.garbage.as_ref().is_some_and(|g| path.starts_with(g.as_str())) {
        return (StatusCode::OK, "{not json").into_response();
    }
    let empty: Value = serde_json::from_str(EMPTY_ACCOUNT).unwrap();
    let parse = |t: &str| serde_json::from_str::<Value>(t).unwrap();
    let c = CHANNEL;
    let v = if path == "/channels/me" {
        parse(CHANNELS_ME)
    } else if path == format!("/bot/commands/{c}") {
        if s.empty { empty["commands"].clone() } else { parse(COMMANDS) }
    } else if path == format!("/bot/timers/{c}") {
        if s.empty { empty["timers"].clone() } else { parse(TIMERS) }
    } else if path == format!("/loyalty/{c}") {
        if s.empty {
            return json_res(404, empty["loyalty"].clone());
        }
        parse(LOYALTY)
    } else if path == format!("/points/{c}/top") {
        let limit = query(&uri, "limit").unwrap_or(25);
        if limit > 1000 {
            return json_res(400, json!({ "statusCode": 400, "message": "Too big: expected number to be <=1000" }));
        }
        let offset = query(&uri, "offset").unwrap_or(0);
        let total = if s.empty { 0 } else { s.points };
        let users: Vec<Value> = (offset..total.min(offset + limit))
            .map(|i| json!({ "username": format!("viewer{i}"), "points": (i + 1) * 10 }))
            .collect();
        json!({ "_total": total, "users": users })
    } else if path == format!("/tips/{c}") {
        if query(&uri, "limit").unwrap_or(25) > 100 {
            return json_res(400, json!({ "statusCode": 400, "message": "Too big: expected number to be <=100" }));
        }
        if s.empty || query(&uri, "offset").unwrap_or(0) > 0 { empty["tips"].clone() } else { parse(TIPS) }
    } else if path == format!("/overlays/{c}") {
        if s.empty {
            empty["overlays"].clone()
        } else {
            json!({ "docs": [{ "_id": "o1", "name": "Main" }, { "_id": "o2", "name": "BRB" }] })
        }
    } else if path == format!("/store/{c}/items") {
        if s.empty { json!([]) } else { json!([{ "_id": "i1", "name": "Song request" }]) }
    } else if path == format!("/bot/modules/{c}") {
        if s.empty { json!({}) } else { json!({ "eightball": { "enabled": true }, "twitter": { "enabled": false } }) }
    } else {
        return json_res(404, json!({ "statusCode": 404, "error": "Not Found" }));
    };
    json_res(200, v)
}

/// Starts the fake on a free port. `base` is the API root to give the importer.
pub async fn fake() -> Fake {
    let state = Arc::new(Mutex::new(FakeState { points: 3, ..Default::default() }));
    let app = Router::new().fallback(handle).with_state(state.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await });
    Fake { base: format!("http://{addr}/kappa/v2"), state }
}
