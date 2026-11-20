//! An obs-websocket v5 server (opt-in), so Stream Deck, Touch Portal, Bitfocus Companion,
//! Streamer.bot and the rest that speak OBS's remote protocol can drive Ferrocast.
//!
//! Off until the owner switches it on in Settings. It binds to 127.0.0.1 unless "allow other
//! devices on my network" is on, and a password is always required: made at random the first
//! time it's switched on, shown once, and kept only as OBS keeps it (a salt and the hash
//! `base64(sha256(password + salt))`), never as text and never in a log.
//!
//! Every request is mapped onto what the dashboard's own API already calls (`studio_act`,
//! `studio_stream`, `studio_record`…), so nothing here changes the Studio by a path of its own.
//! Events come from looking at the Studio a few times a second while a client is connected and
//! telling what changed, so a change made in the dashboard or by a shortcut reaches a Stream Deck
//! too. Requests the protocol has and this doesn't answer 204 (unknown request type).
//!
//! Protocol: `obswebsocket.json` (JSON text frames), RPC version 1. Written from the protocol's
//! documented message shapes and status codes; the `obsproject/obs-websocket` docs weren't on
//! this machine to check against (the checkout's submodule is empty).

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use ferrocast_store::repo;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore, broadcast, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig, frame::coding::CloseCode};

use crate::studio_host::{AudioChange, FilterEdit, ItemEdit, StudioAct};
use crate::{Engine, EngineError};

const SAVE_KEY: &str = "obsws";
pub const DEFAULT_PORT: u16 = 4455;
/// Clients connected at once; more are turned away.
pub const MAX_CLIENTS: usize = 8;
/// The largest message taken (a request is a few hundred bytes).
/// Connections that haven't identified yet, at most: so a page or program that can't log in
/// can't hold every place and keep a Stream Deck out.
const MAX_PENDING: usize = 3;
/// A wrong password waits this long before the answer, which slows guessing from anywhere.
const FAIL_DELAY: Duration = Duration::from_secs(1);
const MAX_MESSAGE: usize = 256 * 1024;
const IDENTIFY_WITHIN: Duration = Duration::from_secs(10);
const SEND_WITHIN: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(250);
/// Failed passwords from one address before it's shut out, and for how long.
const MAX_FAILS: u32 = 5;
const FAIL_WINDOW: Duration = Duration::from_secs(60);
const SHUT_OUT: Duration = Duration::from_secs(60);
const RPC_VERSION: u64 = 1;
const WS_VERSION: &str = "5.5.4";
const SUBPROTOCOL: &str = "obswebsocket.json";
const MAX_BATCH: usize = 100;

// Event subscription bits (EventSubscription).
const SUB_SCENES: u64 = 1 << 2;
const SUB_INPUTS: u64 = 1 << 3;
const SUB_OUTPUTS: u64 = 1 << 6;
const SUB_SCENE_ITEMS: u64 = 1 << 7;
const SUB_UI: u64 = 1 << 10;
/// Everything but the high-volume events (obs-websocket's `All`).
const SUB_ALL: u64 = (1 << 11) - 1;

// Close codes (WebSocketCloseCode).
const CLOSE_DECODE: u16 = 4002;
const CLOSE_MISSING: u16 = 4003;
const CLOSE_BAD_TYPE: u16 = 4004;
const CLOSE_BAD_OP: u16 = 4006;
const CLOSE_NOT_IDENTIFIED: u16 = 4007;
const CLOSE_ALREADY: u16 = 4008;
const CLOSE_AUTH: u16 = 4009;
const CLOSE_RPC: u16 = 4010;
const CLOSE_INVALIDATED: u16 = 4011;

// Request status codes (RequestStatus).
const ST_OK: u16 = 100;
const ST_NOT_READY: u16 = 207;
const ST_MISSING_TYPE: u16 = 203;
const ST_UNKNOWN: u16 = 204;
const ST_MISSING_FIELD: u16 = 300;
const ST_BAD_TYPE: u16 = 401;
const ST_OUT_OF_RANGE: u16 = 402;
const ST_RUNNING: u16 = 500;
const ST_NOT_RUNNING: u16 = 501;
const ST_PAUSED: u16 = 502;
const ST_NOT_PAUSED: u16 = 503;
const ST_STUDIO_NOT_ACTIVE: u16 = 506;
const ST_NOT_FOUND: u16 = 600;
const ST_FAILED: u16 = 702;

/// A request's failure: the protocol's status code and a comment.
type Fail = (u16, String);

/// What's kept on disk. The password itself never is.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    enabled: bool,
    port: u16,
    lan: bool,
    /// base64 salt and `base64(sha256(password + salt))`, as obs-websocket keeps them.
    salt: String,
    secret: String,
}

impl Default for Saved {
    fn default() -> Self {
        Self { enabled: false, port: DEFAULT_PORT, lan: false, salt: String::new(), secret: String::new() }
    }
}

/// What Settings shows.
#[derive(Debug, Clone, Serialize)]
pub struct ObsWsView {
    pub enabled: bool,
    pub port: u16,
    pub lan: bool,
    pub has_password: bool,
    /// Clients connected and identified now.
    pub clients: usize,
    /// The server is listening (it may not be even when on: the port can be taken).
    pub listening: bool,
    /// Why it isn't listening, in words.
    pub error: Option<String>,
}

/// A change to the settings (anything left out stays).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ObsWsPatch {
    pub enabled: Option<bool>,
    pub port: Option<u16>,
    pub lan: Option<bool>,
}

#[derive(Debug, Clone, Copy)]
struct Fails {
    count: u32,
    since: Instant,
    shut_until: Option<Instant>,
}

#[derive(Debug)]
pub(crate) struct Runtime {
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Bumped on every change: connections made before it close.
    generation: watch::Sender<u64>,
    clients: AtomicUsize,
    events: broadcast::Sender<(u64, Arc<str>)>,
    poll_now: Notify,
    status: std::sync::Mutex<(bool, Option<String>)>,
    fails: std::sync::Mutex<HashMap<IpAddr, Fails>>,
    slots: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    poller: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            task: std::sync::Mutex::new(None),
            generation: watch::channel(0).0,
            clients: AtomicUsize::new(0),
            events: broadcast::channel(256).0,
            poll_now: Notify::new(),
            status: std::sync::Mutex::new((false, None)),
            fails: std::sync::Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_CLIENTS)),
            pending: Arc::new(Semaphore::new(MAX_PENDING)),
            poller: std::sync::Mutex::new(None),
        }
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn sha_b64(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
    }
    b64(&h.finalize())
}

/// Equal without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// The system's secure randomness, or None: a password or challenge is never made from anything
/// weaker (a guessable password would let anyone who can reach the port drive the Studio).
fn random_bytes<const N: usize>() -> Option<[u8; N]> {
    let mut b = [0u8; N];
    match getrandom::fill(&mut b) {
        Ok(()) => Some(b),
        Err(e) => {
            tracing::error!(error = %e, "no system randomness for obs-websocket");
            None
        }
    }
}

/// A password of 24 letters and digits (about 140 bits).
fn new_password() -> Option<String> {
    const SET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    Some(random_bytes::<24>()?.iter().map(|b| SET[usize::from(*b) % SET.len()] as char).collect())
}

/// A new password with its salt and stored hash, or why there can't be one.
fn new_secret() -> Result<(String, String, String), EngineError> {
    let none = || EngineError::Invalid("This computer gave no secure randomness for a password: try again.".into());
    let pw = new_password().ok_or_else(none)?;
    let (salt, secret) = derive(&pw).ok_or_else(none)?;
    Ok((pw, salt, secret))
}

/// The salt and the stored hash for `password`.
fn derive(password: &str) -> Option<(String, String)> {
    let salt = b64(&random_bytes::<32>()?);
    let secret = sha_b64(&[password, &salt]);
    Some((salt, secret))
}

/// What a client answers a challenge with: `base64(sha256(secret + challenge))`.
fn answer(secret: &str, challenge: &str) -> String {
    sha_b64(&[secret, challenge])
}

/// OBS's way for a client to work out the secret from the password and the salt.
#[cfg(test)]
fn client_auth(password: &str, salt: &str, challenge: &str) -> String {
    answer(&sha_b64(&[password, salt]), challenge)
}

/// A stable id (UUID-shaped) for a name: Ferrocast's scenes and inputs have none of their own.
fn uuid_of(name: &str) -> String {
    let h = Sha256::digest(name.as_bytes());
    let x: Vec<String> = h[..16].iter().map(|b| format!("{b:02x}")).collect();
    let x = x.concat();
    format!("{}-{}-4{}-a{}-{}", &x[0..8], &x[8..12], &x[13..16], &x[17..20], &x[20..32])
}

fn fail(code: u16, comment: impl Into<String>) -> Fail {
    (code, comment.into())
}

fn field<'a>(d: &'a Value, key: &str) -> Result<&'a Value, Fail> {
    match d.get(key) {
        Some(v) if !v.is_null() => Ok(v),
        _ => Err(fail(ST_MISSING_FIELD, format!("Your request is missing the `{key}` field."))),
    }
}

fn str_field<'a>(d: &'a Value, key: &str) -> Result<&'a str, Fail> {
    field(d, key)?.as_str().ok_or_else(|| fail(ST_BAD_TYPE, format!("`{key}` must be a string.")))
}

fn bool_field(d: &Value, key: &str) -> Result<bool, Fail> {
    field(d, key)?.as_bool().ok_or_else(|| fail(ST_BAD_TYPE, format!("`{key}` must be true or false.")))
}

fn num_field(d: &Value, key: &str) -> Result<f64, Fail> {
    field(d, key)?.as_f64().ok_or_else(|| fail(ST_BAD_TYPE, format!("`{key}` must be a number.")))
}

fn db_of(mul: f64) -> f64 {
    if mul <= 0.0 { -100.0 } else { (20.0 * mul.log10()).max(-100.0) }
}

fn mul_of(db: f64) -> f64 {
    if db <= -100.0 { 0.0 } else { 10f64.powf(db / 20.0) }
}

fn timecode(seconds: f64) -> String {
    let ms = (seconds.max(0.0) * 1000.0) as u64;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

fn obs_error(e: EngineError) -> Fail {
    match e {
        EngineError::Refused { code: "studio_unavailable", message, .. } => fail(ST_NOT_READY, message),
        other => fail(ST_FAILED, other.to_string()),
    }
}

/// The requests answered (what `GetVersion` lists).
const REQUESTS: &[&str] = &[
    "GetVersion",
    "GetStats",
    "GetSceneList",
    "GetCurrentProgramScene",
    "SetCurrentProgramScene",
    "GetCurrentPreviewScene",
    "SetCurrentPreviewScene",
    "GetStudioModeEnabled",
    "SetStudioModeEnabled",
    "TriggerStudioModeTransition",
    "GetInputList",
    "GetInputMute",
    "SetInputMute",
    "ToggleInputMute",
    "GetInputVolume",
    "SetInputVolume",
    "GetSceneItemList",
    "GetSceneItemId",
    "GetSceneItemEnabled",
    "SetSceneItemEnabled",
    "GetStreamStatus",
    "StartStream",
    "StopStream",
    "ToggleStream",
    "GetRecordStatus",
    "StartRecord",
    "StopRecord",
    "ToggleRecord",
    "PauseRecord",
    "ResumeRecord",
    "ToggleRecordPause",
    "GetReplayBufferStatus",
    "StartReplayBuffer",
    "StopReplayBuffer",
    "ToggleReplayBuffer",
    "SaveReplayBuffer",
    "GetLastReplayBufferReplay",
    "GetVirtualCamStatus",
    "StartVirtualCam",
    "StopVirtualCam",
    "ToggleVirtualCam",
    "GetSourceFilterList",
    "SetSourceFilterEnabled",
];

impl Engine {
    // ---- settings ----

    async fn ows_saved(&self) -> Saved {
        self.db(|s| repo::settings::get::<Saved>(s, SAVE_KEY)).await.ok().flatten().unwrap_or_default()
    }

    async fn ows_save(&self, saved: Saved) -> Result<(), EngineError> {
        self.db(move |s| repo::settings::set(s, SAVE_KEY, &saved)).await?;
        Ok(())
    }

    /// What Settings shows.
    pub async fn obsws_view(&self) -> ObsWsView {
        let s = self.ows_saved().await;
        let (listening, error) = self.inner.obsws.status.lock().unwrap_or_else(|p| p.into_inner()).clone();
        ObsWsView {
            enabled: s.enabled,
            port: s.port,
            lan: s.lan,
            has_password: !s.secret.is_empty(),
            clients: self.inner.obsws.clients.load(Ordering::Relaxed),
            listening,
            error,
        }
    }

    /// Changes the settings. Switched on for the first time, a password is made and handed back
    /// once (it can't be read again: only its hash is kept).
    pub async fn obsws_set(&self, patch: ObsWsPatch) -> Result<(ObsWsView, Option<String>), EngineError> {
        let _one = self.inner.config.lock().await;
        let mut s = self.ows_saved().await;
        if let Some(port) = patch.port {
            if port < 1024 {
                return Err(EngineError::Invalid("port: use 1024 or higher.".into()));
            }
            s.port = port;
        }
        if let Some(lan) = patch.lan {
            s.lan = lan;
        }
        let mut made = None;
        if let Some(on) = patch.enabled {
            s.enabled = on;
            if on && s.secret.is_empty() {
                let (pw, salt, secret) = new_secret()?;
                (s.salt, s.secret) = (salt, secret);
                made = Some(pw);
            }
        }
        self.ows_save(s).await?;
        self.obsws_apply();
        self.obsws_settle().await;
        Ok((self.obsws_view().await, made))
    }

    /// A new password (the old one stops working and everyone connected is let go).
    pub async fn obsws_regenerate(&self) -> Result<String, EngineError> {
        let _one = self.inner.config.lock().await;
        let mut s = self.ows_saved().await;
        let (pw, salt, secret) = new_secret()?;
        (s.salt, s.secret) = (salt, secret);
        self.ows_save(s).await?;
        self.obsws_apply();
        self.obsws_settle().await;
        Ok(pw)
    }

    /// Starts what the saved settings ask for (the server calls this once it's up).
    pub fn start_obsws(&self) {
        self.obsws_apply();
        let mut slot = self.inner.obsws.poller.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_none_or(tokio::task::JoinHandle::is_finished) {
            let engine = self.clone();
            *slot = Some(tokio::spawn(async move { engine.ows_poll_loop().await }));
        }
    }

    /// Looks at the saved settings again: closes everyone, and listens afresh when on.
    fn obsws_apply(&self) {
        let rt = &self.inner.obsws;
        rt.generation.send_modify(|g| *g += 1);
        let mut slot = rt.task.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(old) = slot.take() {
            old.abort();
        }
        *rt.status.lock().unwrap_or_else(|p| p.into_inner()) = (false, None);
        let engine = self.clone();
        *slot = Some(tokio::spawn(async move { engine.ows_listen().await }));
    }

    /// Waits (a second at most) for the listener to bind or say why it couldn't.
    async fn obsws_settle(&self) {
        for _ in 0..50 {
            if self.inner.obsws.status.lock().unwrap_or_else(|p| p.into_inner()).clone() != (false, None) {
                return;
            }
            if !self.ows_saved().await.enabled {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn ows_listen(&self) {
        let saved = self.ows_saved().await;
        let set_status = |listening: bool, error: Option<String>| {
            *self.inner.obsws.status.lock().unwrap_or_else(|p| p.into_inner()) = (listening, error);
        };
        if !saved.enabled {
            return;
        }
        if saved.secret.is_empty() {
            set_status(false, Some("There is no password yet.".into()));
            return;
        }
        let ip = if saved.lan { IpAddr::V4(Ipv4Addr::UNSPECIFIED) } else { IpAddr::V4(Ipv4Addr::LOCALHOST) };
        let listener = match TcpListener::bind(SocketAddr::new(ip, saved.port)).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(port = saved.port, error = %e, "obs-websocket couldn't listen");
                set_status(false, Some(format!("Port {} can't be used: {e}.", saved.port)));
                return;
            }
        };
        tracing::info!(port = saved.port, lan = saved.lan, "obs-websocket is listening");
        set_status(true, None);
        let generation = *self.inner.obsws.generation.borrow();
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            // A device that failed the password too often waits.
            if self.ows_shut_out(peer.ip()) {
                continue;
            }
            let Ok(waiting) = Arc::clone(&self.inner.obsws.pending).try_acquire_owned() else {
                tracing::debug!(%peer, "obs-websocket: too many not logged in yet, one turned away");
                continue;
            };
            let Ok(slot) = Arc::clone(&self.inner.obsws.slots).try_acquire_owned() else {
                tracing::debug!(%peer, "obs-websocket: too many clients, one turned away");
                continue;
            };
            let engine = self.clone();
            let saved = saved.clone();
            tokio::spawn(async move {
                let _slot = slot;
                engine.ows_connection(stream, peer, saved, generation, waiting).await;
            });
        }
    }

    fn ows_shut_out(&self, ip: IpAddr) -> bool {
        // This computer is never shut out: any web page open here can reach 127.0.0.1, and
        // shutting it out would shut out the owner's own Stream Deck too. The 24-character
        // password and the delay after a wrong one are what stop guessing here.
        if ip.is_loopback() {
            return false;
        }
        let mut f = self.inner.obsws.fails.lock().unwrap_or_else(|p| p.into_inner());
        f.retain(|_, v| v.shut_until.is_some_and(|t| t > Instant::now()) || v.since.elapsed() < FAIL_WINDOW);
        f.get(&ip).is_some_and(|v| v.shut_until.is_some_and(|t| t > Instant::now()))
    }

    fn ows_note_fail(&self, ip: IpAddr) {
        if ip.is_loopback() {
            return;
        }
        let mut map = self.inner.obsws.fails.lock().unwrap_or_else(|p| p.into_inner());
        let f = map.entry(ip).or_insert(Fails { count: 0, since: Instant::now(), shut_until: None });
        if f.since.elapsed() > FAIL_WINDOW {
            *f = Fails { count: 0, since: Instant::now(), shut_until: None };
        }
        f.count += 1;
        if f.count >= MAX_FAILS {
            f.shut_until = Some(Instant::now() + SHUT_OUT);
            tracing::warn!(%ip, "obs-websocket: too many wrong passwords, this device waits a minute");
        }
    }

    fn ows_note_ok(&self, ip: IpAddr) {
        self.inner.obsws.fails.lock().unwrap_or_else(|p| p.into_inner()).remove(&ip);
    }

    // ---- one connection ----

    async fn ows_connection(
        &self,
        stream: TcpStream,
        peer: SocketAddr,
        saved: Saved,
        generation: u64,
        waiting: tokio::sync::OwnedSemaphorePermit,
    ) {
        let mut waiting = Some(waiting);
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE))
            .read_buffer_size(16 * 1024)
            .write_buffer_size(0);
        // The handshake callback's own signature carries the big error type.
        #[allow(clippy::result_large_err)]
        let pick = |req: &Request, mut resp: Response| -> Result<Response, ErrorResponse> {
            let asked: Vec<&str> = req
                .headers()
                .get_all("sec-websocket-protocol")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .map(str::trim)
                .collect();
            if asked.is_empty() || asked.contains(&SUBPROTOCOL) {
                if !asked.is_empty() {
                    resp.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static(SUBPROTOCOL));
                }
                return Ok(resp);
            }
            let mut no = ErrorResponse::new(Some("Only the obswebsocket.json subprotocol is spoken here.".into()));
            *no.status_mut() = StatusCode::BAD_REQUEST;
            Err(no)
        };
        let handshake = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::accept_hdr_async_with_config(stream, pick, Some(config)),
        )
        .await;
        let Ok(Ok(mut ws)) = handshake else { return };

        let Some(challenge) = random_bytes::<32>().map(|b| b64(&b)) else { return };
        let hello = json!({ "op": 0, "d": {
            "obsStudioVersion": env!("CARGO_PKG_VERSION"),
            "obsWebSocketVersion": WS_VERSION,
            "rpcVersion": RPC_VERSION,
            "authentication": { "challenge": challenge, "salt": saved.salt },
        }});
        if send(&mut ws, &hello).await.is_err() {
            return;
        }

        let mut gen_rx = self.inner.obsws.generation.subscribe();
        let mut events = self.inner.obsws.events.subscribe();
        let mut subs: Option<u64> = None;
        let deadline = tokio::time::sleep(IDENTIFY_WITHIN);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline, if subs.is_none() => {
                    close(&mut ws, CLOSE_NOT_IDENTIFIED, "Identify within 10 seconds.").await;
                    return;
                }
                changed = gen_rx.changed() => {
                    if changed.is_err() || *gen_rx.borrow() != generation {
                        close(&mut ws, CLOSE_INVALIDATED, "The settings changed.").await;
                        break;
                    }
                }
                ev = events.recv(), if subs.is_some() => match ev {
                    Ok((bit, text)) => {
                        if subs.is_some_and(|m| m & bit != 0)
                            && tokio::time::timeout(SEND_WITHIN, ws.send(Message::Text(text.as_ref().into())))
                                .await
                                .map_or(true, |r| r.is_err())
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                msg = ws.next() => {
                    let Some(Ok(msg)) = msg else { break };
                    let text = match msg {
                        Message::Text(t) => t,
                        Message::Close(_) => break,
                        Message::Ping(_) | Message::Pong(_) => continue,
                        _ => {
                            close(&mut ws, CLOSE_DECODE, "Only JSON text messages are taken.").await;
                            break;
                        }
                    };
                    let Ok(doc) = serde_json::from_str::<Value>(text.as_str()) else {
                        close(&mut ws, CLOSE_DECODE, "That wasn't JSON.").await;
                        break;
                    };
                    let (Some(op), Some(d)) = (doc.get("op").and_then(Value::as_u64), doc.get("d")) else {
                        close(&mut ws, CLOSE_MISSING, "A message needs `op` and `d`.").await;
                        break;
                    };
                    match op {
                        1 | 3 => {
                            if op == 1 && subs.is_some() {
                                close(&mut ws, CLOSE_ALREADY, "Already identified.").await;
                                break;
                            }
                            if op == 3 && subs.is_none() {
                                close(&mut ws, CLOSE_NOT_IDENTIFIED, "Identify first.").await;
                                break;
                            }
                            match self.ows_identify(op, d, &saved, &challenge, peer.ip()) {
                                Ok(mask) => {
                                    if subs.is_none() {
                                        // Logged in: no longer one of the few waiting places.
                                        waiting.take();
                                        self.inner.obsws.clients.fetch_add(1, Ordering::Relaxed);
                                        self.inner.obsws.poll_now.notify_one();
                                    }
                                    subs = Some(mask);
                                    // Identified is op 2; Reidentified is op 3.
                                    let reply = json!({ "op": if op == 1 { 2 } else { 3 },
                                        "d": { "negotiatedRpcVersion": RPC_VERSION } });
                                    if send(&mut ws, &reply).await.is_err() {
                                        break;
                                    }
                                }
                                Err((code, why)) => {
                                    if code == CLOSE_AUTH {
                                        tokio::time::sleep(FAIL_DELAY).await;
                                    }
                                    close(&mut ws, code, &why).await;
                                    break;
                                }
                            }
                        }
                        _ if subs.is_none() => {
                            close(&mut ws, CLOSE_NOT_IDENTIFIED, "Identify first.").await;
                            break;
                        }
                        6 => {
                            let reply = self.ows_one(d).await;
                            if send(&mut ws, &json!({ "op": 7, "d": reply })).await.is_err() {
                                break;
                            }
                        }
                        8 => {
                            let reply = self.ows_batch(d).await;
                            if send(&mut ws, &json!({ "op": 9, "d": reply })).await.is_err() {
                                break;
                            }
                        }
                        _ => {
                            close(&mut ws, CLOSE_BAD_OP, "Unknown op code.").await;
                            break;
                        }
                    }
                }
            }
        }
        if subs.is_some() {
            self.inner.obsws.clients.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Checks an Identify (or Reidentify) → its event subscriptions.
    fn ows_identify(&self, op: u64, d: &Value, saved: &Saved, challenge: &str, ip: IpAddr) -> Result<u64, Fail> {
        if op == 1 {
            match d.get("rpcVersion").and_then(Value::as_u64) {
                None => return Err(fail(CLOSE_MISSING, "Identify needs `rpcVersion`.")),
                Some(0) => return Err(fail(CLOSE_RPC, "This speaks RPC version 1.")),
                Some(_) => {}
            }
            let given = d.get("authentication").and_then(Value::as_str).unwrap_or_default();
            if !same(given, &answer(&saved.secret, challenge)) {
                self.ows_note_fail(ip);
                return Err(fail(CLOSE_AUTH, "Authentication failed."));
            }
            self.ows_note_ok(ip);
        }
        match d.get("eventSubscriptions") {
            None | Some(Value::Null) => Ok(SUB_ALL),
            Some(v) => {
                v.as_u64().map(|m| m & SUB_ALL).ok_or_else(|| fail(CLOSE_BAD_TYPE, "eventSubscriptions is a number."))
            }
        }
    }

    async fn ows_batch(&self, d: &Value) -> Value {
        let id = d.get("requestId").cloned().unwrap_or(Value::Null);
        let halt = d.get("haltOnFailure").and_then(Value::as_bool).unwrap_or(false);
        let mut results = Vec::new();
        for r in d.get("requests").and_then(Value::as_array).into_iter().flatten().take(MAX_BATCH) {
            let out = self.ows_one(r).await;
            let ok = out["requestStatus"]["result"] == Value::Bool(true);
            results.push(out);
            if halt && !ok {
                break;
            }
        }
        json!({ "requestId": id, "results": results })
    }

    /// One request → the RequestResponse's data.
    async fn ows_one(&self, d: &Value) -> Value {
        let id = d.get("requestId").cloned().unwrap_or(Value::Null);
        let Some(kind) = d.get("requestType").and_then(Value::as_str) else {
            return json!({ "requestType": "", "requestId": id,
                "requestStatus": { "result": false, "code": ST_MISSING_TYPE, "comment": "Your request is missing the `requestType`." } });
        };
        let data = d.get("requestData").cloned().unwrap_or(Value::Null);
        let data = if data.is_null() { json!({}) } else { data };
        let out = if data.is_object() {
            self.ows_request(kind, &data).await
        } else {
            Err(fail(ST_BAD_TYPE, "`requestData` must be an object."))
        };
        match out {
            Ok(body) => {
                let mut r = json!({ "requestType": kind, "requestId": id,
                    "requestStatus": { "result": true, "code": ST_OK } });
                if !body.is_null() {
                    r["responseData"] = body;
                }
                // A change made here shows in the events at once, not a quarter second on.
                self.inner.obsws.poll_now.notify_one();
                r
            }
            Err((code, comment)) => json!({ "requestType": kind, "requestId": id,
                "requestStatus": { "result": false, "code": code, "comment": comment } }),
        }
    }

    // ---- requests ----

    async fn ows_scenes(&self) -> Result<Value, Fail> {
        self.studio_act(StudioAct::Scenes).await.map_err(obs_error)
    }

    async fn ows_stats(&self) -> Result<Value, Fail> {
        self.studio_stats().await.map_err(obs_error)
    }

    /// A scene's name from `sceneName` or `sceneUuid`.
    async fn ows_scene_name(&self, d: &Value) -> Result<String, Fail> {
        let scenes = self.ows_scenes().await?;
        let names: Vec<&str> =
            scenes["scenes"].as_array().into_iter().flatten().filter_map(|s| s["name"].as_str()).collect();
        if let Some(n) = d.get("sceneName").and_then(Value::as_str) {
            return names
                .contains(&n)
                .then(|| n.to_string())
                .ok_or_else(|| fail(ST_NOT_FOUND, "No scene was found by that name."));
        }
        if let Some(u) = d.get("sceneUuid").and_then(Value::as_str) {
            return names
                .iter()
                .find(|n| uuid_of(n) == u)
                .map(|n| (*n).to_string())
                .ok_or_else(|| fail(ST_NOT_FOUND, "No scene was found by that uuid."));
        }
        Err(fail(ST_MISSING_FIELD, "Your request needs `sceneName` or `sceneUuid`."))
    }

    fn ows_input_name(d: &Value) -> Result<String, Fail> {
        if let Some(n) = d.get("inputName").and_then(Value::as_str) {
            return Ok(n.to_string());
        }
        Err(fail(ST_MISSING_FIELD, "Your request needs `inputName`."))
    }

    /// A sound's mixer row, wherever it is (the scene on air first, then the others).
    async fn ows_audio(&self, name: &str) -> Result<Value, Fail> {
        let scenes = self.ows_scenes().await?;
        let mut order: Vec<String> = vec![scenes["current"].as_str().unwrap_or_default().to_string()];
        order.extend(
            scenes["scenes"].as_array().into_iter().flatten().filter_map(|s| s["name"].as_str()).map(String::from),
        );
        let mut seen = std::collections::HashSet::new();
        for scene in order.into_iter().filter(|s| !s.is_empty()).take(64) {
            if !seen.insert(scene.clone()) {
                continue;
            }
            let list = self.studio_act(StudioAct::Audio { scene }).await.map_err(obs_error)?;
            if let Some(row) = list.as_array().and_then(|l| l.iter().find(|a| a["name"].as_str() == Some(name))) {
                return Ok(row.clone());
            }
        }
        Err(fail(ST_NOT_FOUND, "No input was found by that name."))
    }

    async fn ows_items(&self, scene: &str) -> Result<Vec<Value>, Fail> {
        let v = self.studio_act(StudioAct::Items { scene: scene.to_string() }).await.map_err(obs_error)?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    fn ows_item_id(d: &Value) -> Result<i64, Fail> {
        field(d, "sceneItemId")?.as_i64().ok_or_else(|| fail(ST_BAD_TYPE, "`sceneItemId` must be a number."))
    }

    async fn ows_find_item(&self, scene: &str, id: i64) -> Result<Value, Fail> {
        let items = self.ows_items(scene).await?;
        items
            .iter()
            .flat_map(|i| std::iter::once(i).chain(i["children"].as_array().into_iter().flatten()))
            .find(|i| i["id"].as_i64() == Some(id))
            .cloned()
            .ok_or_else(|| fail(ST_NOT_FOUND, "No scene item was found by that id."))
    }

    async fn ows_set_output(&self, which: &str, on: bool) -> Result<Value, Fail> {
        let stats = self.ows_stats().await?;
        let now = stats[which]["active"].as_bool().unwrap_or(false);
        if on == now {
            return Err(fail(
                if on { ST_RUNNING } else { ST_NOT_RUNNING },
                if on { "The output is already running." } else { "The output is not running." },
            ));
        }
        match which {
            "stream" => self.studio_stream(on).await.map(drop),
            "record" => self.studio_record(on).await.map(drop),
            "replay" => self.studio_replay(on).await.map(drop),
            _ => self.studio_virtual_camera(on, None).await.map(drop),
        }
        .map_err(obs_error)?;
        Ok(Value::Null)
    }

    async fn ows_toggle_output(&self, which: &str) -> Result<Value, Fail> {
        let stats = self.ows_stats().await?;
        let now = stats[which]["active"].as_bool().unwrap_or(false);
        self.ows_set_output(which, !now).await?;
        Ok(json!({ "outputActive": !now }))
    }

    async fn ows_request(&self, kind: &str, d: &Value) -> Result<Value, Fail> {
        match kind {
            "GetVersion" => Ok(json!({
                "obsVersion": env!("CARGO_PKG_VERSION"),
                "obsWebSocketVersion": WS_VERSION,
                "rpcVersion": RPC_VERSION,
                "availableRequests": REQUESTS,
                "supportedImageFormats": ["png"],
                "platform": std::env::consts::OS,
                "platformDescription": format!("Ferrocast on {}", std::env::consts::OS),
            })),
            "GetStats" => {
                let s = self.ows_stats().await?;
                let e = &s["engine"];
                Ok(json!({
                    "cpuUsage": e["cpu"].as_f64().unwrap_or(0.0),
                    "memoryUsage": e["memory_mb"].as_f64().unwrap_or(0.0),
                    "availableDiskSpace": s["disk_free_mb"].as_f64().unwrap_or(0.0),
                    "activeFps": e["fps"].as_f64().unwrap_or(0.0),
                    "averageFrameRenderTime": e["frame_ms"].as_f64().unwrap_or(0.0),
                    "renderSkippedFrames": e["lagged"].as_u64().unwrap_or(0),
                    "renderTotalFrames": e["frames"].as_u64().unwrap_or(0),
                    "outputSkippedFrames": s["stream"]["dropped"].as_u64().unwrap_or(0),
                    "outputTotalFrames": s["stream"]["frames"].as_u64().unwrap_or(0),
                    "webSocketSessionIncomingMessages": 0,
                    "webSocketSessionOutgoingMessages": 0,
                }))
            }
            // ---- scenes ----
            "GetSceneList" => {
                let s = self.ows_scenes().await?;
                let names: Vec<&str> =
                    s["scenes"].as_array().into_iter().flatten().filter_map(|x| x["name"].as_str()).collect();
                // obs-websocket lists the last scene first, each with its place in the list.
                let list: Vec<Value> = names
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(i, n)| json!({ "sceneIndex": i, "sceneName": n, "sceneUuid": uuid_of(n) }))
                    .collect();
                let cur = s["current"].as_str().unwrap_or_default();
                let mut out = json!({
                    "currentProgramSceneName": cur, "currentProgramSceneUuid": uuid_of(cur), "scenes": list,
                    "currentPreviewSceneName": null, "currentPreviewSceneUuid": null,
                });
                if let Some(p) = s["preview"].as_str() {
                    out["currentPreviewSceneName"] = json!(p);
                    out["currentPreviewSceneUuid"] = json!(uuid_of(p));
                }
                Ok(out)
            }
            "GetCurrentProgramScene" => {
                let s = self.ows_scenes().await?;
                let cur = s["current"].as_str().unwrap_or_default();
                Ok(json!({ "sceneName": cur, "sceneUuid": uuid_of(cur),
                    "currentProgramSceneName": cur, "currentProgramSceneUuid": uuid_of(cur) }))
            }
            "SetCurrentProgramScene" => {
                let scene = self.ows_scene_name(d).await?;
                self.studio_act(StudioAct::SetCurrent { scene }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "GetStudioModeEnabled" => {
                let s = self.ows_scenes().await?;
                Ok(json!({ "studioModeEnabled": !s["preview"].is_null() }))
            }
            "SetStudioModeEnabled" => {
                let on = bool_field(d, "studioModeEnabled")?;
                self.studio_act(StudioAct::StudioMode { on }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "GetCurrentPreviewScene" => {
                let s = self.ows_scenes().await?;
                let Some(p) = s["preview"].as_str() else {
                    return Err(fail(ST_STUDIO_NOT_ACTIVE, "Studio mode is not active."));
                };
                Ok(json!({ "sceneName": p, "sceneUuid": uuid_of(p),
                    "currentPreviewSceneName": p, "currentPreviewSceneUuid": uuid_of(p) }))
            }
            "SetCurrentPreviewScene" => {
                if self.ows_scenes().await?["preview"].is_null() {
                    return Err(fail(ST_STUDIO_NOT_ACTIVE, "Studio mode is not active."));
                }
                let scene = self.ows_scene_name(d).await?;
                self.studio_act(StudioAct::SetPreview { scene }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "TriggerStudioModeTransition" => {
                if self.ows_scenes().await?["preview"].is_null() {
                    return Err(fail(ST_STUDIO_NOT_ACTIVE, "Studio mode is not active."));
                }
                self.studio_act(StudioAct::TransitionNow { quick: None }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            // ---- inputs ----
            "GetInputList" => {
                let v = self.studio_act(StudioAct::Sources).await.map_err(obs_error)?;
                let want = d.get("inputKind").and_then(Value::as_str);
                let list: Vec<Value> = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|s| want.is_none_or(|k| s["kind"].as_str() == Some(k)))
                    .filter_map(|s| {
                        let n = s["name"].as_str()?;
                        Some(json!({ "inputName": n, "inputUuid": uuid_of(n),
                            "inputKind": s["kind"], "unversionedInputKind": s["kind"] }))
                    })
                    .collect();
                Ok(json!({ "inputs": list }))
            }
            "GetInputMute" => {
                let row = self.ows_audio(&Self::ows_input_name(d)?).await?;
                Ok(json!({ "inputMuted": row["muted"].as_bool().unwrap_or(false) }))
            }
            "SetInputMute" | "ToggleInputMute" => {
                let name = Self::ows_input_name(d)?;
                let row = self.ows_audio(&name).await?;
                let now = row["muted"].as_bool().unwrap_or(false);
                let muted = if kind == "SetInputMute" { bool_field(d, "inputMuted")? } else { !now };
                let change = AudioChange { muted: Some(muted), ..Default::default() };
                self.studio_act(StudioAct::SetAudio { name, change }).await.map_err(obs_error)?;
                Ok(if kind == "ToggleInputMute" { json!({ "inputMuted": muted }) } else { Value::Null })
            }
            "GetInputVolume" => {
                let row = self.ows_audio(&Self::ows_input_name(d)?).await?;
                let db = row["volume_db"].as_f64().unwrap_or(0.0);
                Ok(json!({ "inputVolumeMul": mul_of(db), "inputVolumeDb": db }))
            }
            "SetInputVolume" => {
                let name = Self::ows_input_name(d)?;
                let db = if d.get("inputVolumeDb").is_some_and(|v| !v.is_null()) {
                    let db = num_field(d, "inputVolumeDb")?;
                    if !(-100.0..=26.0).contains(&db) {
                        return Err(fail(ST_OUT_OF_RANGE, "`inputVolumeDb` must be from -100 to 26."));
                    }
                    db
                } else {
                    let mul = num_field(d, "inputVolumeMul")?;
                    if !(0.0..=20.0).contains(&mul) {
                        return Err(fail(ST_OUT_OF_RANGE, "`inputVolumeMul` must be from 0 to 20."));
                    }
                    db_of(mul)
                };
                self.ows_audio(&name).await?;
                let change = AudioChange { volume_db: Some(db as f32), ..Default::default() };
                self.studio_act(StudioAct::SetAudio { name, change }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            // ---- scene items ----
            "GetSceneItemList" => {
                let scene = self.ows_scene_name(d).await?;
                let items = self.ows_items(&scene).await?;
                let n = items.len();
                let list: Vec<Value> = items
                    .iter()
                    .enumerate()
                    .filter_map(|(pos, i)| {
                        let name = i["name"].as_str()?;
                        Some(json!({
                            "sceneItemId": i["id"], "sceneItemIndex": n - 1 - pos,
                            "sourceName": name, "sourceUuid": uuid_of(name),
                            "sourceType": if i["group"].as_bool() == Some(true) { "OBS_SOURCE_TYPE_SCENE" } else { "OBS_SOURCE_TYPE_INPUT" },
                            "inputKind": i["kind"], "isGroup": i["group"].as_bool().unwrap_or(false),
                            "sceneItemLocked": i["locked"].as_bool().unwrap_or(false),
                            "sceneItemEnabled": i["visible"].as_bool().unwrap_or(true),
                        }))
                    })
                    .collect();
                Ok(json!({ "sceneItems": list }))
            }
            "GetSceneItemId" => {
                let scene = self.ows_scene_name(d).await?;
                let source = str_field(d, "sourceName")?;
                let items = self.ows_items(&scene).await?;
                items
                    .iter()
                    .flat_map(|i| std::iter::once(i).chain(i["children"].as_array().into_iter().flatten()))
                    .find(|i| i["name"].as_str() == Some(source))
                    .map(|i| json!({ "sceneItemId": i["id"] }))
                    .ok_or_else(|| fail(ST_NOT_FOUND, "No scene items were found in the specified scene by that name."))
            }
            "GetSceneItemEnabled" => {
                let scene = self.ows_scene_name(d).await?;
                let item = self.ows_find_item(&scene, Self::ows_item_id(d)?).await?;
                Ok(json!({ "sceneItemEnabled": item["visible"].as_bool().unwrap_or(true) }))
            }
            "SetSceneItemEnabled" => {
                let scene = self.ows_scene_name(d).await?;
                let id = Self::ows_item_id(d)?;
                let on = bool_field(d, "sceneItemEnabled")?;
                self.ows_find_item(&scene, id).await?;
                let edit = if on { ItemEdit::Show } else { ItemEdit::Hide };
                self.studio_act(StudioAct::EditItem { scene, id, edit }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            // ---- stream ----
            "GetStreamStatus" => {
                let s = self.ows_stats().await?;
                let st = &s["stream"];
                Ok(json!({
                    "outputActive": st["active"].as_bool().unwrap_or(false),
                    "outputReconnecting": st["reconnecting"].as_bool().unwrap_or(false),
                    "outputTimecode": timecode(0.0), "outputDuration": 0, "outputCongestion": 0.0,
                    "outputBytes": st["bytes"].as_u64().unwrap_or(0),
                    "outputSkippedFrames": st["dropped"].as_u64().unwrap_or(0),
                    "outputTotalFrames": st["frames"].as_u64().unwrap_or(0),
                }))
            }
            "StartStream" => self.ows_set_output("stream", true).await,
            "StopStream" => self.ows_set_output("stream", false).await,
            "ToggleStream" => self.ows_toggle_output("stream").await,
            // ---- record ----
            "GetRecordStatus" => {
                let s = self.ows_stats().await?;
                let r = &s["record"];
                let secs = r["seconds"].as_f64().unwrap_or(0.0);
                Ok(json!({
                    "outputActive": r["active"].as_bool().unwrap_or(false),
                    "outputPaused": r["paused"].as_bool().unwrap_or(false),
                    "outputTimecode": timecode(secs), "outputDuration": (secs * 1000.0) as u64,
                    "outputBytes": r["bytes"].as_u64().unwrap_or(0),
                }))
            }
            "StartRecord" => self.ows_set_output("record", true).await,
            "StopRecord" => {
                let file = self.ows_stats().await?["record"]["file"].as_str().unwrap_or_default().to_string();
                self.ows_set_output("record", false).await?;
                Ok(json!({ "outputPath": file }))
            }
            "ToggleRecord" => self.ows_toggle_output("record").await,
            "PauseRecord" | "ResumeRecord" | "ToggleRecordPause" => {
                let s = self.ows_stats().await?;
                if s["record"]["active"].as_bool() != Some(true) {
                    return Err(fail(ST_NOT_RUNNING, "The record output is not running."));
                }
                let paused = s["record"]["paused"].as_bool().unwrap_or(false);
                let want = match kind {
                    "PauseRecord" => true,
                    "ResumeRecord" => false,
                    _ => !paused,
                };
                if kind == "PauseRecord" && paused {
                    return Err(fail(ST_PAUSED, "The record output is already paused."));
                }
                if kind == "ResumeRecord" && !paused {
                    return Err(fail(ST_NOT_PAUSED, "The record output is not paused."));
                }
                self.studio_record_pause(want).await.map_err(obs_error)?;
                Ok(if kind == "ToggleRecordPause" { json!({ "outputPaused": want }) } else { Value::Null })
            }
            // ---- replay buffer ----
            "GetReplayBufferStatus" => {
                let s = self.ows_stats().await?;
                Ok(json!({ "outputActive": s["replay"]["active"].as_bool().unwrap_or(false) }))
            }
            "StartReplayBuffer" => self.ows_set_output("replay", true).await,
            "StopReplayBuffer" => self.ows_set_output("replay", false).await,
            "ToggleReplayBuffer" => self.ows_toggle_output("replay").await,
            "SaveReplayBuffer" => {
                if self.ows_stats().await?["replay"]["active"].as_bool() != Some(true) {
                    return Err(fail(ST_NOT_RUNNING, "The replay buffer is not running."));
                }
                self.studio_replay_save().await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "GetLastReplayBufferReplay" => {
                let s = self.ows_stats().await?;
                match s["replay"]["last"].as_str() {
                    Some(f) => Ok(json!({ "savedReplayPath": f })),
                    None => Err(fail(ST_NOT_FOUND, "No replay has been saved yet.")),
                }
            }
            // ---- virtual camera ----
            "GetVirtualCamStatus" => {
                let s = self.ows_stats().await?;
                Ok(json!({ "outputActive": s["virtualcam"]["active"].as_bool().unwrap_or(false) }))
            }
            "StartVirtualCam" => self.ows_set_output("virtualcam", true).await,
            "StopVirtualCam" => self.ows_set_output("virtualcam", false).await,
            "ToggleVirtualCam" => self.ows_toggle_output("virtualcam").await,
            // ---- filters ----
            "GetSourceFilterList" => {
                let name = str_field(d, "sourceName").or_else(|_| str_field(d, "inputName"))?.to_string();
                let v = self.studio_act(StudioAct::Filters { name, item: None }).await.map_err(obs_error)?;
                let list: Vec<Value> = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .map(|(i, f)| {
                        json!({ "filterEnabled": f["enabled"].as_bool().unwrap_or(false), "filterIndex": i,
                        "filterKind": f["kind"], "filterName": f["name"], "filterSettings": {} })
                    })
                    .collect();
                Ok(json!({ "filters": list }))
            }
            "SetSourceFilterEnabled" => {
                let name = str_field(d, "sourceName")?.to_string();
                let filter = str_field(d, "filterName")?.to_string();
                let on = bool_field(d, "filterEnabled")?;
                let list =
                    self.studio_act(StudioAct::Filters { name: name.clone(), item: None }).await.map_err(obs_error)?;
                if !list.as_array().is_some_and(|l| l.iter().any(|f| f["name"].as_str() == Some(filter.as_str()))) {
                    return Err(fail(ST_NOT_FOUND, "No filter was found by that name."));
                }
                let edit = if on { FilterEdit::Enable } else { FilterEdit::Disable };
                self.studio_act(StudioAct::EditFilter { name, filter, edit, item: None }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            _ => Err(fail(ST_UNKNOWN, "Unknown request type.")),
        }
    }

    // ---- events ----

    /// While a client is connected: looks at the Studio a few times a second and says what
    /// changed.
    async fn ows_poll_loop(&self) {
        let mut before: Option<Snap> = None;
        loop {
            if self.inner.obsws.clients.load(Ordering::Relaxed) == 0 {
                before = None;
                self.inner.obsws.poll_now.notified().await;
                continue;
            }
            let now = self.ows_snap().await;
            if let (Some(was), Some(is)) = (&before, &now) {
                for (bit, text) in diff(was, is) {
                    let _ = self.inner.obsws.events.send((bit, text.into()));
                }
            }
            if now.is_some() {
                before = now;
            }
            let _ = tokio::time::timeout(POLL, self.inner.obsws.poll_now.notified()).await;
            // A quick follow-up after a request still waits a moment, so bursts are one look.
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    async fn ows_snap(&self) -> Option<Snap> {
        let scenes = self.studio_act(StudioAct::Scenes).await.ok()?;
        let stats = self.studio_stats().await.ok()?;
        let program = scenes["current"].as_str().unwrap_or_default().to_string();
        let mut snap = Snap {
            program: program.clone(),
            preview: scenes["preview"].as_str().map(String::from),
            studio_mode: !scenes["preview"].is_null(),
            stream: stats["stream"]["active"].as_bool().unwrap_or(false),
            record: stats["record"]["active"].as_bool().unwrap_or(false),
            record_file: stats["record"]["file"].as_str().unwrap_or_default().to_string(),
            replay: stats["replay"]["active"].as_bool().unwrap_or(false),
            replay_last: stats["replay"]["last"].as_str().map(String::from),
            vcam: stats["virtualcam"]["active"].as_bool().unwrap_or(false),
            ..Default::default()
        };
        if !program.is_empty() {
            if let Ok(list) = self.studio_act(StudioAct::Audio { scene: program.clone() }).await {
                for a in list.as_array().into_iter().flatten() {
                    if let Some(n) = a["name"].as_str() {
                        snap.inputs.insert(
                            n.to_string(),
                            (a["muted"].as_bool().unwrap_or(false), a["volume_db"].as_f64().unwrap_or(0.0)),
                        );
                    }
                }
            }
            if let Ok(items) = self.ows_items(&program).await {
                for i in
                    items.iter().flat_map(|i| std::iter::once(i).chain(i["children"].as_array().into_iter().flatten()))
                {
                    if let Some(id) = i["id"].as_i64() {
                        snap.items.insert(id, i["visible"].as_bool().unwrap_or(true));
                    }
                }
            }
        }
        Some(snap)
    }
}

/// What the Studio looked like at one look.
#[derive(Debug, Default, Clone, PartialEq)]
struct Snap {
    program: String,
    preview: Option<String>,
    studio_mode: bool,
    stream: bool,
    record: bool,
    record_file: String,
    replay: bool,
    replay_last: Option<String>,
    vcam: bool,
    /// name → (muted, dB), for the sounds in the scene on air.
    inputs: BTreeMap<String, (bool, f64)>,
    /// item id → shown, in the scene on air.
    items: BTreeMap<i64, bool>,
}

fn event(bit: u64, kind: &str, data: Value) -> (u64, String) {
    (bit, json!({ "op": 5, "d": { "eventType": kind, "eventIntent": bit, "eventData": data } }).to_string())
}

fn output_event(kind: &str, active: bool, mut extra: Value) -> (u64, String) {
    let state = match (kind, active) {
        ("StreamStateChanged", true) => "OBS_WEBSOCKET_OUTPUT_STARTED",
        ("RecordStateChanged", true) => "OBS_WEBSOCKET_OUTPUT_STARTED",
        (_, true) => "OBS_WEBSOCKET_OUTPUT_STARTED",
        (_, false) => "OBS_WEBSOCKET_OUTPUT_STOPPED",
    };
    extra["outputActive"] = json!(active);
    extra["outputState"] = json!(state);
    event(SUB_OUTPUTS, kind, extra)
}

/// The events for what changed between two looks.
fn diff(was: &Snap, is: &Snap) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    if was.program != is.program {
        out.push(event(
            SUB_SCENES,
            "CurrentProgramSceneChanged",
            json!({ "sceneName": is.program, "sceneUuid": uuid_of(&is.program) }),
        ));
    }
    if was.studio_mode != is.studio_mode {
        out.push(event(SUB_UI, "StudioModeStateChanged", json!({ "studioModeEnabled": is.studio_mode })));
    }
    if was.preview != is.preview
        && let Some(p) = &is.preview
    {
        out.push(event(SUB_SCENES, "CurrentPreviewSceneChanged", json!({ "sceneName": p, "sceneUuid": uuid_of(p) })));
    }
    for (name, (muted, db)) in &is.inputs {
        let Some((was_muted, was_db)) = was.inputs.get(name) else { continue };
        if muted != was_muted {
            out.push(event(
                SUB_INPUTS,
                "InputMuteStateChanged",
                json!({ "inputName": name, "inputUuid": uuid_of(name), "inputMuted": muted }),
            ));
        }
        if (db - was_db).abs() > 0.005 {
            out.push(event(
                SUB_INPUTS,
                "InputVolumeChanged",
                json!({ "inputName": name, "inputUuid": uuid_of(name), "inputVolumeMul": mul_of(*db), "inputVolumeDb": db }),
            ));
        }
    }
    if was.program == is.program {
        for (id, on) in &is.items {
            if was.items.get(id).is_some_and(|w| w != on) {
                out.push(event(
                    SUB_SCENE_ITEMS,
                    "SceneItemEnableStateChanged",
                    json!({ "sceneName": is.program, "sceneUuid": uuid_of(&is.program), "sceneItemId": id, "sceneItemEnabled": on }),
                ));
            }
        }
    }
    if was.stream != is.stream {
        out.push(output_event("StreamStateChanged", is.stream, json!({})));
    }
    if was.record != is.record {
        let path = if is.record { &is.record_file } else { &was.record_file };
        out.push(output_event(
            "RecordStateChanged",
            is.record,
            json!({ "outputPath": if is.record { Value::Null } else { json!(path) } }),
        ));
    }
    if was.replay != is.replay {
        out.push(output_event("ReplayBufferStateChanged", is.replay, json!({})));
    }
    if was.replay_last != is.replay_last
        && let Some(f) = &is.replay_last
    {
        out.push(event(SUB_OUTPUTS, "ReplayBufferSaved", json!({ "savedReplayPath": f })));
    }
    if was.vcam != is.vcam {
        out.push(output_event("VirtualcamStateChanged", is.vcam, json!({})));
    }
    out
}

type Ws = tokio_tungstenite::WebSocketStream<TcpStream>;

async fn send(ws: &mut Ws, v: &Value) -> Result<(), ()> {
    match tokio::time::timeout(SEND_WITHIN, ws.send(Message::Text(v.to_string().into()))).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

async fn close(ws: &mut Ws, code: u16, why: &str) {
    let frame = CloseFrame { code: CloseCode::from(code), reason: why.to_string().into() };
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.close(Some(frame))).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_is_long_random_and_only_its_hash_is_kept() {
        let (a, b) = (new_password().unwrap(), new_password().unwrap());
        assert_eq!(a.len(), 24);
        assert_ne!(a, b);
        let (salt, secret) = derive(&a).unwrap();
        assert!(!secret.contains(&a) && !salt.contains(&a));
        // A client that knows the password gets the same answer as the server's.
        assert_eq!(client_auth(&a, &salt, "chal"), answer(&secret, "chal"));
        assert_ne!(client_auth("wrong", &salt, "chal"), answer(&secret, "chal"));
    }

    #[test]
    fn comparing_takes_every_byte_and_the_length() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
        assert!(!same("", "a"));
    }

    #[test]
    fn volumes_go_both_ways() {
        assert!((db_of(1.0)).abs() < 1e-9);
        assert!((db_of(0.5) + 6.0206).abs() < 1e-3);
        assert_eq!(db_of(0.0), -100.0);
        assert!((mul_of(-6.0206) - 0.5).abs() < 1e-3);
        assert_eq!(mul_of(-100.0), 0.0);
    }

    #[test]
    fn a_name_has_one_stable_uuid() {
        let u = uuid_of("Gameplay");
        assert_eq!(u, uuid_of("Gameplay"));
        assert_ne!(u, uuid_of("BRB"));
        assert_eq!(u.len(), 36);
        assert_eq!(u.matches('-').count(), 4);
    }

    #[test]
    fn what_changed_becomes_the_matching_events() {
        let a = Snap {
            program: "One".into(),
            inputs: BTreeMap::from([("Mic".into(), (false, 0.0))]),
            items: BTreeMap::from([(3, true)]),
            ..Default::default()
        };
        let mut b = a.clone();
        b.program = "Two".into();
        b.stream = true;
        b.inputs.insert("Mic".into(), (true, -6.0));
        let names: Vec<String> = diff(&a, &b)
            .iter()
            .map(|(_, t)| serde_json::from_str::<Value>(t).unwrap()["d"]["eventType"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            ["CurrentProgramSceneChanged", "InputMuteStateChanged", "InputVolumeChanged", "StreamStateChanged"]
        );
        // An item changing in the same scene is told; a scene change doesn't tell every item.
        let mut c = a.clone();
        c.items.insert(3, false);
        assert_eq!(diff(&a, &c).len(), 1);
        let mut d = a.clone();
        d.program = "Two".into();
        d.items = BTreeMap::from([(3, false)]);
        assert_eq!(diff(&a, &d).len(), 1);
        assert!(diff(&a, &a).is_empty());
    }

    // ---- a scripted Studio and a real client ----

    use std::sync::Mutex;

    use crate::studio_host::{PreviewRect, StudioHost, StudioSettings, StudioStatus};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    #[derive(Debug)]
    struct Studio {
        current: String,
        preview: Option<String>,
        mic: (bool, f64),
        cam_visible: bool,
        filter_on: bool,
        stream: bool,
        record: bool,
        paused: bool,
        replay: bool,
        replay_last: Option<String>,
        vcam: bool,
    }

    #[derive(Debug)]
    struct Fake(Mutex<Studio>);

    impl Fake {
        fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(Studio {
                current: "Game".into(),
                preview: None,
                mic: (false, 0.0),
                cam_visible: true,
                filter_on: true,
                stream: false,
                record: false,
                paused: false,
                replay: false,
                replay_last: None,
                vcam: false,
            })))
        }
        fn with<R>(&self, f: impl FnOnce(&mut Studio) -> R) -> R {
            f(&mut self.0.lock().unwrap_or_else(|p| p.into_inner()))
        }
    }

    #[async_trait::async_trait]
    impl StudioHost for Fake {
        async fn status(&self) -> StudioStatus {
            StudioStatus { available: true, ..Default::default() }
        }
        async fn start(&self) -> Result<StudioStatus, String> {
            Ok(StudioStatus { available: true, running: true, ..Default::default() })
        }
        async fn stop(&self) -> Result<StudioStatus, String> {
            Ok(StudioStatus::default())
        }
        async fn kinds(&self) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn options(&self, _k: String) -> Result<Value, String> {
            Ok(json!([]))
        }
        async fn preview(&self, _r: Option<PreviewRect>) -> Result<(), String> {
            Ok(())
        }
        async fn defaults(&self, _k: String) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn settings(&self) -> StudioSettings {
            StudioSettings::default()
        }
        async fn set_settings(&self, _s: StudioSettings) -> Result<Option<String>, String> {
            Ok(None)
        }
        async fn monitoring(&self) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn stream_start(&self, _s: String, _k: String, _t: Vec<(u32, u32, u32)>) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn stream_stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn stream_stats(&self) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn obs_collections(&self) -> Result<Value, String> {
            Ok(json!([]))
        }
        async fn import_obs(&self, _i: String) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn stats(&self) -> Result<Value, String> {
            Ok(self.with(|s| {
                json!({
                    "engine": { "cpu": 3.5, "fps": 30.0, "frame_ms": 4.0, "lagged": 0, "frames": 100, "memory_mb": 200.0 },
                    "stream": { "active": s.stream, "frames": 5, "bytes": 9, "dropped": 0 },
                    "record": { "active": s.record, "paused": s.paused, "seconds": 61.5, "bytes": 1, "file": "/r/a.mkv" },
                    "replay": { "active": s.replay, "last": s.replay_last },
                    "virtualcam": { "active": s.vcam },
                })
            }))
        }
        async fn record(&self, on: bool) -> Result<Value, String> {
            self.with(|s| {
                s.record = on;
                s.paused = false;
            });
            Ok(json!({}))
        }
        async fn record_pause(&self, on: bool) -> Result<(), String> {
            self.with(|s| s.paused = on);
            Ok(())
        }
        async fn replay(&self, on: bool) -> Result<Value, String> {
            self.with(|s| s.replay = on);
            Ok(json!({}))
        }
        async fn replay_save(&self) -> Result<Value, String> {
            self.with(|s| s.replay_last = Some("/r/replay.mp4".into()));
            Ok(json!({}))
        }
        async fn virtual_camera(&self, on: bool, _s: Option<String>) -> Result<Value, String> {
            self.with(|s| s.vcam = on);
            Ok(json!({}))
        }
        async fn act(&self, act: StudioAct) -> Result<Value, String> {
            Ok(self.with(|s| match act {
                StudioAct::Scenes => json!({
                    "scenes": [{ "name": "Game" }, { "name": "BRB" }], "current": s.current,
                    "preview": s.preview,
                }),
                StudioAct::SetCurrent { scene } => {
                    s.current = scene;
                    Value::Null
                }
                StudioAct::StudioMode { on } => {
                    s.preview = on.then(|| s.current.clone());
                    Value::Null
                }
                StudioAct::SetPreview { scene } => {
                    s.preview = Some(scene);
                    Value::Null
                }
                StudioAct::TransitionNow { .. } => {
                    if let Some(p) = s.preview.clone() {
                        s.current = p;
                    }
                    Value::Null
                }
                StudioAct::Items { .. } => json!([
                    { "id": 7, "name": "Cam", "kind": "av_capture_input", "visible": s.cam_visible, "locked": false, "group": false, "children": [] },
                    { "id": 8, "name": "Logo", "kind": "image_source", "visible": true, "locked": false, "group": false, "children": [] },
                ]),
                StudioAct::EditItem { id: 7, edit, .. } => {
                    s.cam_visible = matches!(edit, ItemEdit::Show);
                    Value::Null
                }
                StudioAct::Audio { .. } => json!([{ "name": "Mic", "muted": s.mic.0, "volume_db": s.mic.1 }]),
                StudioAct::SetAudio { change, .. } => {
                    if let Some(m) = change.muted {
                        s.mic.0 = m;
                    }
                    if let Some(v) = change.volume_db {
                        s.mic.1 = f64::from(v);
                    }
                    Value::Null
                }
                StudioAct::Sources => json!([{ "name": "Mic", "kind": "coreaudio_input_capture", "scenes": [] }]),
                StudioAct::Filters { .. } => json!([{ "name": "Blur", "kind": "blur", "enabled": s.filter_on }]),
                StudioAct::EditFilter { edit, .. } => {
                    s.filter_on = matches!(edit, FilterEdit::Enable);
                    Value::Null
                }
                _ => Value::Null,
            }))
        }
    }

    struct Client {
        ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        events: Vec<Value>,
        next: u32,
    }

    type Msg = Result<Message, tokio_tungstenite::tungstenite::Error>;

    impl Client {
        async fn open(port: u16) -> Result<Client, tokio_tungstenite::tungstenite::Error> {
            let mut req = format!("ws://127.0.0.1:{port}").into_client_request()?;
            req.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static(SUBPROTOCOL));
            let (ws, resp) = tokio_tungstenite::connect_async(req).await?;
            assert_eq!(
                resp.headers().get("sec-websocket-protocol").map(HeaderValue::as_bytes),
                Some(SUBPROTOCOL.as_bytes())
            );
            Ok(Client { ws, events: Vec::new(), next: 0 })
        }

        async fn frame(&mut self) -> Option<Msg> {
            tokio::time::timeout(Duration::from_secs(5), self.ws.next()).await.ok().flatten()
        }

        async fn json(&mut self) -> Value {
            match self.frame().await {
                Some(Ok(Message::Text(t))) => serde_json::from_str(t.as_str()).unwrap(),
                other => panic!("wanted a message, got {other:?}"),
            }
        }

        /// Hello → Identify → Identified.
        async fn identify(&mut self, password: &str, subs: Option<u64>) -> Value {
            let hello = self.json().await;
            assert_eq!(hello["op"], 0);
            let d = &hello["d"];
            assert_eq!(d["rpcVersion"], 1);
            let auth = client_auth(
                password,
                d["authentication"]["salt"].as_str().unwrap(),
                d["authentication"]["challenge"].as_str().unwrap(),
            );
            let mut id = json!({ "rpcVersion": 1, "authentication": auth });
            if let Some(s) = subs {
                id["eventSubscriptions"] = json!(s);
            }
            self.send_raw(json!({ "op": 1, "d": id })).await;
            self.json().await
        }

        async fn send_raw(&mut self, v: Value) {
            self.ws.send(Message::Text(v.to_string().into())).await.unwrap();
        }

        /// A request → its status and data, keeping the events that came meanwhile.
        async fn req(&mut self, kind: &str, data: Value) -> Value {
            self.next += 1;
            let id = format!("r{}", self.next);
            self.send_raw(json!({ "op": 6, "d": { "requestType": kind, "requestId": id, "requestData": data } })).await;
            loop {
                let m = self.json().await;
                match m["op"].as_u64() {
                    Some(7) if m["d"]["requestId"] == id => return m["d"].clone(),
                    Some(5) => self.events.push(m["d"].clone()),
                    _ => panic!("unexpected {m}"),
                }
            }
        }

        /// The next event of this type (events seen already count).
        async fn event(&mut self, kind: &str) -> Value {
            loop {
                if let Some(i) = self.events.iter().position(|e| e["eventType"] == kind) {
                    return self.events.remove(i)["eventData"].clone();
                }
                let m = self.json().await;
                if m["op"] == 5 {
                    self.events.push(m["d"].clone());
                }
            }
        }

        async fn closed_with(&mut self) -> Option<u16> {
            loop {
                match self.frame().await {
                    Some(Ok(Message::Close(Some(f)))) => return Some(u16::from(f.code)),
                    Some(Ok(Message::Close(None))) | None | Some(Err(_)) => return None,
                    Some(Ok(_)) => {}
                }
            }
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    async fn on() -> (Engine, Arc<Fake>, u16, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
        let fake = Fake::new();
        e.set_studio_host(fake.clone());
        e.start_obsws();
        let port = free_port();
        let (view, pw) = e.obsws_set(ObsWsPatch { enabled: Some(true), port: Some(port), lan: None }).await.unwrap();
        assert!(view.listening, "{view:?}");
        (e, fake, port, pw.expect("the first switch-on makes a password"), dir)
    }

    #[tokio::test]
    async fn it_is_off_until_switched_on_and_listens_on_this_computer_only() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
        e.start_obsws();
        let v = e.obsws_view().await;
        assert!(!v.enabled && !v.listening && !v.has_password && !v.lan);
        assert_eq!(v.port, DEFAULT_PORT);
        let port = free_port();
        // Switching on makes the password once; later changes don't show one.
        let (v, pw) = e.obsws_set(ObsWsPatch { enabled: Some(true), port: Some(port), lan: None }).await.unwrap();
        assert!(v.listening && v.has_password && !v.lan);
        let pw = pw.unwrap();
        assert_eq!(pw.len(), 24);
        assert!(Client::open(port).await.is_ok());
        let (_, again) = e.obsws_set(ObsWsPatch { lan: Some(false), ..Default::default() }).await.unwrap();
        assert!(again.is_none());
        // Only the hash is on disk.
        let saved = e.ows_saved().await;
        assert!(!saved.secret.contains(&pw) && !saved.salt.contains(&pw));
        // Low ports are refused; off closes the door.
        assert!(e.obsws_set(ObsWsPatch { port: Some(80), ..Default::default() }).await.is_err());
        let (v, _) = e.obsws_set(ObsWsPatch { enabled: Some(false), ..Default::default() }).await.unwrap();
        assert!(!v.listening);
        assert!(Client::open(port).await.is_err());
    }

    #[tokio::test]
    async fn a_client_identifies_and_drives_the_studio() {
        let (e, fake, port, pw, _dir) = on().await;
        let mut c = Client::open(port).await.unwrap();
        let identified = c.identify(&pw, None).await;
        assert_eq!(identified["op"], 2);
        assert_eq!(identified["d"]["negotiatedRpcVersion"], 1);
        assert_eq!(e.obsws_view().await.clients, 1);

        let v = c.req("GetVersion", json!({})).await;
        assert_eq!(v["requestStatus"], json!({ "result": true, "code": 100 }));
        assert_eq!(v["responseData"]["rpcVersion"], 1);
        assert!(
            v["responseData"]["availableRequests"].as_array().unwrap().iter().any(|r| r == "SetCurrentProgramScene")
        );

        // Scenes.
        let list = c.req("GetSceneList", json!({})).await;
        assert_eq!(list["responseData"]["currentProgramSceneName"], "Game");
        assert_eq!(list["responseData"]["scenes"].as_array().unwrap().len(), 2);
        let set = c.req("SetCurrentProgramScene", json!({ "sceneName": "BRB" })).await;
        assert_eq!(set["requestStatus"]["result"], true);
        assert_eq!(c.event("CurrentProgramSceneChanged").await["sceneName"], "BRB");
        let by_uuid = c.req("SetCurrentProgramScene", json!({ "sceneUuid": uuid_of("Game") })).await;
        assert_eq!(by_uuid["requestStatus"]["result"], true);
        assert_eq!(c.event("CurrentProgramSceneChanged").await["sceneName"], "Game");
        let none = c.req("SetCurrentProgramScene", json!({ "sceneName": "Nope" })).await;
        assert_eq!(
            (none["requestStatus"]["result"].clone(), none["requestStatus"]["code"].clone()),
            (json!(false), json!(600))
        );
        let missing = c.req("SetCurrentProgramScene", json!({})).await;
        assert_eq!(missing["requestStatus"]["code"], 300);
        let wrong = c.req("SetInputMute", json!({ "inputName": "Mic", "inputMuted": "yes" })).await;
        assert_eq!(wrong["requestStatus"]["code"], 401);

        // Studio Mode.
        assert_eq!(c.req("GetCurrentPreviewScene", json!({})).await["requestStatus"]["code"], 506);
        assert_eq!(c.req("TriggerStudioModeTransition", json!({})).await["requestStatus"]["code"], 506);
        c.req("SetStudioModeEnabled", json!({ "studioModeEnabled": true })).await;
        c.req("SetCurrentPreviewScene", json!({ "sceneName": "BRB" })).await;
        assert_eq!(c.req("GetCurrentPreviewScene", json!({})).await["responseData"]["sceneName"], "BRB");
        c.req("TriggerStudioModeTransition", json!({})).await;
        assert_eq!(c.event("CurrentProgramSceneChanged").await["sceneName"], "BRB");

        // Inputs.
        assert_eq!(c.req("GetInputList", json!({})).await["responseData"]["inputs"][0]["inputName"], "Mic");
        assert_eq!(c.req("GetInputMute", json!({ "inputName": "Mic" })).await["responseData"]["inputMuted"], false);
        let t = c.req("ToggleInputMute", json!({ "inputName": "Mic" })).await;
        assert_eq!(t["responseData"]["inputMuted"], true);
        assert_eq!(c.event("InputMuteStateChanged").await["inputMuted"], true);
        c.req("SetInputMute", json!({ "inputName": "Mic", "inputMuted": false })).await;
        c.req("SetInputVolume", json!({ "inputName": "Mic", "inputVolumeDb": -12.0 })).await;
        let vol = c.req("GetInputVolume", json!({ "inputName": "Mic" })).await;
        assert!((vol["responseData"]["inputVolumeDb"].as_f64().unwrap() + 12.0).abs() < 1e-6);
        assert!((vol["responseData"]["inputVolumeMul"].as_f64().unwrap() - 0.2512).abs() < 1e-3);
        assert!(c.event("InputVolumeChanged").await["inputVolumeDb"].as_f64().unwrap() < -11.9);
        assert_eq!(
            c.req("SetInputVolume", json!({ "inputName": "Mic", "inputVolumeMul": 99 })).await["requestStatus"]["code"],
            402
        );
        assert_eq!(c.req("GetInputMute", json!({ "inputName": "Nope" })).await["requestStatus"]["code"], 600);

        // Scene items.
        let items = c.req("GetSceneItemList", json!({ "sceneName": "Game" })).await;
        assert_eq!(items["responseData"]["sceneItems"].as_array().unwrap().len(), 2);
        let id = c.req("GetSceneItemId", json!({ "sceneName": "Game", "sourceName": "Cam" })).await;
        assert_eq!(id["responseData"]["sceneItemId"], 7);
        c.req("SetSceneItemEnabled", json!({ "sceneName": "Game", "sceneItemId": 7, "sceneItemEnabled": false })).await;
        assert!(!fake.with(|s| s.cam_visible));
        let ev = c.event("SceneItemEnableStateChanged").await;
        assert_eq!((ev["sceneItemId"].clone(), ev["sceneItemEnabled"].clone()), (json!(7), json!(false)));
        assert_eq!(
            c.req("GetSceneItemEnabled", json!({ "sceneName": "Game", "sceneItemId": 7 })).await["responseData"]["sceneItemEnabled"],
            false
        );

        // Filters.
        assert_eq!(
            c.req("GetSourceFilterList", json!({ "sourceName": "Cam" })).await["responseData"]["filters"][0]["filterEnabled"],
            true
        );
        c.req("SetSourceFilterEnabled", json!({ "sourceName": "Cam", "filterName": "Blur", "filterEnabled": false }))
            .await;
        assert!(!fake.with(|s| s.filter_on));
        assert_eq!(
            c.req("SetSourceFilterEnabled", json!({ "sourceName": "Cam", "filterName": "X", "filterEnabled": true }))
                .await["requestStatus"]["code"],
            600
        );

        // Record, pause, replay buffer, virtual camera.
        assert_eq!(c.req("StopRecord", json!({})).await["requestStatus"]["code"], 501);
        c.req("StartRecord", json!({})).await;
        assert_eq!(c.event("RecordStateChanged").await["outputActive"], true);
        assert_eq!(c.req("StartRecord", json!({})).await["requestStatus"]["code"], 500);
        assert_eq!(c.req("ResumeRecord", json!({})).await["requestStatus"]["code"], 503);
        c.req("PauseRecord", json!({})).await;
        assert_eq!(c.req("PauseRecord", json!({})).await["requestStatus"]["code"], 502);
        let st = c.req("GetRecordStatus", json!({})).await;
        assert_eq!(
            (st["responseData"]["outputActive"].clone(), st["responseData"]["outputPaused"].clone()),
            (json!(true), json!(true))
        );
        assert_eq!(st["responseData"]["outputTimecode"], "00:01:01.500");
        c.req("ResumeRecord", json!({})).await;
        let stop = c.req("StopRecord", json!({})).await;
        assert_eq!(stop["responseData"]["outputPath"], "/r/a.mkv");
        assert_eq!(c.req("SaveReplayBuffer", json!({})).await["requestStatus"]["code"], 501);
        c.req("StartReplayBuffer", json!({})).await;
        c.req("SaveReplayBuffer", json!({})).await;
        assert_eq!(c.event("ReplayBufferSaved").await["savedReplayPath"], "/r/replay.mp4");
        assert_eq!(
            c.req("GetLastReplayBufferReplay", json!({})).await["responseData"]["savedReplayPath"],
            "/r/replay.mp4"
        );
        c.req("StartVirtualCam", json!({})).await;
        assert_eq!(c.req("GetVirtualCamStatus", json!({})).await["responseData"]["outputActive"], true);
        assert_eq!(c.event("VirtualcamStateChanged").await["outputActive"], true);
        c.req("StopVirtualCam", json!({})).await;

        // The stream: its state, and an already-running one.
        assert_eq!(c.req("GetStreamStatus", json!({})).await["responseData"]["outputActive"], false);
        assert_eq!(c.req("StopStream", json!({})).await["requestStatus"]["code"], 501);
        fake.with(|s| s.stream = true);
        assert_eq!(c.req("StartStream", json!({})).await["requestStatus"]["code"], 500);
        assert_eq!(c.event("StreamStateChanged").await["outputActive"], true);
        let stats = c.req("GetStats", json!({})).await;
        assert_eq!(stats["responseData"]["activeFps"], 30.0);

        // A change made elsewhere (the dashboard) reaches the client as an event too.
        fake.with(|s| s.current = "Game".into());
        assert_eq!(c.event("CurrentProgramSceneChanged").await["sceneName"], "Game");

        // Requests this doesn't have: the protocol's "unknown request type", not a crash.
        let unknown = c.req("CreateScene", json!({ "sceneName": "X" })).await;
        assert_eq!(
            (unknown["requestStatus"]["result"].clone(), unknown["requestStatus"]["code"].clone()),
            (json!(false), json!(204))
        );
        c.send_raw(json!({ "op": 6, "d": { "requestId": "z" } })).await;
        let m = c.json().await;
        assert_eq!(m["d"]["requestStatus"]["code"], 203);

        // A batch.
        c.send_raw(json!({ "op": 8, "d": { "requestId": "b", "haltOnFailure": true, "requests": [
            { "requestType": "GetVersion", "requestId": "1" },
            { "requestType": "Nope", "requestId": "2" },
            { "requestType": "GetVersion", "requestId": "3" } ] } }))
            .await;
        let b = loop {
            let m = c.json().await;
            if m["op"] == 9 {
                break m;
            }
        };
        assert_eq!(b["d"]["results"].as_array().unwrap().len(), 2, "stopped at the failure");
        drop(c);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(e.obsws_view().await.clients, 0);
    }

    #[tokio::test]
    async fn events_follow_what_the_client_subscribed_to() {
        let (_e, fake, port, pw, _dir) = on().await;
        // Only Scenes (4): a mute isn't told, a scene change is.
        let mut c = Client::open(port).await.unwrap();
        c.identify(&pw, Some(SUB_SCENES)).await;
        fake.with(|s| s.mic.0 = true);
        fake.with(|s| s.current = "BRB".into());
        c.req("GetVersion", json!({})).await;
        assert_eq!(c.event("CurrentProgramSceneChanged").await["sceneName"], "BRB");
        assert!(c.events.iter().all(|e| e["eventType"] != "InputMuteStateChanged"));
    }

    #[tokio::test]
    async fn a_wrong_password_or_a_request_before_identify_is_turned_away() {
        let (_e, _fake, port, pw, _dir) = on().await;
        // Wrong password: closed with 4009.
        let mut bad = Client::open(port).await.unwrap();
        let hello = bad.json().await;
        let auth = client_auth(
            "not-the-password",
            hello["d"]["authentication"]["salt"].as_str().unwrap(),
            hello["d"]["authentication"]["challenge"].as_str().unwrap(),
        );
        bad.send_raw(json!({ "op": 1, "d": { "rpcVersion": 1, "authentication": auth } })).await;
        assert_eq!(bad.closed_with().await, Some(CLOSE_AUTH));
        // No authentication at all.
        let mut none = Client::open(port).await.unwrap();
        none.json().await;
        none.send_raw(json!({ "op": 1, "d": { "rpcVersion": 1 } })).await;
        assert_eq!(none.closed_with().await, Some(CLOSE_AUTH));
        // A request before Identify: closed with 4007, and nothing is answered.
        let mut early = Client::open(port).await.unwrap();
        early.json().await;
        early.send_raw(json!({ "op": 6, "d": { "requestType": "StartStream", "requestId": "1" } })).await;
        assert_eq!(early.closed_with().await, Some(CLOSE_NOT_IDENTIFIED));
        // Not JSON, and a binary frame.
        let mut junk = Client::open(port).await.unwrap();
        junk.json().await;
        junk.ws.send(Message::Text("not json".into())).await.unwrap();
        assert_eq!(junk.closed_with().await, Some(CLOSE_DECODE));
        // The right password still works after all that (3 misses so far, under the limit).
        let mut ok = Client::open(port).await.unwrap();
        assert_eq!(ok.identify(&pw, None).await["op"], 2);
        // A second Identify is refused.
        ok.send_raw(json!({ "op": 1, "d": { "rpcVersion": 1 } })).await;
        assert_eq!(ok.closed_with().await, Some(CLOSE_ALREADY));
    }

    #[tokio::test]
    async fn wrong_passwords_from_this_computer_are_slowed_but_never_shut_it_out() {
        let (_e, _fake, port, pw, _dir) = on().await;
        for _ in 0..MAX_FAILS {
            let mut c = Client::open(port).await.unwrap();
            let hello = c.json().await;
            let auth = client_auth(
                "x",
                hello["d"]["authentication"]["salt"].as_str().unwrap(),
                hello["d"]["authentication"]["challenge"].as_str().unwrap(),
            );
            let t = Instant::now();
            c.send_raw(json!({ "op": 1, "d": { "rpcVersion": 1, "authentication": auth } })).await;
            assert_eq!(c.closed_with().await, Some(CLOSE_AUTH));
            assert!(t.elapsed() >= FAIL_DELAY, "a wrong password is answered only after a wait");
        }
        // A page on this computer guessing wrong can't lock the owner's own Stream Deck out.
        let mut ok = Client::open(port).await.unwrap();
        assert_eq!(ok.identify(&pw, None).await["op"], 2);
    }

    #[tokio::test]
    async fn too_many_wrong_passwords_shut_another_device_out_for_a_while() {
        let (e, _fake, _port, _pw, _dir) = on().await;
        let lan: IpAddr = "192.168.1.50".parse().unwrap();
        for _ in 0..MAX_FAILS - 1 {
            e.ows_note_fail(lan);
        }
        assert!(!e.ows_shut_out(lan), "not yet");
        e.ows_note_fail(lan);
        assert!(e.ows_shut_out(lan), "five wrong in a minute shut it out");
        assert!(!e.ows_shut_out("192.168.1.51".parse().unwrap()), "only that device");
        let here: IpAddr = "127.0.0.1".parse().unwrap();
        for _ in 0..MAX_FAILS * 2 {
            e.ows_note_fail(here);
        }
        assert!(!e.ows_shut_out(here), "this computer never is");
    }

    #[tokio::test]
    async fn connections_that_never_log_in_cant_take_every_place() {
        let (_e, _fake, port, pw, _dir) = on().await;
        let mut idle = Vec::new();
        let mut first_hello = Value::Null;
        for i in 0..MAX_PENDING {
            let mut c = Client::open(port).await.unwrap();
            let hello = c.json().await; // Hello, then nothing.
            if i == 0 {
                first_hello = hello;
            }
            idle.push(c);
        }
        let mut extra = Client::open(port).await;
        if let Ok(c) = extra.as_mut() {
            assert!(
                c.frame().await.is_none_or(|m| !matches!(m, Ok(Message::Text(_)))),
                "no hello past the waiting places"
            );
        }
        // One of them logs in: its waiting place frees, and a real client gets in.
        let auth = client_auth(
            &pw,
            first_hello["d"]["authentication"]["salt"].as_str().unwrap(),
            first_hello["d"]["authentication"]["challenge"].as_str().unwrap(),
        );
        idle[0].send_raw(json!({ "op": 1, "d": { "rpcVersion": 1, "authentication": auth } })).await;
        assert_eq!(idle[0].json().await["op"], 2);
        let mut real = Client::open(port).await.unwrap();
        assert_eq!(real.identify(&pw, None).await["op"], 2);
    }

    #[tokio::test]
    async fn only_eight_clients_at_once_and_a_new_password_lets_everyone_go() {
        let (e, _fake, port, pw, _dir) = on().await;
        let mut held = Vec::new();
        for _ in 0..MAX_CLIENTS {
            let mut c = Client::open(port).await.unwrap();
            assert_eq!(c.identify(&pw, None).await["op"], 2);
            held.push(c);
        }
        let mut ninth = Client::open(port).await;
        if let Ok(c) = ninth.as_mut() {
            assert!(c.frame().await.is_none_or(|m| !matches!(m, Ok(Message::Text(_)))), "no hello for the ninth");
        }
        let fresh = e.obsws_regenerate().await.unwrap();
        assert_ne!(fresh, pw);
        for c in &mut held {
            assert_eq!(c.closed_with().await, Some(CLOSE_INVALIDATED));
        }
        // The old password is no good any more; the new one is.
        let mut old = Client::open(port).await.unwrap();
        let hello = old.json().await;
        let auth = client_auth(
            &pw,
            hello["d"]["authentication"]["salt"].as_str().unwrap(),
            hello["d"]["authentication"]["challenge"].as_str().unwrap(),
        );
        old.send_raw(json!({ "op": 1, "d": { "rpcVersion": 1, "authentication": auth } })).await;
        assert_eq!(old.closed_with().await, Some(CLOSE_AUTH));
        let mut new = Client::open(port).await.unwrap();
        assert_eq!(new.identify(&fresh, None).await["op"], 2);
    }
}
