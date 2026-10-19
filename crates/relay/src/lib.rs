//! The Ferrocast relay (ARCHITECTURE.md §1): the only server Ferrocast runs.
//!
//! It exists because some things need a public URL or a client secret: Kick's webhooks,
//! Kick's token exchange, payment webhooks for the tip page, and Ferrocast Pro billing
//! (`billing`). Everything it
//! receives is forwarded to the streamer's desktop over an outbound WebSocket (no inbound
//! ports on their PC), queued while the desktop is offline (R-01, R-13 in VISION).
//!
//! Pro's features (docs/plan/PRO.md) each live in their own module with their own
//! `routes()`, merged in [`router`], and their own migrations (`migrate`). The shared parts:
//! `billing::gate` (402 without Pro), `vault` (encrypted secrets), `quota` (per-subscription
//! counters) and `blob` (files).

pub mod backups;
pub mod billing;
pub mod blob;
pub mod bot;
pub mod db;
pub mod discord;
mod history;
pub mod kick;
pub mod mail;
mod migrate;
pub mod quota;
pub mod remote;
pub mod tips;
pub mod tts;
pub mod vault;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use rsa::sha2::{Digest, Sha256};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::Notify;

pub use db::Db;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Read from the environment at start (see `deploy/relay.env.example`).
#[derive(Debug, Clone)]
pub struct Config {
    /// The relay's public base URL, e.g. `https://ferrocast-relay.clearcraftlabs.com`.
    pub public_url: String,
    /// Where tip pages live, an origin such as `https://tip.clearcraftlabs.com` (any path in
    /// `RELAY_TIP_URL` is dropped): a page is `<tip_url>/<name>`.
    /// Unset: tip pages are `<public_url>/t/<name>`. Old `/t/` links keep working either way.
    pub tip_url: Option<String>,
    pub kick_client_id: Option<String>,
    pub kick_client_secret: Option<String>,
    pub kick_id_url: String,
    pub kick_api_url: String,
}

impl Config {
    pub fn from_env() -> Config {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Config {
            public_url: env("RELAY_PUBLIC_URL")
                .unwrap_or_else(|| "http://127.0.0.1:8787".into())
                .trim_end_matches('/')
                .to_string(),
            // Only the origin counts: pages are served at `/<name>` on that host.
            tip_url: env("RELAY_TIP_URL")
                .and_then(|u| reqwest::Url::parse(&u).ok())
                .map(|u| u.origin().ascii_serialization())
                .filter(|o| o != "null"),
            kick_client_id: env("KICK_CLIENT_ID"),
            kick_client_secret: env("KICK_CLIENT_SECRET"),
            kick_id_url: env("KICK_ID_URL").unwrap_or_else(|| "https://id.kick.com".into()),
            kick_api_url: env("KICK_API_URL").unwrap_or_else(|| "https://api.kick.com".into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Arc<Db>,
    pub http: reqwest::Client,
    pub keys: Arc<kick::KeyCache>,
    /// Ferrocast Pro billing (Razorpay); off until its keys are set.
    pub billing_cfg: Arc<billing::BillingConfig>,
    pub(crate) billing: Arc<billing::Runtime>,
    /// Encrypts the secrets Pro keeps; `None` without `RELAY_VAULT_KEY`.
    vault: Option<Arc<vault::Vault>>,
    /// Pro's files; `None` until the store is set up.
    blobs: Option<Arc<dyn blob::BlobStore>>,
    /// Pro voices: the provider (`None` without `GOOGLE_TTS_KEY`) and the relay-wide caps.
    pub(crate) tts: Arc<tts::Runtime>,
    /// The dashboard from anywhere: its settings, open tunnels and phone sockets.
    pub(crate) remote: Arc<remote::Runtime>,
    /// The Ferrocast bot for Discord (off without the `DISCORD_*` settings).
    pub(crate) discord: Arc<discord::Runtime>,
    /// The weekly email (off without `BREVO_API_KEY`).
    pub(crate) mail: Arc<mail::Runtime>,
    /// The 24/7 cloud bot (Twitch off without `TWITCH_CLOUD_*`, Kick with the Kick app).
    pub(crate) bot: Arc<bot::Runtime>,
    /// Pro's custom tip addresses (off without `CF_SAAS_TOKEN` / `CF_ZONE_ID`).
    pub(crate) tip_domains: Arc<tips::domains::Runtime>,
    /// Wakes an install's link socket when something is queued for it.
    wakers: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
    /// Install registrations per IP per minute.
    signups: Arc<Mutex<HashMap<std::net::IpAddr, (i64, u32)>>>,
    /// Install registrations from everyone this hour: (hour, count).
    signups_hour: Arc<Mutex<(i64, u32)>>,
}

impl AppState {
    pub fn new(cfg: Config, db: Db) -> AppState {
        AppState {
            cfg: Arc::new(cfg),
            db: Arc::new(db),
            http: reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap_or_default(),
            keys: Arc::default(),
            billing_cfg: Arc::default(),
            billing: Arc::default(),
            vault: None,
            blobs: None,
            tts: Arc::default(),
            remote: Arc::default(),
            discord: Arc::default(),
            mail: Arc::default(),
            bot: Arc::default(),
            tip_domains: Arc::default(),
            wakers: Arc::default(),
            signups: Arc::default(),
            signups_hour: Arc::default(),
        }
    }

    /// Billing settings (`billing::BillingConfig::from_env()` in production).
    pub fn with_billing(mut self, cfg: billing::BillingConfig) -> AppState {
        self.billing_cfg = Arc::new(cfg);
        self.billing = Arc::default();
        self
    }

    /// The vault (`vault::Vault::from_env()` in production).
    pub fn with_vault(mut self, vault: Option<vault::Vault>) -> AppState {
        self.vault = vault.map(Arc::new);
        self
    }

    /// Where Pro keeps files (`blob::FsBlobStore` in production).
    pub fn with_blobs(mut self, blobs: Option<Arc<dyn blob::BlobStore>>) -> AppState {
        self.blobs = blobs;
        self
    }

    /// Pro voices (`tts::GoogleTts::from_env()` in production) and the relay-wide monthly
    /// character cap (`tts::global_cap_from_env()`).
    pub fn with_tts(mut self, provider: Option<Arc<dyn tts::TtsProvider>>, global_monthly_chars: i64) -> AppState {
        self.tts = Arc::new(tts::Runtime::new(provider, global_monthly_chars));
        self
    }

    /// The dashboard from anywhere (`remote::RemoteConfig::from_env()` in production).
    pub fn with_remote(mut self, cfg: remote::RemoteConfig) -> AppState {
        self.remote = Arc::new(remote::Runtime::new(cfg));
        self
    }

    /// The Ferrocast bot for Discord (`discord::Runtime::from_config(DiscordConfig::from_env())`
    /// in production, a fake Discord in tests).
    pub fn with_discord(mut self, rt: discord::Runtime) -> AppState {
        self.discord = Arc::new(rt);
        self
    }

    /// The weekly email: the sender (`mail::BrevoMailer::from_env()` in production, a
    /// `mail::FakeMailer` in tests) and its settings (`mail::MailConfig::from_env()`).
    pub fn with_mail(mut self, mailer: Option<Arc<dyn mail::Mailer>>, cfg: mail::MailConfig) -> AppState {
        self.mail = Arc::new(mail::Runtime::new(mailer, cfg));
        self
    }

    /// The 24/7 cloud bot (`bot::Runtime::from_config(TwitchConfig::from_env(), &cfg)` in
    /// production, fake platforms in tests).
    pub fn with_bot(mut self, rt: bot::Runtime) -> AppState {
        self.bot = Arc::new(rt);
        self
    }

    /// Custom tip addresses: Cloudflare for SaaS (`tips::domains::CloudflareHostnames::from_env()`
    /// in production, a `FakeHostnames` in tests), the DNS the TXT proof is read with (`None`:
    /// Cloudflare's DNS-over-HTTPS; a `FakeDns` in tests), the secret its values are keyed
    /// with (the decoded `RELAY_VAULT_KEY`; without one the feature stays off) and how many
    /// addresses the relay takes in all.
    pub fn with_tip_domains(
        mut self,
        cf: Option<Arc<dyn tips::domains::CustomHostnames>>,
        dns: Option<Arc<dyn tips::domains::ProofDns>>,
        secret: Option<&[u8]>,
        cap: usize,
    ) -> AppState {
        self.tip_domains = Arc::new(tips::domains::Runtime::new(cf, dns, secret, cap));
        self
    }

    /// The vault, or 503 `vault_unavailable` for a feature that needs it.
    #[allow(dead_code)] // the first users are M1+ features
    pub(crate) fn vault(&self) -> Result<&vault::Vault, ApiError> {
        self.vault.as_deref().ok_or_else(vault::unavailable)
    }

    /// The file store, or 503 `storage_unavailable` for a feature that needs it.
    #[allow(dead_code)] // the first users are M1+ features
    pub(crate) fn blobs(&self) -> Result<&dyn blob::BlobStore, ApiError> {
        self.blobs.as_deref().ok_or_else(blob::unavailable)
    }

    fn waker(&self, install: &str) -> Arc<Notify> {
        self.wakers.lock().unwrap_or_else(|p| p.into_inner()).entry(install.to_string()).or_default().clone()
    }

    pub(crate) fn enqueue(&self, install: &str, kind: &str, payload: &Value) -> Result<(), ApiError> {
        self.db.enqueue(install, kind, &payload.to_string()).map_err(ApiError::db)?;
        self.waker(install).notify_one();
        Ok(())
    }
}

/// `{error:{code,message,hint}}` with the right status, never a bare 500.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    hint: Option<String>,
    /// Extra response headers (`Retry-After`, `X-Quota-Reset`).
    headers: Vec<(axum::http::HeaderName, String)>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError { status, code, message: message.into(), hint: None, headers: Vec::new() }
    }
    pub(crate) fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub(crate) fn header(mut self, name: axum::http::HeaderName, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub(crate) fn db(e: rusqlite::Error) -> Self {
        tracing::error!(error = %e, "relay database error");
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "storage", "The relay couldn't save that. Try again shortly.")
    }
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
    fn unauthorized() -> Self {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "This Ferrocast install isn't recognised.")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut res = (
            self.status,
            axum::Json(json!({ "error": { "code": self.code, "message": self.message, "hint": self.hint } })),
        )
            .into_response();
        for (name, value) in self.headers {
            if let Ok(v) = axum::http::HeaderValue::from_str(&value) {
                res.headers_mut().insert(name, v);
            }
        }
        res
    }
}

/// New installs accepted per hour across all addresses.
pub const MAX_SIGNUPS_PER_HOUR: u32 = 2_000;

/// The largest request body the relay reads.
pub const BODY_LIMIT: usize = 256 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/installs", post(new_install))
        .route("/v1/link", get(link))
        .route("/v1/kick/oauth", post(kick_oauth))
        .route("/v1/kick/callback", get(kick_callback))
        .route("/v1/kick/refresh", post(kick_refresh))
        .route("/v1/kick/webhook", post(kick_webhook))
        // Each feature's own routes: later milestones only change their module.
        .merge(tips::routes())
        .merge(billing::routes())
        .merge(history::routes())
        .merge(backups::routes())
        .merge(remote::routes())
        .merge(bot::routes())
        .merge(tts::routes())
        .merge(discord::routes())
        .merge(mail::routes())
        .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT))
        // A streamer's own tip address (`Host` is theirs) gets only their tip pages.
        .layer(axum::middleware::from_fn_with_state(state.clone(), tips::domains::route_custom_hosts))
        .with_state(state)
}

fn random(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    // No OS randomness means nothing secure is possible; refuse loudly.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// The caller's address. Behind our own proxy (the peer is loopback, or a private address
/// such as the Cloudflare Tunnel container next to us) that's `CF-Connecting-IP`, else the
/// last `X-Forwarded-For` hop the proxy added. A caller from the internet reaches the relay
/// only through the proxy, so it can't pick its own address.
pub fn client_ip(peer: std::net::SocketAddr, headers: &HeaderMap) -> std::net::IpAddr {
    let header_ip = |name: &str, last: bool| {
        headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| {
            let part = if last { v.rsplit(',').next() } else { v.split(',').next() };
            part.and_then(|p| p.trim().parse().ok())
        })
    };
    if is_private(peer.ip())
        && let Some(ip) = header_ip("cf-connecting-ip", false).or_else(|| header_ip("x-forwarded-for", true))
    {
        return ip;
    }
    peer.ip()
}

fn is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback() || v4.is_private())
        }
    }
}

fn hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks `Authorization: Bearer <install>.<secret>` and returns the install id.
pub(crate) fn authed(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    let raw = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(ApiError::unauthorized)?;
    let (id, secret) = raw.trim().split_once('.').ok_or_else(ApiError::unauthorized)?;
    let stored = state.db.install_hash(id).map_err(ApiError::db)?.ok_or_else(ApiError::unauthorized)?;
    if !bool::from(stored.as_bytes().ct_eq(hash(secret).as_bytes())) {
        return Err(ApiError::unauthorized());
    }
    let _ = state.db.touch_install(id);
    Ok(id.to_string())
}

async fn new_install(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let ip = client_ip(addr, &headers);
    {
        let mut s = st.signups.lock().unwrap_or_else(|p| p.into_inner());
        let minute = now_ms() / 60_000;
        // Keep the map to the current minute so it can't grow without bound.
        s.retain(|_, (m, _)| *m == minute);
        let e = s.entry(ip).or_insert((minute, 0));
        e.1 += 1;
        if e.1 > 10 {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Too many new installs from this address. Wait a minute.",
            ));
        }
    }
    {
        // Many addresses together can't flood the database either.
        let mut h = st.signups_hour.lock().unwrap_or_else(|p| p.into_inner());
        let hour = now_ms() / 3_600_000;
        if h.0 != hour {
            *h = (hour, 0);
        }
        h.1 += 1;
        if h.1 > MAX_SIGNUPS_PER_HOUR {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                "The relay is taking a lot of new installs right now.",
            )
            .hint("Ferrocast retries on its own; nothing to do."));
        }
    }
    let id = random(12);
    let secret = random(32);
    st.db.add_install(&id, &hash(&secret)).map_err(ApiError::db)?;
    Ok((StatusCode::CREATED, axum::Json(json!({ "install_id": id, "install_secret": secret })).into_response())
        .into_response())
}

/// The desktop's link: queued items first, then live ones; each deleted only when acked.
async fn link(State(st): State<AppState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    Ok(ws.max_message_size(64 * 1024).on_upgrade(move |socket| link_loop(st, install, socket)))
}

async fn link_loop(st: AppState, install: String, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let wake = st.waker(&install);
    let mut sent_up_to = 0i64;
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    loop {
        // Send anything not yet sent on this connection.
        match st.db.pending(&install, sent_up_to, 200) {
            Ok(items) => {
                for it in items {
                    let frame = json!({ "type": "item", "id": it.id, "kind": it.kind, "payload": serde_json::from_str::<Value>(&it.payload).unwrap_or(Value::Null) });
                    if tx.send(Message::Text(Utf8Bytes::from(frame.to_string()))).await.is_err() {
                        return;
                    }
                    sent_up_to = it.id;
                }
            }
            Err(e) => tracing::error!(error = %e, "couldn't read the queue"),
        }
        tokio::select! {
            _ = wake.notified() => {}
            _ = ping.tick() => {
                if tx.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
            }
            msg = rx.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        if v["type"] == "ack"
                            && let Some(id) = v["id"].as_i64()
                        {
                            let _ = st.db.ack(&install, id);
                        } else if v["type"] == "bot_seen" {
                            // The PC's bot is on: the cloud bot stands by (bot::lease).
                            bot::seen_frame(&st, &install, &v);
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

fn kick_app(st: &AppState) -> Result<(String, String), ApiError> {
    match (&st.cfg.kick_client_id, &st.cfg.kick_client_secret) {
        (Some(id), Some(secret)) => Ok((id.clone(), secret.clone())),
        _ => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "kick_unavailable",
            "Kick sign-in isn't set up on this relay yet.",
        )),
    }
}

/// Starts Kick sign-in for an install: returns the URL to open in the browser.
async fn kick_oauth(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let (client_id, _) = kick_app(&st)?;
    let verifier = random(48);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random(24);
    st.db.put_oauth(&state, &install, &verifier).map_err(ApiError::db)?;
    let mut url = url::Url::parse(&format!("{}/oauth/authorize", st.cfg.kick_id_url)).map_err(|_| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "config", "The relay's Kick address is wrong.")
    })?;
    url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &format!("{}/v1/kick/callback", st.cfg.public_url))
        .append_pair("scope", &KICK_SCOPES.join(" "))
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    Ok(axum::Json(json!({ "url": url.to_string() })).into_response())
}

/// Every Kick scope Ferrocast uses (mirrors `ferrocast_kick::SCOPES`).
pub const KICK_SCOPES: &[&str] = &[
    "user:read",
    "channel:read",
    "chat:write",
    "events:subscribe",
    "moderation:ban",
    "moderation:chat_message:manage",
    "channel:rewards:read",
    "kicks:read",
    "channel:write",
];

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// The look shared by the sign-in result pages: one card, light and dark, no scripts.
/// `body` is already-escaped HTML.
pub(crate) fn card(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <meta name=robots content=noindex><title>{t} · Ferrocast</title>\
         <style>:root{{color-scheme:dark light;--bg:#0a0b0d;--card:#101216;--line:#252a31;--text:#eef0f4;--muted:#b3b9c4;--chip:#1a1d23}}\
         @media (prefers-color-scheme:light){{:root{{--bg:#f2f3f6;--card:#fff;--line:#dfe2e8;--text:#0e1014;--muted:#424854;--chip:#f2f3f6}}}}\
         body{{margin:0;min-height:100vh;display:grid;place-items:center;padding:16px;box-sizing:border-box;background:var(--bg);color:var(--text);font:17px/1.5 system-ui,sans-serif}}\
         main{{width:100%;max-width:460px;background:var(--card);border:1px solid var(--line);border-radius:16px;padding:28px;box-sizing:border-box}}\
         h1{{font-size:1.6rem;line-height:1.2;margin:0 0 12px;letter-spacing:-.02em;overflow-wrap:anywhere}}p{{margin:0 0 12px;overflow-wrap:anywhere}}\
         .next{{font-weight:600}}.code{{display:inline-block;margin:4px 0 16px;padding:6px 12px;border:1px solid var(--line);border-radius:10px;background:var(--chip);font:600 1.05rem ui-monospace,monospace;letter-spacing:.06em}}\
         .muted{{color:var(--muted);font-size:.9rem;margin:16px 0 0;padding-top:12px;border-top:1px solid var(--line)}}</style>\
         <main><h1>{t}</h1>{body}</main></html>",
        t = mail::report::esc(title),
    );
    let mut r = (status, Html(html)).into_response();
    let h = r.headers_mut();
    h.insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    h.insert(axum::http::header::REFERRER_POLICY, axum::http::HeaderValue::from_static("no-referrer"));
    h.insert(axum::http::header::CONTENT_SECURITY_POLICY, axum::http::HeaderValue::from_static(mail::pages::CSP));
    r
}

/// A sign-in that worked: what happened, what to press next in Ferrocast, the PC's code,
/// and a quiet footnote for someone who didn't start it. Plain text in, escaped here.
pub(crate) fn done_page(what: &str, next: &str, code: &str, footnote: &str) -> Response {
    let e = mail::report::esc;
    card(
        StatusCode::OK,
        "Done",
        &format!(
            "<p>{}</p><p class=next>{}</p><p>This is for the Ferrocast PC with the code</p><span class=code>{}</span>\
             <p>The Bot page there shows the same code.</p><p class=muted>{}</p>",
            e(what),
            e(next),
            e(code),
            e(footnote)
        ),
    )
}

fn page(status: StatusCode, text: &str) -> Response {
    let title = if status.is_success() { "Ferrocast" } else { "Something went wrong" };
    card(status, title, &format!("<p>{}</p>", mail::report::esc(text)))
}

/// Kick sends the streamer back here. Trades the code (with the secret), learns whose
/// channel it is, and hands the tokens to that desktop through its queue.
async fn kick_callback(State(st): State<AppState>, Query(q): Query<Callback>) -> Response {
    if let Some(e) = q.error {
        return page(StatusCode::OK, &format!("Kick sign-in was cancelled ({e}). You can close this tab."));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return page(
            StatusCode::BAD_REQUEST,
            "This link is incomplete. Go back to Ferrocast and press Connect Kick again.",
        );
    };
    let Ok(Some((install, verifier))) = st.db.take_oauth(&state) else {
        return page(
            StatusCode::BAD_REQUEST,
            "This sign-in link expired or was already used. Press Connect Kick again in Ferrocast.",
        );
    };
    let Ok((client_id, secret)) = kick_app(&st) else {
        return page(StatusCode::SERVICE_UNAVAILABLE, "Kick sign-in isn't set up on this relay yet.");
    };
    let redirect = format!("{}/v1/kick/callback", st.cfg.public_url);
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id.as_str()),
        ("client_secret", secret.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("code_verifier", verifier.as_str()),
        ("code", code.as_str()),
    ];
    let tokens = match st.http.post(format!("{}/oauth/token", st.cfg.kick_id_url)).form(&form).send().await {
        Ok(r) if r.status().is_success() => r.json::<kick::TokenResponse>().await.ok(),
        _ => None,
    };
    let Some(tokens) = tokens else {
        return page(StatusCode::BAD_GATEWAY, "Kick didn't accept the sign-in. Press Connect Kick again in Ferrocast.");
    };
    let user: Option<Value> = match st
        .http
        .get(format!("{}/public/v1/users", st.cfg.kick_api_url))
        .bearer_auth(&tokens.access_token)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.json().await.ok(),
        _ => None,
    };
    let Some(user) = user.and_then(|u| u.pointer("/data/0").cloned()) else {
        return page(StatusCode::BAD_GATEWAY, "Kick didn't say whose account this is. Try again.");
    };
    let broadcaster = match &user["user_id"] {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return page(StatusCode::BAD_GATEWAY, "Kick sent an account without an id."),
    };
    if state.starts_with(bot::kick::STATE_PREFIX) {
        // The cloud bot's opt-in sign-in: kept here (sealed), never sent to the PC, and the
        // channel's owner stays as it is.
        let name = user["name"].as_str().unwrap_or("Kick");
        return bot::kick::finish(&st, &install, &tokens, &broadcaster, name, &state).await;
    }
    if st.db.set_kick_owner(&broadcaster, &install).is_err() {
        return page(StatusCode::SERVICE_UNAVAILABLE, "The relay couldn't save that. Try again shortly.");
    }
    let payload = json!({
        "tokens": { "access_token": tokens.access_token, "refresh_token": tokens.refresh_token,
                    "expires_at": now_ms() + tokens.expires_in.clamp(0, 86_400 * 30) * 1000 },
        "user": { "id": broadcaster, "name": user["name"], "avatar_url": user["profile_picture"] },
    });
    if st.enqueue(&install, "kick_connected", &payload).is_err() {
        return page(StatusCode::SERVICE_UNAVAILABLE, "The relay couldn't save that. Try again shortly.");
    }
    page(StatusCode::OK, "Kick is connected to Ferrocast. You can close this tab.")
}

#[derive(Deserialize)]
struct RefreshBody {
    refresh_token: String,
}

/// Refreshes a Kick token for an install (the client secret never leaves the relay).
async fn kick_refresh(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(b): axum::Json<RefreshBody>,
) -> Result<Response, ApiError> {
    authed(&st, &headers)?;
    let (client_id, secret) = kick_app(&st)?;
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", client_id.as_str()),
        ("client_secret", secret.as_str()),
        ("refresh_token", b.refresh_token.as_str()),
    ];
    let res =
        st.http.post(format!("{}/oauth/token", st.cfg.kick_id_url)).form(&form).send().await.map_err(|_| {
            ApiError::new(StatusCode::BAD_GATEWAY, "kick_unreachable", "Couldn't reach Kick. Try again.")
        })?;
    if !res.status().is_success() {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "sign_in_needed", "Kick needs you to sign in again."));
    }
    let t: kick::TokenResponse = res
        .json()
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "kick_unreachable", "Kick sent a bad answer."))?;
    Ok(axum::Json(json!({ "access_token": t.access_token, "refresh_token": t.refresh_token,
                          "expires_at": now_ms() + t.expires_in.clamp(0, 86_400 * 30) * 1000 }))
    .into_response())
}

/// Kick's webhook: verify, drop replays, route by channel, queue for the desktop. Always
/// quick, so Kick never times out and unsubscribes us.
async fn kick_webhook(State(st): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let (id, ts, sig, ty, ver) = (
        h("kick-event-message-id"),
        h("kick-event-message-timestamp"),
        h("kick-event-signature"),
        h("kick-event-type"),
        h("kick-event-version"),
    );
    if id.is_empty() || ts.is_empty() || sig.is_empty() || ty.is_empty() {
        return ApiError::new(StatusCode::BAD_REQUEST, "missing_headers", "Kick event headers are missing.")
            .into_response();
    }
    let delivery = kick::Delivery { message_id: &id, timestamp: &ts, body: &body, signature: &sig };
    if let Err(e) = st.keys.verify(&st.http, &st.cfg.kick_api_url, &delivery, now_ms()).await {
        tracing::warn!(error = %e, "refused a Kick webhook");
        return ApiError::new(StatusCode::UNAUTHORIZED, "bad_signature", "The event isn't signed by Kick.")
            .into_response();
    }
    // Seen before? Answer OK so Kick stops retrying. The id is recorded only after the
    // event is safely queued, so a failed write lets Kick's retry through.
    match st.db.seen(&id) {
        Ok(true) => return StatusCode::OK.into_response(),
        Ok(false) => {}
        Err(e) => return ApiError::db(e).into_response(),
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return ApiError::new(StatusCode::BAD_REQUEST, "bad_json", "The event body isn't JSON.").into_response();
    };
    let broadcaster = match v.pointer("/broadcaster/user_id") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => return StatusCode::OK.into_response(),
    };
    match st.db.kick_owner(&broadcaster) {
        Ok(Some(install)) => {
            let payload = json!({ "type": ty, "version": ver, "message_id": id, "body": v });
            if let Err(e) = st.enqueue(&install, "kick_event", &payload) {
                return e.into_response();
            }
            // The cloud bot answers while the PC is off (bot::kick): queued, bounded, never
            // holding up Kick.
            bot::kick::queue_webhook(&st, install, broadcaster, ty.clone(), id.clone(), v);
        }
        Ok(None) => tracing::debug!(%broadcaster, "a Kick event for a channel no install owns"),
        Err(e) => return ApiError::db(e).into_response(),
    }
    if let Err(e) = st.db.first_time(&id) {
        tracing::warn!(error = %e, "couldn't record a webhook id; a retry may be queued twice (the desktop dedupes)");
    }
    StatusCode::OK.into_response()
}

#[cfg(test)]
mod lapse_tests;
#[cfg(test)]
mod load_tests;
#[cfg(test)]
mod tests;
