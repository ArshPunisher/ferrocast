//! IRL: streaming from a phone, no computer needed. The streamer's PC (or the dashboard)
//! makes an IRL session ahead (`irl: true`), and its answer carries a link for the phone with
//! a secret in its `#fragment`. The phone opens it (the page here, `/irl`), and sends its
//! camera and mic over WebRTC: H.264 as it is, Opus made AAC by ffmpeg, into the same path as
//! a computer's stream. A dropped phone signal (a tunnel, a lift) is held on Be right back,
//! and the phone picks up again by itself.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::io::AsyncReadExt;

use crate::{ApiError, Bond, Input, Session, random_id};
use ferrocast_golive::demux::TsDemuxer;

/// The phone's page, its manifest and its service worker (for the home screen and pushes).
const PAGE: &str = include_str!("../assets/irl.html");
const MANIFEST: &str = include_str!("../assets/irl.webmanifest");
const WORKER: &str = include_str!("../assets/irl-sw.js");
const ICON_180: &[u8] = include_bytes!("../assets/irl-180.png");
const ICON_192: &[u8] = include_bytes!("../assets/irl-192.png");
const ICON_512: &[u8] = include_bytes!("../assets/irl-512.png");

/// A new connection from the phone no sooner than this after the last (a page reloading in
/// a loop mustn't start ffmpeg after ffmpeg).
const OFFER_GAP: std::time::Duration = std::time::Duration::from_secs(2);

/// A phone session's secret and what's coming in from it now.
pub(crate) struct Irl {
    pub secret: String,
    now: Mutex<Option<Phone>>,
    last_offer: Mutex<Option<Instant>>,
    /// Where the phone takes pushes (a few: the page on more than one browser).
    subs: Mutex<Vec<crate::push::Subscription>>,
}

/// Push subscriptions a session keeps.
const MAX_SUBS: usize = 4;

/// The phone's connection: WebRTC in, ffmpeg making it a stream.
struct Phone {
    guest: ferrocast_guest::Guest,
    task: tokio::task::JoinHandle<()>,
    dir: PathBuf,
}

impl Drop for Phone {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Irl {
    pub fn new() -> Self {
        Irl { secret: random_id(), now: Mutex::new(None), last_offer: Mutex::new(None), subs: Mutex::new(Vec::new()) }
    }

    pub fn subscriptions(&self) -> Vec<crate::push::Subscription> {
        self.subs.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The phone's connection goes (the session ended, or a new one takes over).
    pub fn stop(&self) {
        self.now.lock().unwrap_or_else(|p| p.into_inner()).take();
    }

    fn is(&self, secret: &str) -> bool {
        bool::from(self.secret.as_bytes().ct_eq(secret.as_bytes()))
    }

    /// What the phone's connection is doing.
    fn phone(&self) -> Value {
        match self.now.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            Some(p) => {
                let s = p.guest.status();
                json!({ "connected": s.connected, "receiving": s.receiving, "ended": s.ended })
            }
            None => json!({ "connected": false, "receiving": false, "ended": false }),
        }
    }
}

pub(crate) fn routes() -> Router<Bond> {
    Router::new()
        .route("/irl", get(page))
        .route(
            "/irl/manifest.webmanifest",
            get(|| async { ([(header::CONTENT_TYPE, "application/manifest+json")], MANIFEST) }),
        )
        // Its scope is the page (`/irl`), above where the script is.
        .route(
            "/irl/sw.js",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "text/javascript"),
                        (header::HeaderName::from_static("service-worker-allowed"), "/irl"),
                    ],
                    WORKER,
                )
            }),
        )
        .route("/irl/icon-180.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], ICON_180) }))
        .route("/irl/icon-192.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], ICON_192) }))
        .route("/irl/icon-512.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], ICON_512) }))
        .route("/v1/irl/{id}", get(status))
        .route("/v1/irl/{id}/offer", post(offer))
        .route("/v1/irl/{id}/end", post(end))
        .route("/v1/irl/{id}/push", post(subscribe))
}

async fn page() -> impl IntoResponse {
    // The page talks only to this service; nothing else may run in it.
    (
        [
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; script-src 'unsafe-inline' 'self'; style-src 'unsafe-inline'; img-src 'self' data:; media-src 'self' blob: mediastream:; connect-src 'self'; frame-ancestors 'none'",
            ),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(PAGE),
    )
}

fn no_session() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "no_session",
        "This link isn't for a stream that's ready: make a new one on your computer.",
    )
}

impl Bond {
    /// The phone session `id`, when `secret` is its own.
    fn phone_session(&self, id: &str, secret: &str) -> Result<Arc<Session>, ApiError> {
        let s = self.sessions.lock().unwrap_or_else(|p| p.into_inner()).get(id).cloned();
        s.filter(|s| s.irl.as_ref().is_some_and(|i| i.is(secret))).ok_or_else(no_session)
    }
}

#[derive(Deserialize)]
struct Secret {
    s: String,
}

/// `GET /v1/irl/{id}` with the secret in `x-irl-secret` (a header, never the address: those
/// end up in logs): how the stream and its platforms are doing, for the phone.
async fn status(
    State(b): State<Bond>,
    Path(id): Path<String>,
    h: axum::http::HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let secret = h.get("x-irl-secret").and_then(|v| v.to_str().ok()).unwrap_or("");
    let s = b.phone_session(&id, secret)?;
    let mut v = s.view();
    v["phone"] = s.irl.as_ref().map_or(Value::Null, Irl::phone);
    v["picture"] = json!({ "width": s.picture.0, "height": s.picture.1, "fps": s.picture.2 });
    v["push_key"] = b.pusher.as_ref().map_or(Value::Null, |p| json!(p.public_key()));
    Ok(Json(v))
}

#[derive(Deserialize)]
struct Offer {
    s: String,
    offer: Value,
}

/// `POST /v1/irl/{id}/offer {s, offer}` → `{answer}`: the phone's WebRTC is taken (a newer
/// connection replaces the one before: the phone reconnecting).
async fn offer(State(b): State<Bond>, Path(id): Path<String>, Json(o): Json<Offer>) -> Result<Json<Value>, ApiError> {
    let session = b.phone_session(&id, &o.s)?;
    let irl = session.irl.as_ref().ok_or_else(no_session)?;
    if session.feed.lock().unwrap_or_else(|p| p.into_inner()).input == Input::Ended {
        return Err(ApiError::new(
            StatusCode::GONE,
            "ended",
            "This stream has ended: make a new one on your computer.",
        ));
    }
    let ffmpeg = b.cfg.ffmpeg.clone().ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "no_phones", "Ferrocast Bond can't take phones right now.")
    })?;
    {
        let mut last = irl.last_offer.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|t| t.elapsed() < OFFER_GAP) {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too_soon",
                "One moment: connecting again in a second.",
            ));
        }
        *last = Some(Instant::now());
    }
    let ip = b.cfg.rtc_ip.unwrap_or_else(ferrocast_guest::local_address);
    let (from, to) = b.cfg.rtc_ports;
    let socket = (from..=to).find_map(|p| std::net::UdpSocket::bind((ip, p)).ok()).ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "Ferrocast Bond is busy right now: try again in a minute.",
        )
    })?;
    let dir = std::env::temp_dir().join(format!("ferrocast-irl-{}", random_id()));
    std::fs::create_dir(&dir).map_err(|_| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "busy", "Ferrocast Bond couldn't get ready: try again.")
    })?;
    let sdp = dir.join("phone.sdp");
    let text = o.offer.to_string();
    let sdp2 = sdp.clone();
    let accepted = tokio::task::spawn_blocking(move || {
        ferrocast_guest::Guest::accept_with(&text, &sdp2, ferrocast_guest::Video::H264, socket)
    })
    .await
    .map_err(|_| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "busy", "Ferrocast Bond couldn't answer: try again.")
    })?;
    let (answer, guest) = match accepted {
        Ok(a) => a,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "bad_offer",
                format!("The phone's connection couldn't be taken: {e}"),
            ));
        }
    };
    let task = tokio::spawn(phone_in(session.clone(), sdp, ffmpeg));
    // The one before (if any) goes as this one takes over.
    *irl.now.lock().unwrap_or_else(|p| p.into_inner()) = Some(Phone { guest, task, dir });
    tracing::info!(session = %id, "Bond: a phone connected");
    let answer: Value = serde_json::from_str(&answer).unwrap_or(Value::Null);
    Ok(Json(json!({ "answer": answer })))
}

#[derive(Deserialize)]
struct Subscribe {
    s: String,
    subscription: crate::push::Subscription,
}

/// `POST /v1/irl/{id}/push {s, subscription}`: the phone takes pushes about this stream.
async fn subscribe(
    State(b): State<Bond>,
    Path(id): Path<String>,
    Json(q): Json<Subscribe>,
) -> Result<StatusCode, ApiError> {
    let s = b.phone_session(&id, &q.s)?;
    if b.pusher.is_none() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_push",
            "Notifications aren't set up on Ferrocast Bond.",
        ));
    }
    let sub = q.subscription;
    if !crate::push::allowed(&sub.endpoint) || sub.keys.p256dh.len() > 200 || sub.keys.auth.len() > 64 {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid", "subscription: a browser's push subscription."));
    }
    let irl = s.irl.as_ref().ok_or_else(no_session)?;
    let mut subs = irl.subs.lock().unwrap_or_else(|p| p.into_inner());
    subs.retain(|x| x.endpoint != sub.endpoint);
    subs.push(sub);
    while subs.len() > MAX_SUBS {
        subs.remove(0);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/irl/{id}/end {s}`: the streamer ended it on the phone.
async fn end(State(b): State<Bond>, Path(id): Path<String>, Json(q): Json<Secret>) -> Result<StatusCode, ApiError> {
    b.phone_session(&id, &q.s)?.end();
    Ok(StatusCode::NO_CONTENT)
}

/// The phone's RTP (written to `sdp`'s ports) through ffmpeg, as TS, into the session.
async fn phone_in(session: Arc<Session>, sdp: PathBuf, ffmpeg: PathBuf) {
    // Its turn starts with its first pictures: a phone that connects and then loses the signal
    // again mustn't stop Be right back.
    let mut segment: Option<u64> = None;
    let child = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(["-protocol_whitelist", "file,udp,rtp", "-reorder_queue_size", "64", "-fflags", "+genpts+nobuffer"])
        .args(["-analyzeduration", "2000000", "-probesize", "5000000", "-i"])
        .arg(&sdp)
        .args(["-map", "0:v:0", "-map", "0:a:0?", "-c:v", "copy", "-c:a", "aac", "-b:a", "128k", "-ar", "48000"])
        .args(["-f", "mpegts", "-"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = child else {
        tracing::warn!(session = %session.id, "Bond: ffmpeg didn't start for a phone");
        return;
    };
    let Some(mut out) = child.stdout.take() else { return };
    let mut demux = TsDemuxer::new();
    let mut buf = vec![0u8; 188 * 64];
    loop {
        let n = match out.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let tags = demux.push(&buf[..n]);
        if tags.is_empty() {
            continue;
        }
        let seg = *segment.get_or_insert_with(|| session.begin());
        session.heard();
        for t in &tags {
            if !session.send(seg, t) {
                return;
            }
        }
    }
    tracing::info!(session = %session.id, "Bond: the phone's stream stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_answers_only_its_own_secret() {
        let i = Irl::new();
        assert!(i.is(&i.secret.clone()));
        assert!(!i.is("nope"));
        assert!(!i.is(""));
        assert_eq!(i.phone()["connected"], false);
        assert!(PAGE.contains("<title>") && MANIFEST.contains("\"name\"") && WORKER.contains("push"));
    }
}
