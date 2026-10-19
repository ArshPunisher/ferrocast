//! The phone's side, on `RELAY_REMOTE_URL`'s host only: pairing (`/r/v1/pair`), who am I
//! (`/r/v1/me`), forget this device (`/r/v1/forget`), short-lived tickets (`/r/v1/ticket`),
//! the API proxy (`/r/api/v1/*`) and the live socket (`/r/api/v1/live`).
//!
//! The 30-day device token travels only in `Authorization`, never in a URL: the live socket
//! takes a single-use 60-second ticket, and `<img>`/`<video>` previews a 5-minute media
//! ticket that only opens `GET /media/{id}`.
//!
//! Every proxied request: the device token (401), Pro (402), the rate (429 `remote_rate`,
//! 20 a second per device, live socket opens included), the role (403 `not_allowed_for_role`),
//! the size (413 over 2 MB), then the PC's tunnel (503 `pc_offline`, 503 `pc_busy`, 504
//! `pc_timeout` after 15 s). Only the request's method, API path, query (tickets taken out),
//! `Content-Type`, `Accept`, `Range` and body are sent on; only `Content-Type`,
//! `Content-Disposition`, `Content-Range`, `Accept-Ranges`, `Retry-After` and `X-Quota-Reset`
//! come back, with `no-store` and a sandboxing CSP. Bodies are never stored or logged.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, oneshot};

use super::scope::{self, Role};
use super::store::Device;
use super::tunnel::{Failed, Tunnel};
use super::{
    AppState, DEVICE_RATE, LIVE_TICKET_TTL_MS, MAX_BODY, MAX_LIVE_PER_DEVICE, MAX_RESPONSE, MEDIA_TICKET_TTL_MS,
    PAIR_TRIES, PAIR_WINDOW_MS, TOKEN_TTL_MS, TOUCH_EVERY_MS, available, json_body, limits, normal_code,
};
use crate::billing::gate::pro_required;
use crate::{ApiError, client_ip, now_ms};

/// Frames waiting for one phone; at a full queue the socket closes with [`CLOSE_LAGGED`].
pub(crate) const PHONE_QUEUE: usize = 512;
const PHONE_PING: Duration = Duration::from_secs(30);
/// How long a close may take to reach a phone before its socket is just dropped.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// Inbound frames on the phone socket: the dashboard sends nothing but pings.
const PHONE_MAX_INBOUND: usize = 4 * 1024;
/// Close code for "this device was removed or its pairing expired": the dashboard stops
/// reconnecting, forgets its token and asks to pair again (`store.ts`, `LiveOptions.onClose`).
pub(crate) const CLOSE_REVOKED: u16 = 4401;
/// Close code for "the PC's Pro is paused": the dashboard keeps its token and says so.
pub(crate) const CLOSE_PRO: u16 = 4402;
/// Close code for "this phone fell behind": the dashboard reconnects and refetches.
pub(crate) const CLOSE_LAGGED: u16 = 4408;
/// Close code for "opened in more tabs than [`MAX_LIVE_PER_DEVICE`]" (the oldest closes): the
/// dashboard stops reconnecting there and says it's open elsewhere.
pub(crate) const CLOSE_TOO_MANY: u16 = 4409;

/// One open phone live socket. Taken out of the map to close it: `kill` says why, and the
/// frame queue goes with it, so a full queue can never hold a close back.
#[derive(Debug)]
pub(crate) struct Phone {
    pub sock: u64,
    pub device_id: String,
    /// The device's subscription: the cloud bot's chat lines reach it while its PC is off.
    pub sub_id: String,
    pub role: Role,
    tx: mpsc::Sender<Arc<str>>,
    kill: oneshot::Sender<u16>,
}

impl Phone {
    /// Closes the socket with `code` (the close is sent before the queue drops).
    fn close(self, code: u16) {
        let Phone { kill, tx, .. } = self;
        let _ = kill.send(code);
        drop(tx);
    }
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "There's nothing here.")
}

/// Whether the request came in on the remote host (`RELAY_REMOTE_URL`).
pub(crate) fn on_remote_host(st: &AppState, headers: &HeaderMap) -> bool {
    let Some(want) = st.remote.cfg.host() else { return false };
    headers.get(header::HOST).and_then(|h| h.to_str().ok()).is_some_and(|h| h.eq_ignore_ascii_case(want))
}

/// A present `Origin` must be the remote page's own (no other site can drive a phone's token).
fn origin_ok(st: &AppState, headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        None => true,
        Some(o) => st.remote.cfg.origin.as_deref().is_some_and(|want| want.eq_ignore_ascii_case(o)),
    }
}

/// The host and origin checks every phone route makes first.
fn gate(st: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if !available(st) || !on_remote_host(st, headers) {
        return Err(not_found());
    }
    if !origin_ok(st, headers) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "forbidden", "Requests from other websites aren't allowed."));
    }
    Ok(())
}

fn unauthorized() -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "This device isn't paired any more.")
        .hint("Ask for a new link: Settings › Dashboard anywhere on the streaming PC.")
}

/// 401 `device_moved`: the PC's Pro now runs on another subscription than this phone's.
fn device_moved() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "device_moved",
        "The streaming PC's Pro moved to another subscription since this phone was paired.",
    )
    .hint("Pair this phone again: Settings › Dashboard anywhere on the streaming PC.")
}

fn query_value(raw: Option<&str>, key: &str) -> Option<String> {
    raw?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == key && !v.is_empty()).then(|| v.to_string())
    })
}

/// The query without `ticket` or `session` (credentials never go to the PC).
fn strip_session(raw: Option<&str>) -> Option<String> {
    let kept: Vec<&str> = raw?
        .split('&')
        .filter(|p| !p.is_empty() && !matches!(p.split('=').next(), Some("session" | "ticket")))
        .collect();
    (!kept.is_empty()).then(|| kept.join("&"))
}

#[cfg(test)]
pub(crate) fn tests_strip_session(q: &str) -> Option<String> {
    strip_session(Some(q))
}

/// `fcr_<device id>.<secret>`.
fn parse_token(t: &str) -> Option<(&str, &str)> {
    let (id, secret) = t.trim().strip_prefix("fcr_")?.split_once('.')?;
    (id.starts_with("dv_") && id.len() <= 40 && !secret.is_empty() && secret.len() <= 100).then_some((id, secret))
}

/// The device behind a token: known, hash equal (constant time), then [`device_ok`].
fn device_for(st: &AppState, token: Option<&str>) -> Result<(Device, Role), ApiError> {
    let (id, secret) = token.and_then(parse_token).ok_or_else(unauthorized)?;
    let d = st.db.remote_device(id).map_err(ApiError::db)?.ok_or_else(unauthorized)?;
    let want = st.remote.cfg.hash(secret).ok_or_else(super::unavailable)?;
    if !bool::from(d.token_hash.as_bytes().ct_eq(want.as_bytes())) {
        return Err(unauthorized());
    }
    device_ok(st, d)
}

/// The device a ticket was made for, if it's still paired (then [`device_ok`]).
fn device_by_id(st: &AppState, id: &str) -> Result<(Device, Role), ApiError> {
    let d = st.db.remote_device(id).map_err(ApiError::db)?.ok_or_else(unauthorized)?;
    device_ok(st, d)
}

/// Not expired (401), its PC still Pro (402), and the PC still on the subscription it was
/// paired under (401 `device_moved`: Pro was restored onto another subscription since, and
/// that one's history, cloud bot and audit aren't this phone's to reach). Slides the expiry
/// forward (once a minute at most).
fn device_ok(st: &AppState, d: Device) -> Result<(Device, Role), ApiError> {
    let now = now_ms();
    if d.expires_at <= now {
        let _ = st.db.remote_delete_device(&d.id, None);
        return Err(unauthorized());
    }
    let role = Role::parse(&d.role).ok_or_else(unauthorized)?;
    match st.db.pro_sub_for_install(&d.install_id).map_err(ApiError::db)? {
        Some(s) if crate::billing::access::is_pro(&s.access(), now) => {
            if s.id != d.sub_id {
                return Err(device_moved());
            }
        }
        _ => {
            return Err(pro_required().hint(
                "The streamer's Ferrocast Pro is paused. This phone stays paired and works again once it's back.",
            ));
        }
    }
    if now - d.last_used_at >= TOUCH_EVERY_MS {
        let _ = st.db.remote_touch(&d.id, now, now + TOKEN_TTL_MS);
    }
    Ok((d, role))
}

/// 20 requests a second per device (live socket opens and tickets count too).
fn rate(st: &AppState, d: &Device) -> Result<(), ApiError> {
    st.remote.take("device", &d.id, DEVICE_RATE, 1_000).map_err(|wait| {
        api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "remote_rate",
            "That's a lot of requests at once.",
            "Wait a second and try again.",
        )
        .header(header::RETRY_AFTER, wait.to_string())
    })
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim)
}

/// Headers on every answer to a phone: never cached, never sniffed or framed.
fn secure(res: &mut Response) {
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'; sandbox"),
    );
}

fn done(r: Result<Response, ApiError>) -> Response {
    let mut res = r.unwrap_or_else(IntoResponse::into_response);
    secure(&mut res);
    res
}

// ── Pairing ──────────────────────────────────────────────────────────────────────────

/// `POST /r/v1/pair {code}` → `{token, role, name, device_id, expires_at}`. Any miss (unknown,
/// used, expired, PC no longer Pro) is the same 404; 5 tries per address per 10 minutes.
pub(crate) async fn pair(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    done(pair_inner(&st, peer, &headers, &body))
}

fn pair_inner(
    st: &AppState,
    peer: std::net::SocketAddr,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, ApiError> {
    gate(st, headers)?;
    let ip = limits::ip_key(client_ip(peer, headers));
    if let Err(wait) = st.remote.take("pair", &ip, PAIR_TRIES, PAIR_WINDOW_MS) {
        let mins = wait.div_ceil(60);
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "Too many tries from here.")
            .hint(format!("Wait {mins} minute{}, then scan a fresh code.", if mins == 1 { "" } else { "s" }))
            .header(header::RETRY_AFTER, wait.to_string()));
    }
    let v = json_body(headers, body)?;
    let miss = || {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", "That link has expired or was already used.")
            .hint("Make a new one on the streaming PC: Settings › Dashboard anywhere.")
    };
    let code = normal_code(v["code"].as_str().unwrap_or(""));
    if code.len() != super::CODE_LEN {
        return Err(miss());
    }
    let hash = st.remote.cfg.hash(&code).ok_or_else(super::unavailable)?;
    let now = now_ms();
    let taken = st.db.remote_take_code(&hash, now).map_err(ApiError::db)?.ok_or_else(miss)?;
    let pro_ok = st
        .db
        .pro_sub_for_install(&taken.install_id)
        .map_err(ApiError::db)?
        .is_some_and(|s| crate::billing::access::is_pro(&s.access(), now));
    let role = Role::parse(&taken.role).ok_or_else(miss)?;
    if !pro_ok {
        return Err(miss());
    }
    let id = format!("dv_{}", hex16());
    let secret = crate::random(32);
    let d = Device {
        id: id.clone(),
        install_id: taken.install_id.clone(),
        sub_id: taken.sub_id.clone(),
        role: role.as_str().to_string(),
        name: taken.name.clone(),
        token_hash: st.remote.cfg.hash(&secret).ok_or_else(super::unavailable)?,
        created_at: now,
        last_used_at: now,
        expires_at: now + TOKEN_TTL_MS,
    };
    st.db.remote_add_device(&d).map_err(ApiError::db)?;
    let _ = st.db.remote_audit(&d.sub_id, &d.install_id, &d.id, &d.name, "PAIR device", 200, now);
    super::tell_counts(st, &d.install_id);
    Ok(axum::Json(json!({
        "token": format!("fcr_{id}.{secret}"),
        "device_id": id,
        "role": role.as_str(),
        "name": d.name,
        "expires_at": d.expires_at,
    }))
    .into_response())
}

fn hex16() -> String {
    let mut b = [0u8; 8];
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `GET /r/v1/me` → `{device_id, name, role, expires_at, pc_online, cloud, cloud_paused,
/// cloud_moderation, history}`.
pub(crate) async fn me(State(st): State<AppState>, headers: HeaderMap) -> Response {
    done((|| -> Result<Response, ApiError> {
        gate(&st, &headers)?;
        let (d, role) = device_for(&st, bearer(&headers))?;
        let cloud = crate::bot::cloud_state(&st, &d.sub_id);
        Ok(axum::Json(json!({
            "device_id": d.id, "name": d.name, "role": role.as_str(),
            "expires_at": d.expires_at,
            "pc_online": st.remote.pc_online(&d.install_id),
            // While the PC is off: the cloud bot answers chat and (with the opt-in) bans (M10).
            "cloud": cloud.on, "cloud_paused": cloud.paused,
            "cloud_moderation": cloud.moderation.json(),
            // Stream history (Pro F7) for an owner's phone, online or off.
            "history": history_shown(&st, role),
        }))
        .into_response())
    })())
}

/// `POST /r/v1/forget` → 204: this phone removes itself.
pub(crate) async fn forget(State(st): State<AppState>, headers: HeaderMap) -> Response {
    done((|| -> Result<Response, ApiError> {
        gate(&st, &headers)?;
        let token = bearer(&headers);
        let (id, secret) = token.and_then(parse_token).ok_or_else(unauthorized)?;
        let d = st.db.remote_device(id).map_err(ApiError::db)?.ok_or_else(unauthorized)?;
        let want = st.remote.cfg.hash(secret).ok_or_else(super::unavailable)?;
        if !bool::from(d.token_hash.as_bytes().ct_eq(want.as_bytes())) {
            return Err(unauthorized());
        }
        st.db.remote_delete_device(&d.id, None).map_err(ApiError::db)?;
        let _ = st.db.remote_audit(&d.sub_id, &d.install_id, &d.id, &d.name, "FORGET device", 204, now_ms());
        close_device(&st, &d.install_id, &d.id);
        super::tell_counts(&st, &d.install_id);
        Ok(StatusCode::NO_CONTENT.into_response())
    })())
}

/// `POST /r/v1/ticket {for: "live"|"media"}` → `{ticket, expires_in}` (Bearer token). A live
/// ticket opens one live socket once, within 60 s; a media ticket opens `GET /media/{id}`
/// previews for 5 minutes. Neither is the device token, so a URL in a log or history is
/// worth nothing soon after.
pub(crate) async fn ticket(State(st): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    done((|| -> Result<Response, ApiError> {
        gate(&st, &headers)?;
        let (d, _) = device_for(&st, bearer(&headers))?;
        rate(&st, &d)?;
        let v = json_body(&headers, &body)?;
        let (ticket, ttl) = match v["for"].as_str() {
            Some("live") => (st.remote.new_live_ticket(&d.id), LIVE_TICKET_TTL_MS),
            Some("media") => (st.remote.new_media_ticket(&d.id), MEDIA_TICKET_TTL_MS),
            _ => {
                return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid", "for: \"live\" or \"media\"."));
            }
        };
        let ticket = ticket.ok_or_else(|| {
            api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                "Ferrocast's servers are busy.",
                "Try again in a moment.",
            )
            .header(header::RETRY_AFTER, "2")
        })?;
        Ok(axum::Json(json!({ "ticket": ticket, "expires_in": ttl / 1000 })).into_response())
    })())
}

/// `/media/{id}`: the only route a media ticket opens.
fn is_media_path(path: &str) -> bool {
    scope::segments(path).is_some_and(|s| s.len() == 2 && s[0] == "media")
}

// ── The API proxy ────────────────────────────────────────────────────────────────────

fn api_error(status: StatusCode, code: &'static str, message: &str, hint: &str) -> ApiError {
    ApiError::new(status, code, message).hint(hint)
}

pub(crate) fn pc_offline() -> ApiError {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "pc_offline",
        "Your streaming PC is off.",
        "Turn it on and open Ferrocast. This works again on its own.",
    )
}

/// The body, up to `max` bytes; over that, the rest is drained so the 413 gets through.
async fn read_capped(body: Body, max: usize) -> Result<axum::body::Bytes, ()> {
    use futures_util::StreamExt;
    let mut stream = body.into_data_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ())?;
        if out.len() + chunk.len() > max {
            crate::blob::drain_stream(stream).await;
            return Err(());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out.into())
}

/// `ANY /r/api/v1/*`.
pub(crate) async fn api(State(st): State<AppState>, RawQuery(query): RawQuery, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let unread = std::sync::Mutex::new(Some(body));
    let res = api_inner(&st, query, parts, &unread).await;
    // A refusal before the body was read drains it first, so the phone gets the answer
    // instead of a connection reset partway through sending.
    let left = unread.lock().unwrap_or_else(|e| e.into_inner()).take();
    if res.is_err()
        && let Some(body) = left
    {
        crate::blob::drain(body).await;
    }
    done(res)
}

async fn api_inner(
    st: &AppState,
    query: Option<String>,
    req: axum::http::request::Parts,
    unread: &std::sync::Mutex<Option<Body>>,
) -> Result<Response, ApiError> {
    let headers = req.headers.clone();
    gate(st, &headers)?;
    let method = req.method.clone();
    let path = req.uri.path().strip_prefix("/r/api/v1").unwrap_or("").to_string();
    let (device, role) = match bearer(&headers) {
        Some(token) => device_for(st, Some(token))?,
        None => {
            // `<img>` and `<video>` can't send headers: a media preview may carry a media
            // ticket (never the device token) in its query.
            let t = query_value(query.as_deref(), "ticket")
                .filter(|_| method == Method::GET && is_media_path(&path))
                .ok_or_else(unauthorized)?;
            let id = st.remote.check_media_ticket(&t).ok_or_else(unauthorized)?;
            device_by_id(st, &id)?
        }
    };
    rate(st, &device)?;
    let audit_from = |status: u16, known: bool, source: &str| {
        let action = scope::action(method.as_str(), &path, known);
        // Refusals: one line per device and route a minute, however often it's tried.
        if status == 403 && !st.remote.allow("audit", &format!("{}|{action}", device.id), 1, 60_000) {
            return;
        }
        let _ = st.db.remote_audit_from(
            &device.sub_id,
            &device.install_id,
            &device.id,
            &device.name,
            &action,
            status,
            now_ms(),
            source,
        );
    };
    let audit = |status: u16, known: bool| audit_from(status, known, "pc");
    if !scope::allowed(role, method.as_str(), &path) {
        // A refused read changes nothing: only refused changes are worth a line.
        if method != Method::GET {
            audit(403, false);
        }
        let what =
            if role == Role::Mod { "Mods can't do that from here." } else { "That only works on the streaming PC." };
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "not_allowed_for_role",
            what,
            "Accounts, secrets, Pro, export and import stay on the PC itself.",
        ));
    }
    let too_big = || {
        ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "Requests from a phone can be up to 2 MB.")
            .hint("Send something smaller, or do this on the PC itself.")
    };
    let declared =
        headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_BODY as u64) {
        return Err(too_big());
    }
    let body = unread.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let body = read_capped(body, MAX_BODY).await.map_err(|_| too_big())?;
    let Some(tunnel) = st.remote.tunnel(&device.install_id) else {
        // The PC is off: the relay answers chat, moderation, history and the cloud bot's
        // switch itself (M10); everything else is 503 `pc_offline`.
        let (res, answered) =
            super::offline::answer(st, &device, role, &method, &path, query.as_deref(), &headers, &body).await;
        if answered && method != Method::GET {
            let status = match &res {
                Ok(r) => r.status().as_u16(),
                Err(e) => e.status().as_u16(),
            };
            audit_from(status, true, "cloud");
        }
        return res;
    };
    let pass = |name: HeaderName| headers.get(&name).and_then(|v| v.to_str().ok()).map(|v| json!([name.as_str(), v]));
    // `Range` lets a video preview come in parts, each under the 8 MB answer limit.
    let fwd: Vec<Value> = [header::CONTENT_TYPE, header::ACCEPT, header::RANGE].into_iter().filter_map(pass).collect();
    let full_path = match strip_session(query.as_deref()) {
        Some(q) => format!("/api/v1{path}?{q}"),
        None => format!("/api/v1{path}"),
    };
    let id = st.remote.next_id();
    let frame = json!({
        "t": "req", "id": id, "method": method.as_str(), "path": full_path, "headers": fwd,
        "body_b64": base64::engine::general_purpose::STANDARD.encode(&body),
        "device_id": device.id, "role": role.as_str(),
    });
    drop(body);
    let res = match tunnel.request(id, frame, st.remote.pc_timeout()).await {
        Ok(r) => r,
        Err(Failed::Gone) => return Err(pc_offline()),
        Err(Failed::Busy) => return Err(pc_busy()),
        Err(Failed::Timeout) => {
            if method != Method::GET {
                audit(504, true);
            }
            return Err(api_error(
                StatusCode::GATEWAY_TIMEOUT,
                "pc_timeout",
                "Your streaming PC didn't answer in time.",
                "Check it's online, then try again.",
            ));
        }
    };
    if res.body.len() > MAX_RESPONSE {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "too_large",
            "That's too big to open through Ferrocast's servers (8 MB at most).",
        )
        .hint("Open it on the PC itself."));
    }
    if method != Method::GET {
        audit(res.status, !matches!(res.status, 404 | 405));
    }
    let mut out = Response::new(Body::from(res.body));
    *out.status_mut() = StatusCode::from_u16(res.status).unwrap_or(StatusCode::BAD_GATEWAY);
    for (k, v) in res.headers {
        let k = k.to_ascii_lowercase();
        if !PASSED_BACK.contains(&k.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
            out.headers_mut().insert(name, value);
        }
    }
    Ok(out)
}

/// Headers kept from the PC's answer.
const PASSED_BACK: &[&str] =
    &["content-type", "content-disposition", "content-range", "accept-ranges", "retry-after", "x-quota-reset"];

/// 503 `pc_busy`: too many requests are waiting on the PC (the same words the PC uses).
pub(crate) fn pc_busy() -> ApiError {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "pc_busy",
        "Your PC is busy, try again in a moment.",
        "It's answering a lot of requests at once.",
    )
    .header(header::RETRY_AFTER, "2")
}

// ── Live sockets ─────────────────────────────────────────────────────────────────────

/// The relay's own frame on a phone's live socket: whether the PC is reachable, and while
/// it's off whether the cloud bot stands in (chat, bans and history still work, M10).
pub(crate) fn remote_frame(st: &AppState, online: bool, role: Role, cloud: crate::bot::CloudState) -> Arc<str> {
    Arc::from(
        json!({ "v": 1, "type": "remote", "data": {
            "pc_online": online, "role": role.as_str(), "cloud": cloud.on, "cloud_paused": cloud.paused,
            "cloud_moderation": cloud.moderation.json(), "history": history_shown(st, role),
        } })
        .to_string(),
    )
}

/// Whether a phone of `role` gets stream history (Pro F7): owners, while the relay has it.
fn history_shown(st: &AppState, role: Role) -> bool {
    role == Role::Owner && crate::history::available(st)
}

/// Asks a phone to fetch chat again (the PC went off and the cloud's chat takes over).
pub(crate) fn resync_frame() -> Arc<str> {
    Arc::from(json!({ "v": 1, "type": "resync", "data": { "missed": 0 } }).to_string())
}

/// `GET /r/api/v1/live?ticket=<live ticket>&since=<seq>` (WebSocket). The ticket comes from
/// `POST /r/v1/ticket {for:"live"}` and works once.
pub(crate) async fn live(
    State(st): State<AppState>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let checked = (|| -> Result<(Device, Role), ApiError> {
        gate(&st, &headers)?;
        let t = query_value(raw.as_deref(), "ticket").ok_or_else(unauthorized)?;
        let id = st.remote.take_live_ticket(&t).ok_or_else(unauthorized)?;
        let (d, role) = device_by_id(&st, &id)?;
        rate(&st, &d)?;
        Ok((d, role))
    })();
    let (device, role) = match checked {
        Ok(x) => x,
        Err(e) => return done(Err(e)),
    };
    let since = query_value(raw.as_deref(), "since").and_then(|s| s.parse::<u64>().ok());
    ws.max_message_size(PHONE_MAX_INBOUND).on_upgrade(move |socket| phone_loop(st, device, role, since, socket))
}

/// Adds a phone socket; past [`MAX_LIVE_PER_DEVICE`] for its device the oldest ones close
/// with [`CLOSE_TOO_MANY`].
fn register(st: &AppState, install: &str, phone: Phone) {
    let mut m = st.remote.phones.lock().unwrap_or_else(|p| p.into_inner());
    // `phones` first, then `phone_subs` (the documented order), so `sub_installs` never sees
    // the index entry without the phone and drops it.
    st.remote
        .phone_subs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(phone.sub_id.clone())
        .or_default()
        .insert(install.to_string());
    let list = m.entry(install.to_string()).or_default();
    while list.iter().filter(|p| p.device_id == phone.device_id).count() >= MAX_LIVE_PER_DEVICE {
        let Some(i) = list
            .iter()
            .enumerate()
            .filter(|(_, p)| p.device_id == phone.device_id)
            .min_by_key(|(_, p)| p.sock)
            .map(|(i, _)| i)
        else {
            break;
        };
        list.remove(i).close(CLOSE_TOO_MANY);
    }
    list.push(phone);
}

async fn phone_loop(st: AppState, device: Device, role: Role, since: Option<u64>, socket: WebSocket) {
    let (tx_q, mut rx_q) = mpsc::channel::<Arc<str>>(PHONE_QUEUE);
    let (kill, mut killed) = oneshot::channel::<u16>();
    let sock = st.remote.next_id();
    let install = device.install_id.clone();
    let cloud = crate::bot::cloud_state(&st, &device.sub_id);
    register(
        &st,
        &install,
        Phone { sock, device_id: device.id.clone(), sub_id: device.sub_id.clone(), role, tx: tx_q, kill },
    );
    let (mut tx, mut rx) = socket.split();
    let tunnel = st.remote.tunnel(&install);
    let first = remote_frame(&st, tunnel.is_some(), role, cloud);
    let close = |code: u16| Message::Close(Some(CloseFrame { code, reason: "".into() }));
    let mut opened = tx.send(Message::Text(Utf8Bytes::from(first.as_ref()))).await.is_ok();
    if opened && tunnel.is_none() && cloud.on {
        // The PC is off and the cloud's chat stands in: the phone fetches it (`GET /chat`)
        // after this, so a line that came while the socket opened isn't missed.
        opened = tx.send(Message::Text(Utf8Bytes::from(resync_frame().as_ref()))).await.is_ok();
    }
    if opened {
        if let Some(t) = &tunnel {
            t.send(&open_frame(sock, &device.id, role, since));
        }
        let mut ping = tokio::time::interval(PHONE_PING);
        ping.tick().await;
        loop {
            tokio::select! {
                // A close always wins over frames still queued.
                biased;
                code = &mut killed => {
                    // A phone that stopped reading can't hold this task: give the close 2 s.
                    let _ = tokio::time::timeout(CLOSE_WAIT, tx.send(close(code.unwrap_or(1001)))).await;
                    break;
                }
                m = rx_q.recv() => match m {
                    Some(f) => {
                        // Sending can wait on a slow phone; a close still gets through.
                        tokio::select! {
                            biased;
                            code = &mut killed => {
                                let _ = tokio::time::timeout(CLOSE_WAIT, tx.send(close(code.unwrap_or(1001)))).await;
                                break;
                            }
                            sent = tx.send(Message::Text(Utf8Bytes::from(f.as_ref()))) => if sent.is_err() { break },
                        }
                    }
                    None => break,
                },
                _ = ping.tick() => {
                    // Still paired and still Pro? (A removed device's socket is closed at once;
                    // this catches expiry and a lapse.)
                    let paired = matches!(st.db.remote_device(&device.id), Ok(Some(d)) if d.expires_at > now_ms());
                    let sub = st.db.pro_sub_for_install(&install);
                    let pro = matches!(&sub, Ok(Some(s)) if crate::billing::access::is_pro(&s.access(), now_ms()));
                    // Pro restored onto another subscription: this phone pairs again.
                    let paired = paired && !matches!(&sub, Ok(Some(s)) if pro && s.id != device.sub_id);
                    if !paired || !pro {
                        let _ = tx.send(close(if paired { CLOSE_PRO } else { CLOSE_REVOKED })).await;
                        break;
                    }
                    if tx.send(Message::Ping(Default::default())).await.is_err() { break }
                }
                incoming = rx.next() => match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                },
            }
        }
    }
    {
        let mut m = st.remote.phones.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(list) = m.get_mut(&install) {
            list.retain(|p| p.sock != sock);
            if list.is_empty() {
                m.remove(&install);
            }
        }
    }
    if let Some(t) = st.remote.tunnel(&install) {
        t.send(&json!({ "t": "live_close", "sock": sock }));
    }
}

fn open_frame(sock: u64, device_id: &str, role: Role, since: Option<u64>) -> Value {
    json!({ "t": "live_open", "sock": sock, "device_id": device_id, "role": role.as_str(), "since": since })
}

/// A new tunnel: tell its phones the PC is back, and ask the PC for their live frames.
pub(crate) fn tunnel_up(st: &AppState, install: &str, tunnel: &Tunnel) {
    let cloud = phones_sub(st, install).map(|s| crate::bot::cloud_state(st, &s)).unwrap_or_default();
    let m = st.remote.phones.lock().unwrap_or_else(|p| p.into_inner());
    for p in m.get(install).into_iter().flatten() {
        let _ = p.tx.try_send(remote_frame(st, true, p.role, cloud));
        // The phone's seq is unknown here: the PC sends `hello` and `resync`, so it refetches.
        tunnel.send(&open_frame(p.sock, &p.device_id, p.role, Some(0)));
    }
}

/// The tunnel closed: the phones show "PC offline" (their sockets stay open), and when the
/// cloud bot stands in, they switch to its chat (a `resync` makes them fetch it).
pub(crate) fn tunnel_down(st: &AppState, install: &str) {
    let cloud = phones_sub(st, install).map(|s| crate::bot::cloud_state(st, &s)).unwrap_or_default();
    let m = st.remote.phones.lock().unwrap_or_else(|p| p.into_inner());
    for p in m.get(install).into_iter().flatten() {
        let _ = p.tx.try_send(remote_frame(st, false, p.role, cloud));
        if cloud.on {
            let _ = p.tx.try_send(resync_frame());
        }
    }
}

/// Whether any phone of `sub` has its live socket open.
pub(crate) fn has_phones(st: &AppState, sub: &str) -> bool {
    !sub_installs(st, sub).is_empty()
}

/// The installs with an open phone socket of `sub` (from the index; stale entries dropped).
fn sub_installs(st: &AppState, sub: &str) -> Vec<String> {
    let listed: Vec<String> = {
        let m = st.remote.phone_subs.lock().unwrap_or_else(|p| p.into_inner());
        match m.get(sub) {
            Some(set) => set.iter().cloned().collect(),
            None => return Vec::new(),
        }
    };
    let (live, gone): (Vec<String>, Vec<String>) = {
        let m = st.remote.phones.lock().unwrap_or_else(|p| p.into_inner());
        listed.into_iter().partition(|i| m.get(i).is_some_and(|l| l.iter().any(|p| p.sub_id == sub)))
    };
    if !gone.is_empty() {
        let phones = st.remote.phones.lock().unwrap_or_else(|p| p.into_inner());
        let mut m = st.remote.phone_subs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(set) = m.get_mut(sub) {
            // Checked again under both locks: a phone that opened meanwhile keeps its entry.
            for i in &gone {
                if !phones.get(i).is_some_and(|l| l.iter().any(|p| p.sub_id == sub)) {
                    set.remove(i);
                }
            }
            if set.is_empty() {
                m.remove(sub);
            }
        }
    }
    live
}

/// The subscription of an install's open phones (they all share their PC's).
fn phones_sub(st: &AppState, install: &str) -> Option<String> {
    let m = st.remote.phones.lock().unwrap_or_else(|p| p.into_inner());
    m.get(install).and_then(|l| l.first()).map(|p| p.sub_id.clone())
}

/// Frames the relay itself sends while a PC is off (M10): to every open phone of `sub`
/// whose PC has no tunnel, `frames(role)` for each phone's role (the caller leaves out what
/// a role may not get). A phone that can't keep up closes with [`CLOSE_LAGGED`].
pub(crate) fn fan_out_offline(st: &AppState, sub: &str, frames: impl Fn(Role) -> Vec<Arc<str>>) {
    let (for_owner, for_mod) = (frames(Role::Owner), frames(Role::Mod));
    for install in sub_installs(st, sub) {
        if st.remote.tunnel(&install).is_some() {
            continue; // the PC sends its own frames
        }
        let mut m = st.remote.phones.lock().unwrap_or_else(|p| p.into_inner());
        let Some(list) = m.get_mut(&install) else { continue };
        let mut behind = Vec::new();
        for (i, p) in list.iter().enumerate() {
            if p.sub_id != sub {
                continue;
            }
            let frames = if p.role == Role::Owner { &for_owner } else { &for_mod };
            for f in frames {
                if let Err(mpsc::error::TrySendError::Full(_)) = p.tx.try_send(f.clone()) {
                    behind.push(i);
                    break;
                }
            }
        }
        for i in behind.into_iter().rev() {
            list.remove(i).close(CLOSE_LAGGED);
        }
    }
}

/// A live frame from the PC to its phones (`to`: one socket; `owners_only`: owner devices).
/// Each frame is checked against the phone's role here too. A phone whose queue is full is
/// taken out and closed with [`CLOSE_LAGGED`] (it reconnects and refetches).
pub(crate) fn fan_out(st: &AppState, install: &str, to: Option<u64>, owners_only: bool, data: &str) {
    let text: Arc<str> = Arc::from(data);
    let mut m = st.remote.phones.lock().unwrap_or_else(|p| p.into_inner());
    let Some(list) = m.get_mut(install) else { return };
    let mut behind = Vec::new();
    for (i, p) in list.iter().enumerate() {
        if to.is_some_and(|s| s != p.sock) || (owners_only && p.role != Role::Owner) {
            continue;
        }
        if !scope::live_frame_allowed(p.role, data) {
            continue;
        }
        if let Err(mpsc::error::TrySendError::Full(_)) = p.tx.try_send(text.clone()) {
            behind.push(i);
        }
    }
    for i in behind.into_iter().rev() {
        list.remove(i).close(CLOSE_LAGGED);
    }
}

/// A device was removed: its sockets close with [`CLOSE_REVOKED`] at once.
pub(crate) fn close_device(st: &AppState, install: &str, device_id: &str) {
    let mut m = st.remote.phones.lock().unwrap_or_else(|p| p.into_inner());
    let Some(list) = m.get_mut(install) else { return };
    let mut i = 0;
    while i < list.len() {
        if list[i].device_id == device_id {
            list.remove(i).close(CLOSE_REVOKED);
        } else {
            i += 1;
        }
    }
}

/// The installs the index lists for `sub` (before any clean-up).
#[cfg(test)]
pub(crate) fn tests_indexed_installs(st: &AppState, sub: &str) -> usize {
    st.remote.phone_subs.lock().unwrap_or_else(|p| p.into_inner()).get(sub).map_or(0, |s| s.len())
}

#[cfg(test)]
pub(crate) fn tests_has_phones(st: &AppState, sub: &str) -> bool {
    has_phones(st, sub)
}

#[cfg(test)]
pub(crate) fn tests_phone_count(st: &AppState, install: &str) -> usize {
    let m = st.remote.phones.lock().unwrap_or_else(|p| p.into_inner());
    m.get(install).map_or(0, Vec::len)
}
