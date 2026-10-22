//! Sign-in for the website (`RELAY_WEB_URL`, ferrocast-web plan §9): the same steps as the
//! pages in `pages.rs` (the login cookie, its form token, the emailed link and code, Google,
//! the app's sign-in flow), as JSON the website's `/login` calls across sites with the
//! cookie. The form token comes in `X-CSRF-Token`.
//!
//! `GET  /v1/login?flow=&next=` → `{ csrf, google, app, signed_in, next }` (sets the login cookie)
//! `POST /v1/login/email  { email, flow, next }` → `{ magic_id }`
//! `POST /v1/login/code   { magic_id, code }`    → `{ done: "web" | "app", next }`
//! `POST /v1/login/link   { t }`                 → `{ done: "web" | "app", next }`
//! `POST /v1/login/google { flow, next }`        → `{ url }`
//!
//! `done: "web"`: the session cookie is set and `next` is a dashboard path on the website;
//! `"app"`: `next` is the app's loopback address to open. Errors are `{error:{code,message,hint}}`.

use axum::extract::{ConnectInfo, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::pages::safe_next;
use super::pages::{
    Fail, Finished, check_code, finish_core, google_url, ip_of, live_flow, login_guard, send_magic, session_cookies,
    take_link,
};
use super::*;

pub(crate) fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/v1/login", get(start))
        .route("/v1/login/email", post(email))
        .route("/v1/login/code", post(code))
        .route("/v1/login/link", post(link))
        .route("/v1/login/google", post(google))
}

fn fail(f: Fail) -> Response {
    let mut r = ApiError::new(f.status, f.code, f.text).into_response();
    if let Some(w) = f.retry_after
        && let Ok(v) = HeaderValue::from_str(&w.to_string())
    {
        r.headers_mut().insert(header::RETRY_AFTER, v);
    }
    no_store(r)
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn not_here() -> Response {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "There's no sign-in here.").into_response()
}

/// The guard every POST makes, with the token from `X-CSRF-Token`; the login cookie.
fn guard(st: &AppState, headers: &HeaderMap) -> Result<String, Box<Response>> {
    let given = headers.get("x-csrf-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    match login_guard(st, headers, given) {
        Ok(Some(c)) => Ok(c),
        Ok(None) => Err(Box::new(not_here())),
        Err(f) => Err(Box::new(fail(f))),
    }
}

fn body_of(headers: &HeaderMap, body: &[u8]) -> Result<Value, Box<Response>> {
    json_body(headers, body).map_err(|e| Box::new(e.into_response()))
}

fn text<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v[k].as_str().filter(|s| !s.is_empty() && s.len() <= 2048)
}

fn done(d: Finished) -> Response {
    match d {
        Finished::App(u) => no_store(axum::Json(json!({ "done": "app", "next": u })).into_response()),
        Finished::Web { token, age, next } => {
            let mut r = axum::Json(json!({ "done": "web", "next": next })).into_response();
            session_cookies(&mut r, &token, age);
            no_store(r)
        }
    }
}

/// `GET /v1/login`: the form token for this browser (making its login cookie), whether
/// Google is on, whether this is the app's sign-in, and whether it's already signed in.
async fn start(State(st): State<AppState>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    if !available(&st) || !crate::remote::on_web_host(&st, &headers) {
        return not_here();
    }
    if !same_origin(&st, &headers) {
        return ApiError::new(StatusCode::FORBIDDEN, "forbidden", "Requests from other websites aren't allowed.")
            .into_response();
    }
    let q: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(raw.as_deref().unwrap_or("").as_bytes()).into_owned().collect();
    let next = safe_next(q.get("next").map(String::as_str));
    let flow = match live_flow(&st, q.get("flow").map(String::as_str)) {
        Ok(f) => f,
        Err(f) => return fail(f),
    };
    let (c, fresh) = match cookie(&headers, LOGIN_COOKIE).filter(|c| c.len() >= 20 && c.len() <= 64) {
        Some(c) => (c, false),
        None => (crate::random(24), true),
    };
    let signed_in = flow.is_none() && web_session(&st, &headers).is_ok();
    let mut r = axum::Json(json!({
        "csrf": form_token(&c),
        "google": st.accounts.cfg.google.on(),
        "app": flow.is_some(),
        "signed_in": signed_in,
        "next": next,
    }))
    .into_response();
    if fresh {
        r.headers_mut().append(header::SET_COOKIE, set_cookie(LOGIN_COOKIE, &c, LOGIN_COOKIE_S));
    }
    no_store(r)
}

async fn email(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let c = match guard(&st, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let v = match body_of(&headers, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let next = safe_next(text(&v, "next"));
    let flow = match live_flow(&st, text(&v, "flow")) {
        Ok(f) => f.map(|f| f.id),
        Err(f) => return fail(f),
    };
    match send_magic(&st, &ip_of(peer, &headers), &c, text(&v, "email").unwrap_or(""), flow.as_deref(), &next).await {
        Ok(id) => no_store(axum::Json(json!({ "magic_id": id })).into_response()),
        Err(f) => fail(f),
    }
}

async fn code(State(st): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let c = match guard(&st, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let v = match body_of(&headers, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let m = match check_code(&st, &c, text(&v, "magic_id").unwrap_or(""), text(&v, "code").unwrap_or("")) {
        Ok(m) => m,
        Err(f) => return fail(f),
    };
    match finish_core(&st, &headers, &m.email, None, m.flow_id.as_deref(), &m.next).await {
        Ok(d) => done(d),
        Err(f) => fail(f),
    }
}

async fn link(State(st): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let c = match guard(&st, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let v = match body_of(&headers, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let m = match take_link(&st, &c, text(&v, "t").unwrap_or("")) {
        Ok(m) => m,
        Err(f) => return fail(f),
    };
    match finish_core(&st, &headers, &m.email, None, m.flow_id.as_deref(), &m.next).await {
        Ok(d) => done(d),
        Err(f) => fail(f),
    }
}

async fn google(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let c = match guard(&st, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let v = match body_of(&headers, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let next = safe_next(text(&v, "next"));
    let flow = match live_flow(&st, text(&v, "flow")) {
        Ok(f) => f.map(|f| f.id),
        Err(f) => return fail(f),
    };
    match google_url(&st, &ip_of(peer, &headers), &c, flow.as_deref(), &next) {
        Ok(url) => no_store(axum::Json(json!({ "url": url })).into_response()),
        Err(f) => fail(f),
    }
}
