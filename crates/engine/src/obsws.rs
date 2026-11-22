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

use crate::studio_host::{
    AudioChange, FilterEdit, HOTKEY_ACTIONS, ItemEdit, ItemTransform, MediaAct, Monitor, NewSource, ShelfAct,
    StudioAct, TRANSITION_KINDS,
};
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
const POLL: Duration = Duration::from_secs(1);
/// Maximum failed LAN IPs tracked so spoofed floods cannot grow memory without bound.
const MAX_FAIL_IPS: usize = 1024;
/// Failed passwords from one address before it's shut out, and for how long.
const MAX_FAILS: u32 = 5;
const FAIL_WINDOW: Duration = Duration::from_secs(60);
const SHUT_OUT: Duration = Duration::from_secs(60);
const RPC_VERSION: u64 = 1;
const WS_VERSION: &str = "5.5.4";
const SUBPROTOCOL: &str = "obswebsocket.json";
const MAX_BATCH: usize = 100;

// Event subscription bits (EventSubscription).
#[allow(dead_code)]
const SUB_GENERAL: u64 = 1 << 0;
#[allow(dead_code)]
const SUB_CONFIG: u64 = 1 << 1;
const SUB_SCENES: u64 = 1 << 2;
const SUB_INPUTS: u64 = 1 << 3;
const SUB_TRANSITIONS: u64 = 1 << 4;
#[allow(dead_code)]
const SUB_FILTERS: u64 = 1 << 5;
const SUB_OUTPUTS: u64 = 1 << 6;
const SUB_SCENE_ITEMS: u64 = 1 << 7;
#[allow(dead_code)]
const SUB_MEDIA_INPUTS: u64 = 1 << 8;
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
struct ListenerTask {
    stop: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
pub(crate) struct Runtime {
    task: tokio::sync::Mutex<Option<ListenerTask>>,
    apply_lock: tokio::sync::Mutex<()>,
    listening_tasks: AtomicUsize,
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
            task: tokio::sync::Mutex::new(None),
            apply_lock: tokio::sync::Mutex::new(()),
            listening_tasks: AtomicUsize::new(0),
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

fn bounds_to_obs(b: &str) -> &'static str {
    match b {
        "stretch" => "OBS_BOUNDS_STRETCH",
        "fit" => "OBS_BOUNDS_SCALE_INNER",
        "fill" => "OBS_BOUNDS_SCALE_OUTER",
        "width" => "OBS_BOUNDS_SCALE_TO_WIDTH",
        "height" => "OBS_BOUNDS_SCALE_TO_HEIGHT",
        "max" => "OBS_BOUNDS_MAX_ONLY",
        _ => "OBS_BOUNDS_NONE",
    }
}

fn obs_to_bounds(s: &str) -> Result<&'static str, Fail> {
    match s {
        "OBS_BOUNDS_NONE" | "none" => Ok("none"),
        "OBS_BOUNDS_STRETCH" | "stretch" => Ok("stretch"),
        "OBS_BOUNDS_SCALE_INNER" | "fit" => Ok("fit"),
        "OBS_BOUNDS_SCALE_OUTER" | "fill" => Ok("fill"),
        "OBS_BOUNDS_SCALE_TO_WIDTH" | "width" => Ok("width"),
        "OBS_BOUNDS_SCALE_TO_HEIGHT" | "height" => Ok("height"),
        "OBS_BOUNDS_MAX_ONLY" | "max" => Ok("max"),
        _ => Err(fail(ST_OUT_OF_RANGE, "Unknown `boundsType` value.")),
    }
}

fn monitor_to_obs(m: &str) -> &'static str {
    match m {
        "only_me" => "OBS_MONITORING_TYPE_MONITOR_ONLY",
        "me_and_stream" => "OBS_MONITORING_TYPE_MONITOR_AND_OUTPUT",
        _ => "OBS_MONITORING_TYPE_NONE",
    }
}

fn obs_to_monitor(s: &str) -> Result<Monitor, Fail> {
    match s {
        "OBS_MONITORING_TYPE_NONE" => Ok(Monitor::Off),
        "OBS_MONITORING_TYPE_MONITOR_ONLY" => Ok(Monitor::OnlyMe),
        "OBS_MONITORING_TYPE_MONITOR_AND_OUTPUT" => Ok(Monitor::MeAndStream),
        _ => Err(fail(ST_BAD_TYPE, "Invalid `monitorType`.")),
    }
}

fn media_state_to_obs(s: &str) -> &'static str {
    match s {
        "playing" => "OBS_MEDIA_STATE_PLAYING",
        "paused" => "OBS_MEDIA_STATE_PAUSED",
        "stopped" => "OBS_MEDIA_STATE_STOPPED",
        "ended" => "OBS_MEDIA_STATE_ENDED",
        "opening" => "OBS_MEDIA_STATE_OPENING",
        "buffering" => "OBS_MEDIA_STATE_BUFFERING",
        "error" => "OBS_MEDIA_STATE_ERROR",
        _ => "OBS_MEDIA_STATE_NONE",
    }
}

fn obs_to_media_act(s: &str) -> Result<MediaAct, Fail> {
    match s {
        "OBS_WEBSOCKET_MEDIA_INPUT_ACTION_PLAY" => Ok(MediaAct::Play),
        "OBS_WEBSOCKET_MEDIA_INPUT_ACTION_PAUSE" => Ok(MediaAct::Pause),
        "OBS_WEBSOCKET_MEDIA_INPUT_ACTION_PLAY_PAUSE" => Ok(MediaAct::PlayPause),
        "OBS_WEBSOCKET_MEDIA_INPUT_ACTION_STOP" => Ok(MediaAct::Stop),
        "OBS_WEBSOCKET_MEDIA_INPUT_ACTION_RESTART" => Ok(MediaAct::Restart),
        "OBS_WEBSOCKET_MEDIA_INPUT_ACTION_NEXT" => Ok(MediaAct::Next),
        "OBS_WEBSOCKET_MEDIA_INPUT_ACTION_PREVIOUS" => Ok(MediaAct::Previous),
        _ => Err(fail(ST_OUT_OF_RANGE, "Unsupported `mediaAction`.")),
    }
}

fn tracks_to_obs(v: &Value) -> Value {
    let list: Vec<u64> = v.as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
    let mut obj = serde_json::Map::new();
    for track in 1..=6u64 {
        obj.insert(track.to_string(), Value::Bool(list.contains(&track)));
    }
    Value::Object(obj)
}

fn obs_to_tracks(v: &Value) -> Result<Vec<u8>, Fail> {
    let Some(obj) = v.as_object() else {
        return Err(fail(ST_BAD_TYPE, "`inputAudioTracks` must be an object mapping track numbers to booleans."));
    };
    let mut out = Vec::new();
    for track in 1..=6u8 {
        if let Some(val) = obj.get(&track.to_string()) {
            let on = val.as_bool().ok_or_else(|| fail(ST_BAD_TYPE, "Track values must be booleans."))?;
            if on {
                out.push(track);
            }
        }
    }
    Ok(out)
}

fn transform_to_obs(t: &ItemTransform) -> Value {
    let w = if t.width == 0 { 1920.0 } else { f64::from(t.width) };
    let h = if t.height == 0 { 1080.0 } else { f64::from(t.height) };
    json!({
        "alignment": t.alignment,
        "boundsAlignment": t.bounds_alignment,
        "boundsHeight": t.bounds_height,
        "boundsType": bounds_to_obs(&t.bounds),
        "boundsWidth": t.bounds_width,
        "cropBottom": t.crop_bottom,
        "cropLeft": t.crop_left,
        "cropRight": t.crop_right,
        "cropTop": t.crop_top,
        "height": h * f64::from(t.scale_y.abs()),
        "positionX": t.x,
        "positionY": t.y,
        "rotation": t.rotation,
        "scaleX": t.scale_x,
        "scaleY": t.scale_y,
        "sourceHeight": h,
        "sourceWidth": w,
        "width": w * f64::from(t.scale_x.abs()),
    })
}

fn patch_transform(t: &mut ItemTransform, d: &Value) -> Result<(), Fail> {
    let f32_opt = |k: &str| -> Result<Option<f32>, Fail> {
        match d.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => {
                v.as_f64().map(|n| Some(n as f32)).ok_or_else(|| fail(ST_BAD_TYPE, format!("`{k}` must be a number.")))
            }
        }
    };
    let u32_opt = |k: &str| -> Result<Option<u32>, Fail> {
        match d.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| fail(ST_BAD_TYPE, format!("`{k}` must be a non-negative integer."))),
        }
    };
    let i32_opt = |k: &str| -> Result<Option<i32>, Fail> {
        match d.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| fail(ST_BAD_TYPE, format!("`{k}` must be an integer."))),
        }
    };
    if let Some(v) = f32_opt("positionX")? {
        t.x = v;
    }
    if let Some(v) = f32_opt("positionY")? {
        t.y = v;
    }
    if let Some(v) = f32_opt("rotation")? {
        t.rotation = v;
    }
    if let Some(v) = f32_opt("scaleX")? {
        t.scale_x = v;
    }
    if let Some(v) = f32_opt("scaleY")? {
        t.scale_y = v;
    }
    if let Some(v) = u32_opt("alignment")? {
        t.alignment = v;
    }
    if let Some(v) = u32_opt("boundsAlignment")? {
        t.bounds_alignment = v;
    }
    if let Some(v) = f32_opt("boundsWidth")? {
        t.bounds_width = v;
    }
    if let Some(v) = f32_opt("boundsHeight")? {
        t.bounds_height = v;
    }
    if let Some(v) = i32_opt("cropLeft")? {
        t.crop_left = v;
    }
    if let Some(v) = i32_opt("cropTop")? {
        t.crop_top = v;
    }
    if let Some(v) = i32_opt("cropRight")? {
        t.crop_right = v;
    }
    if let Some(v) = i32_opt("cropBottom")? {
        t.crop_bottom = v;
    }
    if let Some(v) = d.get("boundsType").filter(|v| !v.is_null()) {
        let s = v.as_str().ok_or_else(|| fail(ST_BAD_TYPE, "`boundsType` must be a string."))?;
        t.bounds = obs_to_bounds(s)?.to_string();
    }
    Ok(())
}

fn transition_parts(scenes_doc: &Value) -> (String, u32) {
    let t = &scenes_doc["transition"];
    let kind = t["kind"].as_str().unwrap_or("fade_transition").to_string();
    let ms = t["ms"].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(300);
    (kind, ms)
}

/// The requests answered (what `GetVersion` lists).
const REQUESTS: &[&str] = &[
    "GetVersion",
    "GetStats",
    "GetHotkeyList",
    // Scenes
    "GetSceneList",
    "GetCurrentProgramScene",
    "SetCurrentProgramScene",
    "GetCurrentPreviewScene",
    "SetCurrentPreviewScene",
    "CreateScene",
    "RemoveScene",
    // Studio Mode & Transitions
    "GetStudioModeEnabled",
    "SetStudioModeEnabled",
    "TriggerStudioModeTransition",
    "GetTransitionKindList",
    "GetCurrentSceneTransition",
    "SetCurrentSceneTransition",
    "SetCurrentSceneTransitionDuration",
    "SetCurrentSceneTransitionSettings",
    "SetTBarPosition",
    // Scene Collections & Profiles
    "GetSceneCollectionList",
    "SetCurrentSceneCollection",
    "CreateSceneCollection",
    "GetProfileList",
    "SetCurrentProfile",
    // Inputs & Audio
    "GetInputKindList",
    "GetInputList",
    "GetInputSettings",
    "SetInputSettings",
    "CreateInput",
    "SetInputName",
    "GetInputMute",
    "SetInputMute",
    "ToggleInputMute",
    "GetInputVolume",
    "SetInputVolume",
    "GetInputAudioMonitorType",
    "SetInputAudioMonitorType",
    "GetInputAudioSyncOffset",
    "SetInputAudioSyncOffset",
    "GetInputAudioBalance",
    "SetInputAudioBalance",
    "GetInputAudioTracks",
    "SetInputAudioTracks",
    // Scene Items
    "GetSceneItemList",
    "GetSceneItemId",
    "CreateSceneItem",
    "RemoveSceneItem",
    "DuplicateSceneItem",
    "GetSceneItemTransform",
    "SetSceneItemTransform",
    "GetSceneItemEnabled",
    "SetSceneItemEnabled",
    "GetSceneItemLocked",
    "SetSceneItemLocked",
    "GetSceneItemIndex",
    "SetSceneItemIndex",
    // Stream / Record / Replay / Virtual Camera
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
    "SplitRecordFile",
    "CreateRecordChapter",
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
    // Filters
    "GetSourceFilterList",
    "GetSourceFilter",
    "CreateSourceFilter",
    "RemoveSourceFilter",
    "SetSourceFilterEnabled",
    "SetSourceFilterSettings",
    "SetSourceFilterIndex",
    // Media Inputs & Screenshots
    "GetMediaInputStatus",
    "TriggerMediaInputAction",
    "SetMediaInputCursor",
    "GetSourceScreenshot",
    "SaveSourceScreenshot",
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
        self.obsws_apply().await;
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
        self.obsws_apply().await;
        self.obsws_settle().await;
        Ok(pw)
    }

    /// Starts what the saved settings ask for (the server calls this once it's up).
    pub fn start_obsws(&self) {
        let engine = self.clone();
        tokio::spawn(async move { engine.obsws_apply().await });
        let mut slot = self.inner.obsws.poller.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_none_or(tokio::task::JoinHandle::is_finished) {
            let engine = self.clone();
            *slot = Some(tokio::spawn(async move { engine.ows_poll_loop().await }));
        }
    }

    /// Looks at the saved settings again: closes everyone, awaits the old listener's socket drop,
    /// and listens afresh when on.
    async fn obsws_apply(&self) {
        let rt = &self.inner.obsws;
        let _apply = rt.apply_lock.lock().await;
        rt.generation.send_modify(|g| *g += 1);
        let prev = rt.task.lock().await.take();
        if let Some(ListenerTask { stop, mut handle }) = prev {
            let _ = stop.send(true);
            if tokio::time::timeout(Duration::from_secs(2), &mut handle).await.is_err() {
                handle.abort();
                let _ = handle.await;
            }
        }
        *rt.status.lock().unwrap_or_else(|p| p.into_inner()) = (false, None);
        let (stop, stop_rx) = watch::channel(false);
        let engine = self.clone();
        let handle = tokio::spawn(async move { engine.ows_listen(stop_rx).await });
        *rt.task.lock().await = Some(ListenerTask { stop, handle });
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

    async fn ows_listen(&self, mut stop_rx: watch::Receiver<bool>) {
        struct CountGuard<'a>(&'a AtomicUsize);
        impl Drop for CountGuard<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let saved = self.ows_saved().await;
        let set_status = |listening: bool, error: Option<String>| {
            *self.inner.obsws.status.lock().unwrap_or_else(|p| p.into_inner()) = (listening, error);
        };
        if !saved.enabled || *stop_rx.borrow() {
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
        self.inner.obsws.listening_tasks.fetch_add(1, Ordering::SeqCst);
        let _guard = CountGuard(&self.inner.obsws.listening_tasks);
        tracing::info!(port = saved.port, lan = saved.lan, "obs-websocket is listening");
        set_status(true, None);
        let generation = *self.inner.obsws.generation.borrow();
        loop {
            let accepted = tokio::select! {
                biased;
                _ = stop_rx.changed() => break,
                res = listener.accept() => res,
            };
            let Ok((stream, peer)) = accepted else {
                tokio::select! {
                    biased;
                    _ = stop_rx.changed() => break,
                    () = tokio::time::sleep(Duration::from_millis(100)) => continue,
                }
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
        let now = Instant::now();
        map.retain(|_, v| v.shut_until.is_some_and(|t| t > now) || now.duration_since(v.since) < FAIL_WINDOW);
        if map.len() >= MAX_FAIL_IPS
            && !map.contains_key(&ip)
            && let Some(oldest) = map.iter().min_by_key(|(_, v)| v.since).map(|(k, _)| *k)
        {
            map.remove(&oldest);
        }
        let f = map.entry(ip).or_insert(Fails { count: 0, since: now, shut_until: None });
        if now.duration_since(f.since) > FAIL_WINDOW {
            *f = Fails { count: 0, since: now, shut_until: None };
        }
        f.count += 1;
        if f.count >= MAX_FAILS {
            f.shut_until = Some(now + SHUT_OUT);
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
                                    // obs-websocket v5 has only Identified (op 2), even in response to Reidentify (op 3).
                                    let reply = json!({ "op": 2,
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
                            let Some(reply) = self.ows_batch(d, generation, &mut gen_rx).await else {
                                close(&mut ws, CLOSE_INVALIDATED, "The settings changed.").await;
                                break;
                            };
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

    async fn ows_batch(&self, d: &Value, generation: u64, gen_rx: &mut watch::Receiver<u64>) -> Option<Value> {
        let id = d.get("requestId").cloned().unwrap_or(Value::Null);
        let halt = d.get("haltOnFailure").and_then(Value::as_bool).unwrap_or(false);
        let mut results = Vec::new();
        for r in d.get("requests").and_then(Value::as_array).into_iter().flatten().take(MAX_BATCH) {
            if *gen_rx.borrow() != generation {
                return None;
            }
            let out = tokio::select! {
                biased;
                changed = gen_rx.changed() => {
                    if changed.is_err() || *gen_rx.borrow() != generation {
                        return None;
                    }
                    continue;
                }
                out = self.ows_one(r) => out,
            };
            let ok = out["requestStatus"]["result"] == Value::Bool(true);
            results.push(out);
            if halt && !ok {
                break;
            }
        }
        Some(json!({ "requestId": id, "results": results }))
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
                if ows_mutates(kind) {
                    // A change made here shows in the events at once, without polling on read-only Get* calls.
                    self.inner.obsws.poll_now.notify_one();
                }
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

    async fn ows_resolve_source_uuid(&self, u: &str, missing: &'static str) -> Result<String, Fail> {
        let v = self.studio_act(StudioAct::Sources).await.map_err(obs_error)?;
        if let Some(n) = v.as_array().into_iter().flatten().filter_map(|s| s["name"].as_str()).find(|n| uuid_of(n) == u)
        {
            return Ok(n.to_string());
        }
        // Global channels live outside `Sources`; check cached snapshot inputs when available.
        if let Ok(host) = self.studio_host()
            && let Some(Ok(snap)) = host.obsws_snapshot().await
            && let Some(n) = snap["inputs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a["name"].as_str())
                .find(|n| uuid_of(n) == u)
        {
            return Ok(n.to_string());
        }
        Err(fail(ST_NOT_FOUND, missing))
    }

    async fn ows_input_name(&self, d: &Value) -> Result<String, Fail> {
        if let Some(n) = d.get("inputName").and_then(Value::as_str) {
            return Ok(n.to_string());
        }
        if let Some(u) = d.get("inputUuid").and_then(Value::as_str) {
            return self.ows_resolve_source_uuid(u, "No input was found by that uuid.").await;
        }
        Err(fail(ST_MISSING_FIELD, "Your request needs `inputName` or `inputUuid`."))
    }

    async fn ows_source_name(&self, d: &Value) -> Result<String, Fail> {
        for key in ["sourceName", "inputName"] {
            if let Some(n) = d.get(key).and_then(Value::as_str) {
                return Ok(n.to_string());
            }
        }
        for key in ["sourceUuid", "inputUuid"] {
            if let Some(u) = d.get(key).and_then(Value::as_str) {
                return self.ows_resolve_source_uuid(u, "No source was found by that uuid.").await;
            }
        }
        Err(fail(ST_MISSING_FIELD, "Your request needs `sourceName` or `sourceUuid`."))
    }

    /// A sound's mixer row in O(1), without scanning every scene.
    async fn ows_audio(&self, name: &str) -> Result<Value, Fail> {
        match self.studio_act(StudioAct::AudioByName { name: name.to_string() }).await {
            Ok(v) if !v.is_null() => Ok(v),
            Ok(_) => Err(fail(ST_NOT_FOUND, "No input was found by that name.")),
            Err(e) if e.to_string().contains("no source") => {
                Err(fail(ST_NOT_FOUND, "No input was found by that name."))
            }
            Err(e) => Err(obs_error(e)),
        }
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

    async fn ows_transition_request(&self, kind: &str, d: &Value) -> Result<Value, Fail> {
        match kind {
            "GetTransitionKindList" => Ok(json!({ "transitionKinds": TRANSITION_KINDS })),
            "GetCurrentSceneTransition" => {
                let s = self.ows_scenes().await?;
                let (tk, ms) = transition_parts(&s);
                let settings = self
                    .studio_act(StudioAct::TransitionSettings { kind: tk.clone() })
                    .await
                    .unwrap_or_else(|_| json!({}));
                Ok(json!({
                    "transitionName": tk,
                    "transitionUuid": uuid_of(&tk),
                    "transitionKind": tk,
                    "transitionDuration": ms,
                    "transitionConfigurable": tk != "cut_transition",
                    "transitionSettings": settings,
                }))
            }
            "SetCurrentSceneTransition" => {
                let tk = d
                    .get("transitionName")
                    .or_else(|| d.get("transitionKind"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(ST_MISSING_FIELD, "Your request needs `transitionName`."))?
                    .to_string();
                let (_, ms) = transition_parts(&self.ows_scenes().await?);
                self.studio_act(StudioAct::SetTransition { kind: tk, ms }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "SetCurrentSceneTransitionDuration" => {
                let ms_raw = num_field(d, "transitionDuration")?;
                if !ms_raw.is_finite() || !(0.0..=20_000.0).contains(&ms_raw) {
                    return Err(fail(ST_OUT_OF_RANGE, "`transitionDuration` must be from 0 to 20000 ms."));
                }
                let (tk, _) = transition_parts(&self.ows_scenes().await?);
                self.studio_act(StudioAct::SetTransition { kind: tk, ms: ms_raw.round() as u32 })
                    .await
                    .map_err(obs_error)?;
                Ok(Value::Null)
            }
            "SetCurrentSceneTransitionSettings" => {
                let settings = field(d, "transitionSettings")?.clone();
                if !settings.is_object() {
                    return Err(fail(ST_BAD_TYPE, "`transitionSettings` must be an object."));
                }
                let (tk, _) = transition_parts(&self.ows_scenes().await?);
                self.studio_act(StudioAct::SetTransitionSettings { kind: tk, settings }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "SetTBarPosition" => {
                let pos = num_field(d, "position")?;
                if !pos.is_finite() || !(0.0..=1.0).contains(&pos) {
                    return Err(fail(ST_OUT_OF_RANGE, "`position` must be between 0.0 and 1.0."));
                }
                let release = d.get("release").and_then(Value::as_bool).unwrap_or(false);
                self.studio_act(StudioAct::TBar { at: pos as f32, release }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            _ => Err(fail(ST_UNKNOWN, "Unknown request type.")),
        }
    }

    async fn ows_shelf_request(&self, kind: &str, d: &Value) -> Result<Value, Fail> {
        let shelf_kind = if kind.contains("Profile") { "profiles" } else { "collections" };
        match kind {
            "GetSceneCollectionList" | "GetProfileList" => {
                let v = self.studio_shelf(shelf_kind, ShelfAct::List).await.map_err(obs_error)?;
                let current_id = v["current"].as_str().unwrap_or_default();
                let mut current_name = current_id.to_string();
                let mut names = Vec::new();
                for entry in v["list"].as_array().into_iter().flatten() {
                    if let Some(name) = entry["name"].as_str() {
                        if entry["id"].as_str() == Some(current_id) {
                            current_name = name.to_string();
                        }
                        names.push(name.to_string());
                    }
                }
                if shelf_kind == "collections" {
                    Ok(json!({ "currentSceneCollectionName": current_name, "sceneCollections": names }))
                } else {
                    Ok(json!({ "currentProfileName": current_name, "profiles": names }))
                }
            }
            "SetCurrentSceneCollection" | "SetCurrentProfile" => {
                let key = if shelf_kind == "collections" { "sceneCollectionName" } else { "profileName" };
                let want = str_field(d, key)?;
                let v = self.studio_shelf(shelf_kind, ShelfAct::List).await.map_err(obs_error)?;
                let id = v["list"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|e| e["name"].as_str() == Some(want) || e["id"].as_str() == Some(want))
                    .and_then(|e| e["id"].as_str())
                    .map(String::from)
                    .ok_or_else(|| fail(ST_NOT_FOUND, "No entry was found by that name."))?;
                self.studio_shelf(shelf_kind, ShelfAct::Switch { id }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "CreateSceneCollection" => {
                let name = str_field(d, "sceneCollectionName")?.trim().to_string();
                self.studio_shelf("collections", ShelfAct::New { name, copy: false }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            _ => Err(fail(ST_UNKNOWN, "Unknown request type.")),
        }
    }

    async fn ows_input_request(&self, kind: &str, d: &Value) -> Result<Value, Fail> {
        match kind {
            "GetInputKindList" => {
                let v = self.studio_kinds().await.map_err(obs_error)?;
                let mut list: Vec<String> = v["inputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|k| k["id"].as_str().or_else(|| k.as_str()).map(String::from))
                    .collect();
                list.sort();
                list.dedup();
                Ok(json!({ "inputKinds": list }))
            }
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
            "GetInputSettings" => {
                let name = self.ows_input_name(d).await?;
                let sources = self.studio_act(StudioAct::Sources).await.map_err(obs_error)?;
                let input_kind = sources
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|s| s["name"].as_str() == Some(name.as_str()))
                    .and_then(|s| s["kind"].as_str())
                    .map(String::from)
                    .ok_or_else(|| fail(ST_NOT_FOUND, "No input was found by that name."))?;
                let settings = self.studio_act(StudioAct::Settings { name }).await.map_err(obs_error)?;
                // Keys, passwords, tokens and URL credentials never leave over obs-websocket.
                Ok(json!({ "inputSettings": redact_source_settings(&settings), "inputKind": input_kind }))
            }
            "SetInputSettings" => {
                let name = self.ows_input_name(d).await?;
                let settings = field(d, "inputSettings")?.clone();
                if !settings.is_object() {
                    return Err(fail(ST_BAD_TYPE, "`inputSettings` must be an object."));
                }
                if let Some(why) = source_settings_problem(&settings) {
                    return Err(fail(ST_OUT_OF_RANGE, &why));
                }
                let overlay = d.get("overlay").and_then(Value::as_bool).unwrap_or(true);
                let final_settings = if overlay {
                    settings
                } else {
                    let cur = self.studio_act(StudioAct::Settings { name: name.clone() }).await.map_err(obs_error)?;
                    let mut merged = serde_json::Map::new();
                    if let Some(obj) = cur.as_object() {
                        for k in obj.keys() {
                            merged.insert(k.clone(), Value::Null);
                        }
                    }
                    if let Some(obj) = settings.as_object() {
                        for (k, v) in obj {
                            merged.insert(k.clone(), v.clone());
                        }
                    }
                    Value::Object(merged)
                };
                self.studio_act(StudioAct::Update { name, settings: final_settings }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "CreateInput" => {
                let scene = self.ows_scene_name(d).await?;
                let name = str_field(d, "inputName")?.to_string();
                let input_kind = str_field(d, "inputKind")?.to_string();
                let settings = d.get("inputSettings").cloned().unwrap_or_else(|| json!({}));
                if let Some(why) = source_settings_problem(&settings) {
                    return Err(fail(ST_OUT_OF_RANGE, &why));
                }
                let enabled = d.get("sceneItemEnabled").and_then(Value::as_bool).unwrap_or(true);
                let added = self
                    .studio_act(StudioAct::AddSource {
                        scene: scene.clone(),
                        source: NewSource { kind: input_kind, name: name.clone(), settings },
                        fit: true,
                    })
                    .await
                    .map_err(obs_error)?;
                let id = added["id"].as_i64().unwrap_or(0);
                if !enabled && id != 0 {
                    self.studio_act(StudioAct::EditItem { scene, id, edit: ItemEdit::Hide })
                        .await
                        .map_err(obs_error)?;
                }
                Ok(json!({ "inputUuid": uuid_of(&name), "sceneItemId": id }))
            }
            "SetInputName" => {
                let name = self.ows_input_name(d).await?;
                let new_name = str_field(d, "newInputName")?.to_string();
                self.studio_act(StudioAct::Rename { name, new_name }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "GetInputMute" => {
                let row = self.ows_audio(&self.ows_input_name(d).await?).await?;
                Ok(json!({ "inputMuted": row["muted"].as_bool().unwrap_or(false) }))
            }
            "SetInputMute" | "ToggleInputMute" => {
                let name = self.ows_input_name(d).await?;
                let row = self.ows_audio(&name).await?;
                let now = row["muted"].as_bool().unwrap_or(false);
                let muted = if kind == "SetInputMute" { bool_field(d, "inputMuted")? } else { !now };
                let change = AudioChange { muted: Some(muted), ..Default::default() };
                self.studio_act(StudioAct::SetAudio { name, change }).await.map_err(obs_error)?;
                Ok(if kind == "ToggleInputMute" { json!({ "inputMuted": muted }) } else { Value::Null })
            }
            "GetInputVolume" => {
                let row = self.ows_audio(&self.ows_input_name(d).await?).await?;
                let db = row["volume_db"].as_f64().unwrap_or(0.0);
                Ok(json!({ "inputVolumeMul": mul_of(db), "inputVolumeDb": db }))
            }
            "SetInputVolume" => {
                let name = self.ows_input_name(d).await?;
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
            "GetInputAudioMonitorType" => {
                let row = self.ows_audio(&self.ows_input_name(d).await?).await?;
                let m = row["monitor"].as_str().unwrap_or("off");
                Ok(json!({ "monitorType": monitor_to_obs(m) }))
            }
            "SetInputAudioMonitorType" => {
                let name = self.ows_input_name(d).await?;
                let monitor = obs_to_monitor(str_field(d, "monitorType")?)?;
                self.ows_audio(&name).await?;
                let change = AudioChange { monitor: Some(monitor), ..Default::default() };
                self.studio_act(StudioAct::SetAudio { name, change }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "GetInputAudioSyncOffset" => {
                let row = self.ows_audio(&self.ows_input_name(d).await?).await?;
                let sync_ms = row["sync_ms"].as_i64().unwrap_or(0);
                Ok(json!({ "inputAudioSyncOffset": sync_ms.saturating_mul(1_000_000) }))
            }
            "SetInputAudioSyncOffset" => {
                let name = self.ows_input_name(d).await?;
                let ns = field(d, "inputAudioSyncOffset")?
                    .as_i64()
                    .ok_or_else(|| fail(ST_BAD_TYPE, "`inputAudioSyncOffset` must be an integer (nanoseconds)."))?;
                let ms = ns / 1_000_000;
                if !(-950..=20_000).contains(&ms) {
                    return Err(fail(ST_OUT_OF_RANGE, "`inputAudioSyncOffset` must correspond to -950..20000 ms."));
                }
                self.ows_audio(&name).await?;
                let change = AudioChange { sync_ms: Some(ms), ..Default::default() };
                self.studio_act(StudioAct::SetAudio { name, change }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "GetInputAudioBalance" => {
                let row = self.ows_audio(&self.ows_input_name(d).await?).await?;
                Ok(json!({ "inputAudioBalance": row["balance"].as_f64().unwrap_or(0.5) }))
            }
            "SetInputAudioBalance" => {
                let name = self.ows_input_name(d).await?;
                let balance = num_field(d, "inputAudioBalance")?;
                if !balance.is_finite() || !(0.0..=1.0).contains(&balance) {
                    return Err(fail(ST_OUT_OF_RANGE, "`inputAudioBalance` must be from 0.0 to 1.0."));
                }
                self.ows_audio(&name).await?;
                let change = AudioChange { balance: Some(balance as f32), ..Default::default() };
                self.studio_act(StudioAct::SetAudio { name, change }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "GetInputAudioTracks" => {
                let row = self.ows_audio(&self.ows_input_name(d).await?).await?;
                Ok(json!({ "inputAudioTracks": tracks_to_obs(&row["tracks"]) }))
            }
            "SetInputAudioTracks" => {
                let name = self.ows_input_name(d).await?;
                let tracks = obs_to_tracks(field(d, "inputAudioTracks")?)?;
                self.ows_audio(&name).await?;
                let change = AudioChange { tracks: Some(tracks), ..Default::default() };
                self.studio_act(StudioAct::SetAudio { name, change }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            _ => Err(fail(ST_UNKNOWN, "Unknown request type.")),
        }
    }

    async fn ows_item_request(&self, kind: &str, d: &Value) -> Result<Value, Fail> {
        let scene = self.ows_scene_name(d).await?;
        match kind {
            "GetSceneItemList" => {
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
                let source = self.ows_source_name(d).await?;
                let items = self.ows_items(&scene).await?;
                items
                    .iter()
                    .flat_map(|i| std::iter::once(i).chain(i["children"].as_array().into_iter().flatten()))
                    .find(|i| i["name"].as_str() == Some(source.as_str()))
                    .map(|i| json!({ "sceneItemId": i["id"] }))
                    .ok_or_else(|| fail(ST_NOT_FOUND, "No scene items were found in the specified scene by that name."))
            }
            "CreateSceneItem" => {
                let name = self.ows_source_name(d).await?;
                let enabled = d.get("sceneItemEnabled").and_then(Value::as_bool).unwrap_or(true);
                let added =
                    self.studio_act(StudioAct::AddExisting { scene: scene.clone(), name }).await.map_err(obs_error)?;
                let id = added["id"].as_i64().unwrap_or(0);
                if !enabled && id != 0 {
                    self.studio_act(StudioAct::EditItem { scene, id, edit: ItemEdit::Hide })
                        .await
                        .map_err(obs_error)?;
                }
                Ok(json!({ "sceneItemId": id }))
            }
            "RemoveSceneItem" => {
                let id = Self::ows_item_id(d)?;
                self.ows_find_item(&scene, id).await?;
                self.studio_act(StudioAct::EditItem { scene, id, edit: ItemEdit::Remove }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "DuplicateSceneItem" => {
                let id = Self::ows_item_id(d)?;
                let item = self.ows_find_item(&scene, id).await?;
                let dst = if d.get("destinationSceneName").is_some() || d.get("destinationSceneUuid").is_some() {
                    let sub = json!({
                        "sceneName": d.get("destinationSceneName").cloned().unwrap_or(Value::Null),
                        "sceneUuid": d.get("destinationSceneUuid").cloned().unwrap_or(Value::Null),
                    });
                    self.ows_scene_name(&sub).await?
                } else {
                    scene.clone()
                };
                if dst == scene {
                    let v = self
                        .studio_act(StudioAct::EditItem { scene, id, edit: ItemEdit::Duplicate })
                        .await
                        .map_err(obs_error)?;
                    let new_id = v["id"].as_i64().unwrap_or(id);
                    Ok(json!({ "sceneItemId": new_id }))
                } else {
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    let v = self.studio_act(StudioAct::AddExisting { scene: dst, name }).await.map_err(obs_error)?;
                    Ok(json!({ "sceneItemId": v["id"].as_i64().unwrap_or(0) }))
                }
            }
            "GetSceneItemTransform" => {
                let id = Self::ows_item_id(d)?;
                let v = self.studio_act(StudioAct::Transform { scene, id }).await.map_err(obs_error)?;
                let t: ItemTransform =
                    serde_json::from_value(v).map_err(|_| fail(ST_FAILED, "Could not read item transform."))?;
                Ok(json!({ "sceneItemTransform": transform_to_obs(&t) }))
            }
            "SetSceneItemTransform" => {
                let id = Self::ows_item_id(d)?;
                let patch = field(d, "sceneItemTransform")?;
                if !patch.is_object() {
                    return Err(fail(ST_BAD_TYPE, "`sceneItemTransform` must be an object."));
                }
                let v = self.studio_act(StudioAct::Transform { scene: scene.clone(), id }).await.map_err(obs_error)?;
                let mut t: ItemTransform =
                    serde_json::from_value(v).map_err(|_| fail(ST_FAILED, "Could not read current item transform."))?;
                patch_transform(&mut t, patch)?;
                t.validate().map_err(obs_error)?;
                self.studio_act(StudioAct::SetTransform { scene, id, transform: t }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "GetSceneItemEnabled" => {
                let item = self.ows_find_item(&scene, Self::ows_item_id(d)?).await?;
                Ok(json!({ "sceneItemEnabled": item["visible"].as_bool().unwrap_or(true) }))
            }
            "SetSceneItemEnabled" => {
                let id = Self::ows_item_id(d)?;
                let on = bool_field(d, "sceneItemEnabled")?;
                self.ows_find_item(&scene, id).await?;
                let edit = if on { ItemEdit::Show } else { ItemEdit::Hide };
                self.studio_act(StudioAct::EditItem { scene, id, edit }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "GetSceneItemLocked" => {
                let item = self.ows_find_item(&scene, Self::ows_item_id(d)?).await?;
                Ok(json!({ "sceneItemLocked": item["locked"].as_bool().unwrap_or(false) }))
            }
            "SetSceneItemLocked" => {
                let id = Self::ows_item_id(d)?;
                let on = bool_field(d, "sceneItemLocked")?;
                self.ows_find_item(&scene, id).await?;
                let edit = if on { ItemEdit::Lock } else { ItemEdit::Unlock };
                self.studio_act(StudioAct::EditItem { scene, id, edit }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "GetSceneItemIndex" => {
                let id = Self::ows_item_id(d)?;
                let items = self.ows_items(&scene).await?;
                let n = items.len();
                let pos = items
                    .iter()
                    .position(|i| i["id"].as_i64() == Some(id))
                    .ok_or_else(|| fail(ST_NOT_FOUND, "No scene item was found by that id."))?;
                Ok(json!({ "sceneItemIndex": n - 1 - pos }))
            }
            "SetSceneItemIndex" => {
                let id = Self::ows_item_id(d)?;
                let idx_raw = field(d, "sceneItemIndex")?
                    .as_u64()
                    .ok_or_else(|| fail(ST_BAD_TYPE, "`sceneItemIndex` must be a non-negative integer."))?;
                let items = self.ows_items(&scene).await?;
                let n = items.len();
                let idx =
                    usize::try_from(idx_raw).map_err(|_| fail(ST_OUT_OF_RANGE, "`sceneItemIndex` out of range."))?;
                if idx >= n {
                    return Err(fail(ST_OUT_OF_RANGE, "`sceneItemIndex` exceeds scene item count."));
                }
                if !items.iter().any(|i| i["id"].as_i64() == Some(id)) {
                    return Err(fail(ST_NOT_FOUND, "No scene item was found by that id."));
                }
                let to = n - 1 - idx;
                self.studio_act(StudioAct::ItemOrder { scene, id, to }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            _ => Err(fail(ST_UNKNOWN, "Unknown request type.")),
        }
    }

    async fn ows_filter_request(&self, kind: &str, d: &Value) -> Result<Value, Fail> {
        let name = self.ows_source_name(d).await?;
        match kind {
            "GetSourceFilterList" => {
                let v = self.studio_act(StudioAct::Filters { name, item: None }).await.map_err(obs_error)?;
                let list: Vec<Value> = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .map(|(i, f)| {
                        json!({
                            "filterEnabled": f["enabled"].as_bool().unwrap_or(false),
                            "filterIndex": i,
                            "filterKind": f["kind"],
                            "filterName": f["name"],
                            "filterSettings": {},
                        })
                    })
                    .collect();
                Ok(json!({ "filters": list }))
            }
            "GetSourceFilter" => {
                let filter = str_field(d, "filterName")?.to_string();
                let v =
                    self.studio_act(StudioAct::Filters { name: name.clone(), item: None }).await.map_err(obs_error)?;
                let (idx, info) = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .find(|(_, f)| f["name"].as_str() == Some(filter.as_str()))
                    .ok_or_else(|| fail(ST_NOT_FOUND, "No filter was found by that name."))?;
                let settings = self
                    .studio_act(StudioAct::FilterSettings { name, filter: filter.clone(), item: None })
                    .await
                    .unwrap_or_else(|_| json!({}));
                Ok(json!({
                    "filterEnabled": info["enabled"].as_bool().unwrap_or(false),
                    "filterIndex": idx,
                    "filterKind": info["kind"],
                    "filterName": filter,
                    "filterSettings": settings,
                }))
            }
            "CreateSourceFilter" => {
                let filter = str_field(d, "filterName")?.to_string();
                let filter_kind = str_field(d, "filterKind")?.to_string();
                let settings = d.get("filterSettings").cloned().unwrap_or_else(|| json!({}));
                self.studio_act(StudioAct::AddFilter {
                    name,
                    filter: NewSource { kind: filter_kind, name: filter, settings },
                    item: None,
                })
                .await
                .map_err(obs_error)?;
                Ok(Value::Null)
            }
            "RemoveSourceFilter" => {
                let filter = str_field(d, "filterName")?.to_string();
                self.studio_act(StudioAct::RemoveFilter { name, filter, item: None }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "SetSourceFilterEnabled" => {
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
            "SetSourceFilterSettings" => {
                let filter = str_field(d, "filterName")?.to_string();
                let settings = field(d, "filterSettings")?.clone();
                if !settings.is_object() {
                    return Err(fail(ST_BAD_TYPE, "`filterSettings` must be an object."));
                }
                self.studio_act(StudioAct::UpdateFilter { name, filter, settings, item: None })
                    .await
                    .map_err(obs_error)?;
                Ok(Value::Null)
            }
            "SetSourceFilterIndex" => {
                let filter = str_field(d, "filterName")?.to_string();
                let target_raw = field(d, "filterIndex")?
                    .as_u64()
                    .ok_or_else(|| fail(ST_BAD_TYPE, "`filterIndex` must be a non-negative integer."))?;
                let list =
                    self.studio_act(StudioAct::Filters { name: name.clone(), item: None }).await.map_err(obs_error)?;
                let arr = list.as_array().map(Vec::as_slice).unwrap_or_default();
                let target =
                    usize::try_from(target_raw).map_err(|_| fail(ST_OUT_OF_RANGE, "`filterIndex` out of range."))?;
                if target >= arr.len() {
                    return Err(fail(ST_OUT_OF_RANGE, "`filterIndex` exceeds filter count."));
                }
                let Some(mut cur) = arr.iter().position(|f| f["name"].as_str() == Some(filter.as_str())) else {
                    return Err(fail(ST_NOT_FOUND, "No filter was found by that name."));
                };
                if cur != target {
                    if target == 0 {
                        self.studio_act(StudioAct::EditFilter { name, filter, edit: FilterEdit::Top, item: None })
                            .await
                            .map_err(obs_error)?;
                    } else if target + 1 == arr.len() {
                        self.studio_act(StudioAct::EditFilter { name, filter, edit: FilterEdit::Bottom, item: None })
                            .await
                            .map_err(obs_error)?;
                    } else {
                        while cur < target {
                            self.studio_act(StudioAct::EditFilter {
                                name: name.clone(),
                                filter: filter.clone(),
                                edit: FilterEdit::Down,
                                item: None,
                            })
                            .await
                            .map_err(obs_error)?;
                            cur += 1;
                        }
                        while cur > target {
                            self.studio_act(StudioAct::EditFilter {
                                name: name.clone(),
                                filter: filter.clone(),
                                edit: FilterEdit::Up,
                                item: None,
                            })
                            .await
                            .map_err(obs_error)?;
                            cur -= 1;
                        }
                    }
                }
                Ok(Value::Null)
            }
            _ => Err(fail(ST_UNKNOWN, "Unknown request type.")),
        }
    }

    async fn ows_media_or_shot_request(&self, kind: &str, d: &Value) -> Result<Value, Fail> {
        match kind {
            "GetMediaInputStatus" => {
                let name = self.ows_input_name(d).await?;
                let v =
                    self.studio_act(StudioAct::Media { name, what: None, seek_ms: None }).await.map_err(obs_error)?;
                let st = v["state"].as_str().unwrap_or("none");
                Ok(json!({
                    "mediaState": media_state_to_obs(st),
                    "mediaDuration": v["duration_ms"],
                    "mediaCursor": v["time_ms"],
                }))
            }
            "TriggerMediaInputAction" => {
                let name = self.ows_input_name(d).await?;
                let act = obs_to_media_act(str_field(d, "mediaAction")?)?;
                self.studio_act(StudioAct::Media { name, what: Some(act), seek_ms: None }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "SetMediaInputCursor" => {
                let name = self.ows_input_name(d).await?;
                let cursor = field(d, "mediaCursor")?
                    .as_i64()
                    .ok_or_else(|| fail(ST_BAD_TYPE, "`mediaCursor` must be an integer."))?;
                if cursor < 0 {
                    return Err(fail(ST_OUT_OF_RANGE, "`mediaCursor` must be >= 0."));
                }
                self.studio_act(StudioAct::Media { name, what: None, seek_ms: Some(cursor) })
                    .await
                    .map_err(obs_error)?;
                Ok(Value::Null)
            }
            "GetSourceScreenshot" => {
                let name = self.ows_source_name(d).await?;
                let fmt = str_field(d, "imageFormat")?;
                if !fmt.eq_ignore_ascii_case("png") {
                    return Err(fail(ST_OUT_OF_RANGE, "Only PNG screenshots (`png`) are supported."));
                }
                let width = d
                    .get("imageWidth")
                    .and_then(Value::as_u64)
                    .and_then(|w| u32::try_from(w).ok())
                    .unwrap_or(640)
                    .clamp(64, 640);
                let v = self
                    .studio_act(StudioAct::SourcePicture { name, width, filters: true, item: None })
                    .await
                    .map_err(obs_error)?;
                Ok(json!({ "imageData": v["image"] }))
            }
            "SaveSourceScreenshot" => {
                let name = self.ows_source_name(d).await?;
                self.studio_act(StudioAct::Screenshot { name }).await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            _ => Err(fail(ST_UNKNOWN, "Unknown request type.")),
        }
    }

    async fn ows_stream_stoppable(&self) -> Result<bool, Fail> {
        let stats = self.ows_stats().await?;
        if stats["stream"]["active"].as_bool() == Some(true) {
            return Ok(true);
        }
        if let Ok(host) = self.studio_host() {
            if host.delay_drain().is_some() {
                return Ok(true);
            }
            if host.stream_stats().await.ok().is_some_and(|v| !v.is_null()) {
                return Ok(true);
            }
        }
        let show_active = {
            let m = self.studio().machine.lock().unwrap_or_else(|p| p.into_inner());
            matches!(
                m.phase,
                crate::studio::show::BroadcastPhase::Preparing
                    | crate::studio::show::BroadcastPhase::Starting
                    | crate::studio::show::BroadcastPhase::Countdown
                    | crate::studio::show::BroadcastPhase::Live
            ) || (m.phase == crate::studio::show::BroadcastPhase::Ending && m.stop_waiting)
        };
        Ok(show_active)
    }

    async fn ows_set_output(&self, which: &str, on: bool) -> Result<Value, Fail> {
        if which == "stream" {
            if on {
                let stats = self.ows_stats().await?;
                if stats["stream"]["active"].as_bool() == Some(true) {
                    return Err(fail(ST_RUNNING, "The output is already running."));
                }
                self.go_live().await.map_err(obs_error)?;
            } else {
                if !self.ows_stream_stoppable().await? {
                    return Err(fail(ST_NOT_RUNNING, "The output is not running."));
                }
                let had_drain = self.studio_host().ok().is_some_and(|h| h.delay_drain().is_some());
                self.end_live().await.map_err(obs_error)?;
                if had_drain && let Ok(host) = self.studio_host() {
                    host.discard_delay().await;
                }
            }
            return Ok(Value::Null);
        }
        let stats = self.ows_stats().await?;
        let now = stats[which]["active"].as_bool().unwrap_or(false);
        if on == now {
            return Err(fail(
                if on { ST_RUNNING } else { ST_NOT_RUNNING },
                if on { "The output is already running." } else { "The output is not running." },
            ));
        }
        match which {
            "record" => self.studio_record(on).await.map(drop),
            "replay" => self.studio_replay(on).await.map(drop),
            _ => self.studio_virtual_camera(on, None).await.map(drop),
        }
        .map_err(obs_error)?;
        Ok(Value::Null)
    }

    async fn ows_toggle_output(&self, which: &str) -> Result<Value, Fail> {
        let now = if which == "stream" {
            self.ows_stream_stoppable().await?
        } else {
            self.ows_stats().await?[which]["active"].as_bool().unwrap_or(false)
        };
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
                let stats = self.ows_stats().await?;
                let e = &stats["engine"];
                Ok(json!({
                    "cpuUsage": e["cpu"].as_f64().unwrap_or(0.0),
                    "memoryUsage": e["memory_mb"].as_f64().unwrap_or(0.0),
                    "availableDiskSpace": stats["disk_free_mb"].as_f64().unwrap_or(0.0),
                    "activeFps": e["fps"].as_f64().unwrap_or(0.0),
                    "averageFrameRenderTime": e["frame_ms"].as_f64().unwrap_or(0.0),
                    "renderSkippedFrames": e["lagged"].as_u64().unwrap_or(0),
                    "renderTotalFrames": e["frames"].as_u64().unwrap_or(0),
                    "outputSkippedFrames": stats["stream"]["dropped"].as_u64().unwrap_or(0),
                    "outputTotalFrames": stats["stream"]["frames"].as_u64().unwrap_or(0),
                    "webSocketSessionIncomingMessages": 0,
                    "webSocketSessionOutgoingMessages": 0,
                }))
            }
            "GetHotkeyList" => Ok(json!({ "hotkeys": HOTKEY_ACTIONS })),
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
            "CreateScene" => {
                let name = str_field(d, "sceneName")?.trim().to_string();
                let scenes = self.ows_scenes().await?;
                if scenes["scenes"].as_array().into_iter().flatten().any(|x| x["name"].as_str() == Some(name.as_str()))
                {
                    return Err(fail(ST_FAILED, "A scene with that name already exists."));
                }
                self.studio_act(StudioAct::AddScene { name: name.clone(), vertical: false })
                    .await
                    .map_err(obs_error)?;
                Ok(json!({ "sceneUuid": uuid_of(&name) }))
            }
            "RemoveScene" => {
                let scene = self.ows_scene_name(d).await?;
                self.studio_act(StudioAct::RemoveScene { scene }).await.map_err(obs_error)?;
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
            // ---- transitions & T-bar ----
            "GetTransitionKindList"
            | "GetCurrentSceneTransition"
            | "SetCurrentSceneTransition"
            | "SetCurrentSceneTransitionDuration"
            | "SetCurrentSceneTransitionSettings"
            | "SetTBarPosition" => self.ows_transition_request(kind, d).await,
            // ---- scene collections & profiles ----
            "GetSceneCollectionList"
            | "SetCurrentSceneCollection"
            | "CreateSceneCollection"
            | "GetProfileList"
            | "SetCurrentProfile" => self.ows_shelf_request(kind, d).await,
            // ---- inputs & audio ----
            "GetInputKindList"
            | "GetInputList"
            | "GetInputSettings"
            | "SetInputSettings"
            | "CreateInput"
            | "SetInputName"
            | "GetInputMute"
            | "SetInputMute"
            | "ToggleInputMute"
            | "GetInputVolume"
            | "SetInputVolume"
            | "GetInputAudioMonitorType"
            | "SetInputAudioMonitorType"
            | "GetInputAudioSyncOffset"
            | "SetInputAudioSyncOffset"
            | "GetInputAudioBalance"
            | "SetInputAudioBalance"
            | "GetInputAudioTracks"
            | "SetInputAudioTracks" => self.ows_input_request(kind, d).await,
            // ---- scene items ----
            "GetSceneItemList"
            | "GetSceneItemId"
            | "CreateSceneItem"
            | "RemoveSceneItem"
            | "DuplicateSceneItem"
            | "GetSceneItemTransform"
            | "SetSceneItemTransform"
            | "GetSceneItemEnabled"
            | "SetSceneItemEnabled"
            | "GetSceneItemLocked"
            | "SetSceneItemLocked"
            | "GetSceneItemIndex"
            | "SetSceneItemIndex" => self.ows_item_request(kind, d).await,
            // ---- stream ----
            "GetStreamStatus" => {
                let s = self.ows_stats().await?;
                let st = &s["stream"];
                let secs = st["seconds"].as_f64().unwrap_or(0.0);
                Ok(json!({
                    "outputActive": st["active"].as_bool().unwrap_or(false),
                    "outputReconnecting": st["reconnecting"].as_bool().unwrap_or(false),
                    "outputTimecode": timecode(secs), "outputDuration": (secs * 1000.0) as u64,
                    "outputCongestion": 0.0,
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
            "SplitRecordFile" => {
                if self.ows_stats().await?["record"]["active"].as_bool() != Some(true) {
                    return Err(fail(ST_NOT_RUNNING, "The record output is not running."));
                }
                self.studio_record_split().await.map_err(obs_error)?;
                Ok(Value::Null)
            }
            "CreateRecordChapter" => {
                if self.ows_stats().await?["record"]["active"].as_bool() != Some(true) {
                    return Err(fail(ST_NOT_RUNNING, "The record output is not running."));
                }
                let chapter = d.get("chapterName").and_then(Value::as_str).unwrap_or_default().to_string();
                self.studio_record_chapter(chapter).await.map_err(obs_error)?;
                Ok(Value::Null)
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
            "GetSourceFilterList"
            | "GetSourceFilter"
            | "CreateSourceFilter"
            | "RemoveSourceFilter"
            | "SetSourceFilterEnabled"
            | "SetSourceFilterSettings"
            | "SetSourceFilterIndex" => self.ows_filter_request(kind, d).await,
            // ---- media & screenshots ----
            "GetMediaInputStatus"
            | "TriggerMediaInputAction"
            | "SetMediaInputCursor"
            | "GetSourceScreenshot"
            | "SaveSourceScreenshot" => self.ows_media_or_shot_request(kind, d).await,
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
        let host = self.studio_host().ok()?;
        if let Some(res) = host.obsws_snapshot().await {
            let raw = res.ok()?;
            let program = raw["program"].as_str().unwrap_or_default().to_string();
            let preview = raw["preview"].as_str().map(String::from);
            let mut snap = Snap {
                program,
                studio_mode: preview.is_some(),
                preview,
                scenes: raw["scenes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect(),
                transition: (
                    raw["transition"]["kind"].as_str().unwrap_or_default().to_string(),
                    raw["transition"]["ms"].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0),
                ),
                stream: raw["stream"].as_bool().unwrap_or(false) && self.golive_main_failed().is_none(),
                record: raw["record"]["active"].as_bool().unwrap_or(false),
                record_paused: raw["record"]["paused"].as_bool().unwrap_or(false),
                record_file: raw["record"]["file"].as_str().unwrap_or_default().to_string(),
                replay: raw["replay"]["active"].as_bool().unwrap_or(false),
                replay_last: raw["replay"]["last"].as_str().map(String::from),
                vcam: raw["vcam"].as_bool().unwrap_or(false),
                ..Default::default()
            };
            for a in raw["inputs"].as_array().into_iter().flatten() {
                if let Some(n) = a["name"].as_str() {
                    snap.inputs.insert(
                        n.to_string(),
                        (a["muted"].as_bool().unwrap_or(false), a["volume_db"].as_f64().unwrap_or(0.0)),
                    );
                }
            }
            for sc in raw["scene_items"].as_array().into_iter().flatten() {
                let Some(scene) = sc["scene"].as_str() else { continue };
                for i in sc["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|i| std::iter::once(i).chain(i["children"].as_array().into_iter().flatten()))
                {
                    if let Some(id) = i["id"].as_i64() {
                        snap.items.insert((scene.to_string(), id), i["visible"].as_bool().unwrap_or(true));
                        snap.item_locks.insert((scene.to_string(), id), i["locked"].as_bool().unwrap_or(false));
                    }
                }
            }
            return Some(snap);
        }
        let scenes = self.studio_act(StudioAct::Scenes).await.ok()?;
        let stats = self.studio_stats().await.ok()?;
        let program = scenes["current"].as_str().unwrap_or_default().to_string();
        let preview = scenes["preview"].as_str().map(String::from);
        let mut snap = Snap {
            program: program.clone(),
            studio_mode: preview.is_some(),
            preview: preview.clone(),
            scenes: scenes["scenes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s["name"].as_str().map(String::from))
                .collect(),
            transition: transition_parts(&scenes),
            stream: stats["stream"]["active"].as_bool().unwrap_or(false),
            record: stats["record"]["active"].as_bool().unwrap_or(false),
            record_paused: stats["record"]["paused"].as_bool().unwrap_or(false),
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
                        snap.items.insert((program.clone(), id), i["visible"].as_bool().unwrap_or(true));
                        snap.item_locks.insert((program.clone(), id), i["locked"].as_bool().unwrap_or(false));
                    }
                }
            }
        }
        Some(snap)
    }
}

fn ows_mutates(kind: &str) -> bool {
    matches!(
        kind,
        "SetCurrentProgramScene"
            | "SetCurrentPreviewScene"
            | "CreateScene"
            | "RemoveScene"
            | "SetStudioModeEnabled"
            | "TriggerStudioModeTransition"
            | "SetCurrentSceneTransition"
            | "SetCurrentSceneTransitionDuration"
            | "SetCurrentSceneTransitionSettings"
            | "SetTBarPosition"
            | "SetCurrentSceneCollection"
            | "CreateSceneCollection"
            | "SetCurrentProfile"
            | "SetInputSettings"
            | "CreateInput"
            | "SetInputName"
            | "SetInputMute"
            | "ToggleInputMute"
            | "SetInputVolume"
            | "SetInputAudioMonitorType"
            | "SetInputAudioSyncOffset"
            | "SetInputAudioBalance"
            | "SetInputAudioTracks"
            | "CreateSceneItem"
            | "RemoveSceneItem"
            | "DuplicateSceneItem"
            | "SetSceneItemTransform"
            | "SetSceneItemEnabled"
            | "SetSceneItemLocked"
            | "SetSceneItemIndex"
            | "StartStream"
            | "StopStream"
            | "ToggleStream"
            | "StartRecord"
            | "StopRecord"
            | "ToggleRecord"
            | "PauseRecord"
            | "ResumeRecord"
            | "ToggleRecordPause"
            | "SplitRecordFile"
            | "CreateRecordChapter"
            | "StartReplayBuffer"
            | "StopReplayBuffer"
            | "ToggleReplayBuffer"
            | "SaveReplayBuffer"
            | "StartVirtualCam"
            | "StopVirtualCam"
            | "ToggleVirtualCam"
            | "CreateSourceFilter"
            | "RemoveSourceFilter"
            | "SetSourceFilterEnabled"
            | "SetSourceFilterSettings"
            | "SetSourceFilterIndex"
            | "TriggerMediaInputAction"
            | "SetMediaInputCursor"
    )
}

/// What the Studio looked like at one look.
#[derive(Debug, Default, Clone, PartialEq)]
struct Snap {
    program: String,
    preview: Option<String>,
    scenes: Vec<String>,
    transition: (String, u32),
    studio_mode: bool,
    stream: bool,
    record: bool,
    record_paused: bool,
    record_file: String,
    replay: bool,
    replay_last: Option<String>,
    vcam: bool,
    /// name → (muted, dB), across all audio inputs.
    inputs: BTreeMap<String, (bool, f64)>,
    /// (scene, item id) → shown, for program and Studio Mode preview scenes.
    items: BTreeMap<(String, i64), bool>,
    /// (scene, item id) → locked.
    item_locks: BTreeMap<(String, i64), bool>,
}

fn event(bit: u64, kind: &str, data: Value) -> (u64, String) {
    (bit, json!({ "op": 5, "d": { "eventType": kind, "eventIntent": bit, "eventData": data } }).to_string())
}

fn output_event(kind: &str, active: bool, mut extra: Value) -> (u64, String) {
    let state = if active { "OBS_WEBSOCKET_OUTPUT_STARTED" } else { "OBS_WEBSOCKET_OUTPUT_STOPPED" };
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
    for ((scene, id), on) in &is.items {
        if was.items.get(&(scene.clone(), *id)).is_some_and(|w| w != on) {
            out.push(event(
                SUB_SCENE_ITEMS,
                "SceneItemEnableStateChanged",
                json!({ "sceneName": scene, "sceneUuid": uuid_of(scene), "sceneItemId": id, "sceneItemEnabled": on }),
            ));
        }
    }
    for ((scene, id), locked) in &is.item_locks {
        if was.item_locks.get(&(scene.clone(), *id)).is_some_and(|w| w != locked) {
            out.push(event(
                SUB_SCENE_ITEMS,
                "SceneItemLockStateChanged",
                json!({ "sceneName": scene, "sceneUuid": uuid_of(scene), "sceneItemId": id, "sceneItemLocked": locked }),
            ));
        }
    }
    if !was.scenes.is_empty() && !is.scenes.is_empty() && was.scenes != is.scenes {
        for s in &is.scenes {
            if !was.scenes.contains(s) {
                out.push(event(
                    SUB_SCENES,
                    "SceneCreated",
                    json!({ "sceneName": s, "sceneUuid": uuid_of(s), "isGroup": false }),
                ));
            }
        }
        for s in &was.scenes {
            if !is.scenes.contains(s) {
                out.push(event(
                    SUB_SCENES,
                    "SceneRemoved",
                    json!({ "sceneName": s, "sceneUuid": uuid_of(s), "isGroup": false }),
                ));
            }
        }
        let scenes_list: Vec<Value> = is
            .scenes
            .iter()
            .enumerate()
            .rev()
            .map(|(i, n)| json!({ "sceneIndex": i, "sceneName": n, "sceneUuid": uuid_of(n) }))
            .collect();
        out.push(event(SUB_SCENES, "SceneListChanged", json!({ "scenes": scenes_list })));
    }
    if !was.transition.0.is_empty() && !is.transition.0.is_empty() {
        if was.transition.0 != is.transition.0 {
            out.push(event(
                SUB_TRANSITIONS,
                "CurrentSceneTransitionChanged",
                json!({ "transitionName": is.transition.0, "transitionUuid": uuid_of(&is.transition.0) }),
            ));
        }
        if was.transition.1 != is.transition.1 {
            out.push(event(
                SUB_TRANSITIONS,
                "CurrentSceneTransitionDurationChanged",
                json!({ "transitionDuration": is.transition.1 }),
            ));
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
    } else if is.record && was.record_paused != is.record_paused {
        let state = if is.record_paused { "OBS_WEBSOCKET_OUTPUT_PAUSED" } else { "OBS_WEBSOCKET_OUTPUT_RESUMED" };
        out.push(event(
            SUB_OUTPUTS,
            "RecordStateChanged",
            json!({ "outputActive": true, "outputState": state, "outputPath": Value::Null }),
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

/// A source's settings as an obs-websocket client may read them: keys, passwords, tokens and
/// the credentials or secret path parts of a URL are replaced (the same settings stay whole for
/// the owner in the dashboard).
fn redact_source_settings(v: &Value) -> Value {
    const SECRET_KEY_PARTS: &[&str] = &["key", "password", "passwd", "secret", "token", "auth", "credential", "bearer"];
    fn secret_key(k: &str) -> bool {
        let k = k.to_ascii_lowercase();
        SECRET_KEY_PARTS.iter().any(|p| k.contains(p))
    }
    fn scrub_text(s: &str) -> String {
        let mut out = s.to_string();
        // user:password@ in a URL.
        if let Some(i) = out.find("://") {
            let rest = &out[i + 3..];
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            if let Some(at) = rest[..end].rfind('@') {
                out = format!("{}://{}", &out[..i], &rest[at + 1..]);
            }
        }
        // ?token=…&… : the query's values go.
        if let Some(q) = out.find('?') {
            let (head, query) = out.split_at(q);
            let kept: Vec<String> = query[1..]
                .split('&')
                .map(|p| match p.split_once('=') {
                    Some((k, _)) if secret_key(k) => format!("{k}=***"),
                    _ => p.to_string(),
                })
                .collect();
            out = format!("{head}?{}", kept.join("&"));
        }
        // Overlay and invite links carry their secret in the path (…/o/<token>/…).
        for marker in ["/o/", "/guest/", "/join/", "/invite/"] {
            if let Some(i) = out.find(marker) {
                let start = i + marker.len();
                let end = out[start..].find(['/', '?', '#']).map_or(out.len(), |e| start + e);
                out.replace_range(start..end, "***");
            }
        }
        // rtmp://host/app/<key>: the last path part of a stream address.
        if (out.starts_with("rtmp://") || out.starts_with("rtmps://")) && out.matches('/').count() >= 4 {
            if let Some(i) = out.rfind('/') {
                out.replace_range(i + 1.., "***");
            }
        }
        out
    }
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        if secret_key(k) && !v.is_object() && !v.is_array() {
                            json!("***")
                        } else {
                            redact_source_settings(v)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact_source_settings).collect()),
        Value::String(s) => Value::String(scrub_text(s)),
        other => other.clone(),
    }
}

/// Why settings for a new or changed source are refused over obs-websocket: a local file read
/// through a URL or a text file switch (a picture of it could be read back with a screenshot).
fn source_settings_problem(v: &Value) -> Option<String> {
    match v {
        Value::Object(m) => {
            if m.get("read_from_file").and_then(Value::as_bool) == Some(true) {
                return Some("Reading a text source from a file isn't allowed from obs-websocket.".into());
            }
            m.values().find_map(source_settings_problem)
        }
        Value::Array(a) => a.iter().find_map(source_settings_problem),
        Value::String(s) if s.trim_start().to_ascii_lowercase().starts_with("file:") => {
            Some("file: addresses aren't allowed in a source's settings from obs-websocket.".into())
        }
        _ => None,
    }
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
    fn input_settings_never_show_keys_passwords_tokens_or_credentials_in_urls() {
        let v = json!({
            "url": "https://u:pw@example.com/a?token=abc&x=1",
            "stream_key": "live_123",
            "password": "p",
            "nested": { "api_key": "k", "auth_header": "Bearer zzz", "text": "hello" },
            "overlay": "http://127.0.0.1:47820/o/abcdef123456/alerts",
            "list": ["rtmp://h/app/KEY123", { "secret": "s" }],
            "width": 1280
        });
        let out = redact_source_settings(&v).to_string();
        for leaked in
            ["u:pw", "token=abc", "live_123", "\"p\"", "\"k\"", "Bearer zzz", "abcdef123456", "KEY123", "\"s\""]
        {
            assert!(!out.contains(leaked), "{leaked} leaked: {out}");
        }
        // What isn't secret is kept.
        assert!(out.contains("hello") && out.contains("1280") && out.contains("example.com"), "{out}");
    }

    #[test]
    fn a_source_cannot_be_pointed_at_local_files_through_obs_websocket() {
        assert!(source_settings_problem(&json!({ "url": "file:///etc/passwd" })).is_some());
        assert!(source_settings_problem(&json!({ "a": { "b": "FILE:///x" } })).is_some());
        assert!(source_settings_problem(&json!({ "read_from_file": true, "file": "/etc/hosts" })).is_some());
        assert!(source_settings_problem(&json!({ "url": "https://example.com", "width": 100 })).is_none());
        assert!(source_settings_problem(&json!({ "text": "hello", "read_from_file": false })).is_none());
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
            items: BTreeMap::from([(("One".into(), 3), true)]),
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
        // An item changing in the same scene is told; switching program scene without that item does not emit.
        let mut c = a.clone();
        c.items.insert(("One".into(), 3), false);
        assert_eq!(diff(&a, &c).len(), 1);
        let mut d = a.clone();
        d.program = "Two".into();
        d.items = BTreeMap::from([(("Two".into(), 3), false)]);
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
        aux: (bool, f64),
        mic_monitor: Monitor,
        mic_sync_ms: i64,
        mic_balance: f32,
        mic_tracks: Vec<u8>,
        cam_visible: bool,
        cam_locked: bool,
        preview_cam_visible: bool,
        cam_transform: ItemTransform,
        filter_on: bool,
        filters: Vec<(String, String, bool, Value)>,
        transition: (String, u32),
        transition_settings: Value,
        tbar: f32,
        input_settings: BTreeMap<String, Value>,
        media_state: (String, i64, i64),
        splits: usize,
        chapters: Vec<String>,
        collections: (String, Vec<(String, String)>),
        profiles: (String, Vec<(String, String)>),
        items_game: Vec<(i64, String, String, bool, bool)>,
        next_item_id: i64,
        stream: bool,
        stream_secs: f64,
        record: bool,
        paused: bool,
        replay: bool,
        replay_last: Option<String>,
        vcam: bool,
        extra_scenes: Vec<String>,
        stats_calls: usize,
        acts: Vec<&'static str>,
        drain: Option<(f64, f64)>,
        discarded: bool,
        slow_act_once: Option<Arc<tokio::sync::Notify>>,
    }

    #[derive(Debug)]
    struct Fake(Mutex<Studio>);

    impl Fake {
        fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(Studio {
                current: "Game".into(),
                preview: None,
                mic: (false, 0.0),
                aux: (false, -3.0),
                mic_monitor: Monitor::Off,
                mic_sync_ms: 0,
                mic_balance: 0.5,
                mic_tracks: vec![1, 2],
                cam_visible: true,
                cam_locked: false,
                preview_cam_visible: true,
                cam_transform: ItemTransform {
                    x: 100.0,
                    y: 50.0,
                    rotation: 0.0,
                    scale_x: 1.0,
                    scale_y: 1.0,
                    alignment: 5,
                    bounds: "none".into(),
                    bounds_alignment: 0,
                    bounds_width: 0.0,
                    bounds_height: 0.0,
                    crop_left: 0,
                    crop_top: 0,
                    crop_right: 0,
                    crop_bottom: 0,
                    width: 1920,
                    height: 1080,
                },
                filter_on: true,
                filters: vec![("Blur".into(), "blur".into(), true, json!({ "radius": 5 }))],
                transition: ("fade_transition".into(), 300),
                transition_settings: json!({}),
                tbar: 0.0,
                input_settings: BTreeMap::from([("Cam".into(), json!({ "device": "FaceTime HD" }))]),
                media_state: ("playing".into(), 1200, 60000),
                splits: 0,
                chapters: Vec::new(),
                collections: ("col-1".into(), vec![("col-1".into(), "Main".into()), ("col-2".into(), "IRL".into())]),
                profiles: (
                    "prof-1".into(),
                    vec![("prof-1".into(), "1080p60".into()), ("prof-2".into(), "720p30".into())],
                ),
                items_game: vec![
                    (7, "Cam".into(), "av_capture_input".into(), true, false),
                    (8, "Logo".into(), "image_source".into(), true, false),
                ],
                next_item_id: 20,
                stream: false,
                stream_secs: 0.0,
                record: false,
                paused: false,
                replay: false,
                replay_last: None,
                vcam: false,
                extra_scenes: Vec::new(),
                stats_calls: 0,
                acts: Vec::new(),
                drain: None,
                discarded: false,
                slow_act_once: None,
            })))
        }
        fn with<R>(&self, f: impl FnOnce(&mut Studio) -> R) -> R {
            f(&mut self.0.lock().unwrap_or_else(|p| p.into_inner()))
        }
    }

    #[async_trait::async_trait]
    impl StudioHost for Fake {
        fn delay_drain(&self) -> Option<(f64, f64)> {
            self.with(|s| s.drain)
        }
        async fn discard_delay(&self) {
            self.with(|s| {
                s.drain = None;
                s.discarded = true;
            });
        }
        async fn status(&self) -> StudioStatus {
            StudioStatus { available: true, running: true, ..Default::default() }
        }
        async fn start(&self) -> Result<StudioStatus, String> {
            Ok(StudioStatus { available: true, running: true, ..Default::default() })
        }
        async fn stop(&self) -> Result<StudioStatus, String> {
            Ok(StudioStatus::default())
        }
        async fn kinds(&self) -> Result<Value, String> {
            Ok(json!({
                "inputs": [
                    { "id": "av_capture_input", "name": "Camera" },
                    { "id": "image_source", "name": "Image" },
                    { "id": "ffmpeg_source", "name": "Media Source" },
                ],
                "filters": [],
                "transitions": [],
                "encoders": [],
                "outputs": [],
            }))
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
            self.with(|s| s.stream = true);
            Ok(json!({ "active": true }))
        }
        async fn stream_stop(&self) -> Result<(), String> {
            self.with(|s| s.stream = false);
            Ok(())
        }
        async fn stream_stats(&self) -> Result<Value, String> {
            Ok(self.with(|s| {
                if s.stream { json!({ "active": true, "seconds": s.stream_secs, "error": null }) } else { Value::Null }
            }))
        }
        async fn obs_collections(&self) -> Result<Value, String> {
            Ok(json!([]))
        }
        async fn import_obs(&self, _i: String) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn obsws_snapshot(&self) -> Option<Result<Value, String>> {
            Some(Ok(self.with(|s| {
                let mut scenes = vec!["Game".to_string(), "BRB".to_string()];
                scenes.extend(s.extra_scenes.iter().cloned());
                let mut scene_items = Vec::new();
                let program_items = if s.current == "BRB" {
                    json!([
                        { "id": 7, "name": "Cam", "visible": s.cam_visible, "locked": s.cam_locked, "children": [] },
                        { "id": 19, "name": "CamPreview", "visible": s.preview_cam_visible, "locked": false, "children": [] },
                    ])
                } else {
                    json!(s.items_game.iter().map(|(id, name, _, vis, locked)| {
                        let visible = if *id == 7 { s.cam_visible } else { *vis };
                        let lck = if *id == 7 { s.cam_locked } else { *locked };
                        json!({ "id": id, "name": name, "visible": visible, "locked": lck, "children": [] })
                    }).collect::<Vec<_>>())
                };
                scene_items.push(json!({ "scene": s.current, "items": program_items }));
                if let Some(p) = &s.preview
                    && p != &s.current
                {
                    let prev_items = if p == "BRB" {
                        json!([
                            { "id": 7, "name": "Cam", "visible": s.cam_visible, "locked": s.cam_locked, "children": [] },
                            { "id": 19, "name": "CamPreview", "visible": s.preview_cam_visible, "locked": false, "children": [] },
                        ])
                    } else {
                        json!([{ "id": 7, "name": "Cam", "visible": s.cam_visible, "locked": s.cam_locked, "children": [] }])
                    };
                    scene_items.push(json!({ "scene": p, "items": prev_items }));
                }
                json!({
                    "program": s.current,
                    "preview": s.preview,
                    "scenes": scenes,
                    "transition": { "kind": s.transition.0, "ms": s.transition.1 },
                    "stream": s.stream,
                    "record": { "active": s.record, "paused": s.paused, "file": "/r/a.mkv" },
                    "replay": { "active": s.replay, "last": s.replay_last },
                    "vcam": s.vcam,
                    "inputs": [
                        { "name": "Mic", "muted": s.mic.0, "volume_db": s.mic.1 },
                        { "name": "Aux", "muted": s.aux.0, "volume_db": s.aux.1 },
                    ],
                    "scene_items": scene_items,
                })
            })))
        }
        async fn stats(&self) -> Result<Value, String> {
            Ok(self.with(|s| {
                s.stats_calls += 1;
                json!({
                    "engine": { "cpu": 3.5, "fps": 30.0, "frame_ms": 4.0, "lagged": 0, "frames": 100, "memory_mb": 200.0 },
                    "stream": { "active": s.stream, "seconds": s.stream_secs, "frames": 5, "bytes": 9, "dropped": 0 },
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
        async fn record_split(&self) -> Result<(), String> {
            self.with(|s| s.splits += 1);
            Ok(())
        }
        async fn record_chapter(&self, name: String) -> Result<(), String> {
            self.with(|s| s.chapters.push(name));
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
        async fn shelf(&self, kind: String, act: ShelfAct) -> Result<Value, String> {
            self.with(|s| {
                let target = if kind == "profiles" { &mut s.profiles } else { &mut s.collections };
                Ok(match act {
                    ShelfAct::List => json!({
                        "current": target.0,
                        "list": target.1.iter().map(|(id, name)| json!({ "id": id, "name": name })).collect::<Vec<_>>(),
                    }),
                    ShelfAct::New { name, .. } => {
                        let id = format!("{kind}-{}", target.1.len() + 1);
                        target.1.push((id.clone(), name));
                        target.0 = id.clone();
                        json!({ "id": id })
                    }
                    ShelfAct::Switch { id } => {
                        target.0 = id;
                        Value::Null
                    }
                    _ => Value::Null,
                })
            })
        }
        async fn act(&self, act: StudioAct) -> Result<Value, String> {
            let slow = self.with(|s| s.slow_act_once.take());
            if let Some(n) = slow {
                n.notify_one();
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
            self.with(|s| {
                let tag = match &act {
                    StudioAct::Scenes => "Scenes",
                    StudioAct::Audio { .. } => "Audio",
                    StudioAct::AudioByName { .. } => "AudioByName",
                    StudioAct::Items { .. } => "Items",
                    StudioAct::Sources => "Sources",
                    StudioAct::SetAudio { .. } => "SetAudio",
                    _ => "Other",
                };
                s.acts.push(tag);
                Ok(match act {
                    StudioAct::Scenes => {
                        let mut scenes = vec![json!({ "name": "Game" }), json!({ "name": "BRB" })];
                        for name in &s.extra_scenes {
                            scenes.push(json!({ "name": name }));
                        }
                        json!({
                            "scenes": scenes,
                            "current": s.current,
                            "preview": s.preview,
                            "transition": { "kind": s.transition.0, "ms": s.transition.1 },
                        })
                    }
                    StudioAct::AddScene { name, .. } => {
                        s.extra_scenes.push(name);
                        Value::Null
                    }
                    StudioAct::RemoveScene { scene } => {
                        s.extra_scenes.retain(|n| n != &scene);
                        Value::Null
                    }
                    StudioAct::SetTransition { kind, ms } => {
                        s.transition = (kind, ms);
                        Value::Null
                    }
                    StudioAct::TransitionSettings { .. } => s.transition_settings.clone(),
                    StudioAct::SetTransitionSettings { settings, .. } => {
                        s.transition_settings = settings;
                        Value::Null
                    }
                    StudioAct::TBar { at, release } => {
                        s.tbar = if release { 0.0 } else { at };
                        json!({ "active": !release, "at": s.tbar })
                    }
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
                    StudioAct::Items { scene } => {
                        if scene == "BRB" {
                            json!([{ "id": 19, "name": "CamPreview", "kind": "av_capture_input", "visible": s.preview_cam_visible, "locked": false, "group": false, "children": [] }])
                        } else {
                            json!(s.items_game.iter().map(|(id, name, kind, vis, locked)| {
                                let visible = if *id == 7 { s.cam_visible } else { *vis };
                                let lck = if *id == 7 { s.cam_locked } else { *locked };
                                json!({ "id": id, "name": name, "kind": kind, "visible": visible, "locked": lck, "group": false, "children": [] })
                            }).collect::<Vec<_>>())
                        }
                    }
                    StudioAct::AddSource { source, .. } => {
                        let id = s.next_item_id;
                        s.next_item_id += 1;
                        s.input_settings.insert(source.name.clone(), source.settings.clone());
                        s.items_game.insert(0, (id, source.name, source.kind, true, false));
                        json!({ "id": id })
                    }
                    StudioAct::AddExisting { name, .. } => {
                        let id = s.next_item_id;
                        s.next_item_id += 1;
                        s.items_game.insert(0, (id, name, "av_capture_input".into(), true, false));
                        json!({ "id": id })
                    }
                    StudioAct::EditItem { id, edit, .. } => {
                        match edit {
                            ItemEdit::Show | ItemEdit::Hide => {
                                let on = matches!(edit, ItemEdit::Show);
                                if id == 19 {
                                    s.preview_cam_visible = on;
                                } else {
                                    if id == 7 { s.cam_visible = on; }
                                    if let Some(item) = s.items_game.iter_mut().find(|i| i.0 == id) {
                                        item.3 = on;
                                    }
                                }
                            }
                            ItemEdit::Lock | ItemEdit::Unlock => {
                                let locked = matches!(edit, ItemEdit::Lock);
                                if id == 7 { s.cam_locked = locked; }
                                if let Some(item) = s.items_game.iter_mut().find(|i| i.0 == id) {
                                    item.4 = locked;
                                }
                            }
                            ItemEdit::Remove => {
                                s.items_game.retain(|i| i.0 != id);
                            }
                            ItemEdit::Duplicate => {
                                let new_id = s.next_item_id;
                                s.next_item_id += 1;
                                if let Some((_, name, kind, vis, locked)) = s.items_game.iter().find(|i| i.0 == id).cloned() {
                                    s.items_game.insert(0, (new_id, name, kind, vis, locked));
                                }
                                return Ok(json!({ "id": new_id }));
                            }
                            _ => {}
                        }
                        Value::Null
                    }
                    StudioAct::ItemOrder { id, to, .. } => {
                        if let Some(pos) = s.items_game.iter().position(|i| i.0 == id) {
                            let item = s.items_game.remove(pos);
                            let idx = to.min(s.items_game.len());
                            s.items_game.insert(idx, item);
                        }
                        Value::Null
                    }
                    StudioAct::Transform { .. } => serde_json::to_value(&s.cam_transform).unwrap_or(Value::Null),
                    StudioAct::SetTransform { transform, .. } => {
                        s.cam_transform = transform;
                        Value::Null
                    }
                    StudioAct::Settings { name } => s.input_settings.get(&name).cloned().unwrap_or_else(|| json!({})),
                    StudioAct::Update { name, settings } => {
                        s.input_settings.insert(name, settings);
                        Value::Null
                    }
                    StudioAct::Rename { name, new_name } => {
                        if let Some(v) = s.input_settings.remove(&name) {
                            s.input_settings.insert(new_name.clone(), v);
                        }
                        for item in &mut s.items_game {
                            if item.1 == name { item.1 = new_name.clone(); }
                        }
                        Value::Null
                    }
                    StudioAct::Audio { scene } => {
                        if s.extra_scenes.last().is_some_and(|last| last == &scene) {
                            json!([{ "name": "Aux", "muted": s.aux.0, "volume_db": s.aux.1 }])
                        } else {
                            json!([{ "name": "Mic", "muted": s.mic.0, "volume_db": s.mic.1 }])
                        }
                    }
                    StudioAct::AudioByName { name } => match name.as_str() {
                        "Mic" => json!({
                            "name": "Mic",
                            "muted": s.mic.0,
                            "volume_db": s.mic.1,
                            "monitor": s.mic_monitor,
                            "sync_ms": s.mic_sync_ms,
                            "balance": s.mic_balance,
                            "tracks": s.mic_tracks,
                        }),
                        "Aux" => json!({ "name": "Aux", "muted": s.aux.0, "volume_db": s.aux.1 }),
                        other => return Err(format!("There's no source called \"{other}\".")),
                    },
                    StudioAct::SetAudio { name, change } => {
                        let is_aux = name == "Aux";
                        let target = if is_aux { &mut s.aux } else { &mut s.mic };
                        if let Some(m) = change.muted {
                            target.0 = m;
                        }
                        if let Some(v) = change.volume_db {
                            target.1 = f64::from(v);
                        }
                        if !is_aux {
                            if let Some(mon) = change.monitor { s.mic_monitor = mon; }
                            if let Some(sync) = change.sync_ms { s.mic_sync_ms = sync; }
                            if let Some(bal) = change.balance { s.mic_balance = bal; }
                            if let Some(tr) = change.tracks { s.mic_tracks = tr; }
                        }
                        Value::Null
                    }
                    StudioAct::Sources => {
                        let mut out = vec![
                            json!({ "name": "Mic", "kind": "coreaudio_input_capture", "scenes": ["Game"] }),
                            json!({ "name": "Cam", "kind": "av_capture_input", "scenes": ["Game"] }),
                            json!({ "name": "Aux", "kind": "coreaudio_input_capture", "scenes": [] }),
                        ];
                        for name in s.input_settings.keys() {
                            if !matches!(name.as_str(), "Mic" | "Cam" | "Aux") {
                                out.push(json!({ "name": name, "kind": "image_source", "scenes": ["Game"] }));
                            }
                        }
                        json!(out)
                    }
                    StudioAct::Filters { .. } => json!(s.filters.iter().map(|(n, k, on, _)| {
                        json!({ "name": n, "kind": k, "enabled": on })
                    }).collect::<Vec<_>>()),
                    StudioAct::FilterSettings { filter, .. } => s
                        .filters
                        .iter()
                        .find(|f| f.0 == filter)
                        .map(|f| f.3.clone())
                        .unwrap_or_else(|| json!({})),
                    StudioAct::AddFilter { filter, .. } => {
                        s.filters.push((filter.name, filter.kind, true, filter.settings));
                        Value::Null
                    }
                    StudioAct::RemoveFilter { filter, .. } => {
                        s.filters.retain(|f| f.0 != filter);
                        Value::Null
                    }
                    StudioAct::UpdateFilter { filter, settings, .. } => {
                        if let Some(f) = s.filters.iter_mut().find(|f| f.0 == filter) {
                            f.3 = settings;
                        }
                        Value::Null
                    }
                    StudioAct::EditFilter { filter, edit, .. } => {
                        if let Some(pos) = s.filters.iter().position(|f| f.0 == filter) {
                            match edit {
                                FilterEdit::Enable | FilterEdit::Disable => {
                                    let on = matches!(edit, FilterEdit::Enable);
                                    s.filters[pos].2 = on;
                                    if filter == "Blur" { s.filter_on = on; }
                                }
                                FilterEdit::Top => {
                                    let f = s.filters.remove(pos);
                                    s.filters.insert(0, f);
                                }
                                FilterEdit::Bottom => {
                                    let f = s.filters.remove(pos);
                                    s.filters.push(f);
                                }
                                FilterEdit::Up if pos > 0 => s.filters.swap(pos, pos - 1),
                                FilterEdit::Down if pos + 1 < s.filters.len() => s.filters.swap(pos, pos + 1),
                                _ => {}
                            }
                        }
                        Value::Null
                    }
                    StudioAct::Media { what, seek_ms, .. } => {
                        if let Some(ms) = seek_ms {
                            s.media_state.1 = ms;
                        }
                        if let Some(act) = what {
                            s.media_state.0 = match act {
                                MediaAct::Play => "playing".into(),
                                MediaAct::Pause => "paused".into(),
                                MediaAct::Stop => "stopped".into(),
                                MediaAct::Restart => { s.media_state.1 = 0; "playing".into() }
                                _ => "playing".into(),
                            };
                        }
                        json!({ "state": s.media_state.0, "time_ms": s.media_state.1, "duration_ms": s.media_state.2 })
                    }
                    StudioAct::SourcePicture { .. } => json!({ "image": "data:image/png;base64,iVBORw0KGgo=", "width": 1920, "height": 1080 }),
                    StudioAct::Screenshot { .. } => json!({ "file": "/r/shot.png" }),
                    _ => Value::Null,
                })
            })
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
        let unknown = c.req("NotARealObsWsRequest", json!({})).await;
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

    #[tokio::test]
    async fn r1_1_poller_avoids_heavy_stats_and_readonly_requests_do_not_wake_poller() {
        let (_e, fake, port, pw, _dir) = on().await;
        let mut c = Client::open(port).await.unwrap();
        c.identify(&pw, None).await;
        // Let poller run for a couple of intervals without calling GetStats.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            fake.with(|s| s.stats_calls),
            0,
            "poller must use lightweight snapshot, never heavy StudioHost::stats"
        );

        // Mutate external state, then send read-only GetVersion: because POLL >= 1s and
        // GetVersion is read-only, poll_now must NOT be notified by GetVersion.
        fake.with(|s| s.current = "BRB".into());
        c.req("GetVersion", json!({})).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(120), c.ws.next()).await.is_err(),
            "read-only request must not wake the event poller early"
        );
    }

    #[tokio::test]
    async fn r1_2_stream_follows_show_path_stops_draining_and_reports_uptime() {
        let (e, fake, port, pw, _dir) = on().await;
        e.start_golive(0).await;
        // Point ingest to Fake and configure a Show (countdown_s = 15) + live_scene = Game.
        e.patch_show_config(
            json!({
                "starting_scene": "Game",
                "live_scene": "Game",
                "ending_scene": "BRB",
                "countdown_s": 15,
                "start_stream": false
            })
            .as_object()
            .unwrap(),
        )
        .await
        .unwrap();
        let mut c = Client::open(port).await.unwrap();
        c.identify(&pw, None).await;

        let start = c.req("StartStream", json!({})).await;
        assert_eq!(start["requestStatus"]["result"], true, "{start:?}");
        assert!(
            e.show_run().is_some(),
            "StartStream must route through Show / go_live path when Show style is configured"
        );

        // Stream uptime is non-zero when live.
        fake.with(|s| {
            s.stream = true;
            s.stream_secs = 42.5;
        });
        let st = c.req("GetStreamStatus", json!({})).await;
        assert_eq!(st["responseData"]["outputDuration"], 42_500);
        assert_eq!(st["responseData"]["outputTimecode"], "00:00:42.500");

        let stop = c.req("StopStream", json!({})).await;
        assert_eq!(stop["requestStatus"]["result"], true, "{stop:?}");

        // Draining stream delay with `active == false` must still be stoppable by StopStream.
        fake.with(|s| {
            s.stream = false;
            s.drain = Some((8.0, 2.5));
            s.discarded = false;
        });
        let stop_drain = c.req("StopStream", json!({})).await;
        assert_eq!(
            stop_drain["requestStatus"]["result"], true,
            "StopStream must stop a draining delay tail: {stop_drain:?}"
        );
        assert!(fake.with(|s| s.discarded), "draining delay tail must be discarded");
    }

    #[tokio::test]
    async fn r1_3_listener_awaits_old_bind_before_rebind() {
        let (e, _fake, _port, _pw, _dir) = on().await;
        for i in 0..12 {
            let new_pw = e.obsws_regenerate().await.unwrap();
            assert_eq!(new_pw.len(), 24);
            let view = e.obsws_view().await;
            assert!(view.listening, "rebind iteration {i} lost listener: {view:?}");
            assert!(view.error.is_none(), "rebind iteration {i} failed: {view:?}");
            assert_eq!(e.inner.obsws.listening_tasks.load(Ordering::SeqCst), 1, "only one listener alive");
        }
    }

    #[tokio::test]
    async fn r1_4_reidentify_replies_op_2_identified() {
        let (_e, _fake, port, pw, _dir) = on().await;
        let mut c = Client::open(port).await.unwrap();
        assert_eq!(c.identify(&pw, Some(SUB_ALL)).await["op"], 2);
        c.send_raw(json!({ "op": 3, "d": { "eventSubscriptions": SUB_SCENES } })).await;
        let reply = c.json().await;
        assert_eq!(reply["op"], 2, "obs-websocket v5 has no op 3 reply; server must reply op 2 Identified: {reply}");
    }

    #[tokio::test]
    async fn r1_5_audio_lookup_is_single_act_and_batch_stops_on_generation_change() {
        let (e, fake, port, pw, _dir) = on().await;
        fake.with(|s| {
            s.extra_scenes = vec!["S1".into(), "S2".into(), "S3".into(), "S4".into()];
            s.acts.clear();
        });
        let mut c = Client::open(port).await.unwrap();
        c.identify(&pw, None).await;
        let mute = c.req("GetInputMute", json!({ "inputName": "Aux" })).await;
        assert_eq!(mute["requestStatus"]["result"], true, "{mute:?}");
        let audio_scans = fake.with(|s| s.acts.iter().filter(|&&t| t == "Scenes" || t == "Audio").count());
        assert!(audio_scans <= 1, "GetInputMute must look up audio in O(1), saw {audio_scans} scene/audio scans");

        // Start a batch whose first item pauses briefly while we regenerate password.
        let gate = Arc::new(tokio::sync::Notify::new());
        let g = gate.clone();
        fake.with(|s| {
            s.acts.clear();
            s.slow_act_once = Some(g);
        });
        let mut requests = Vec::new();
        for i in 0..20 {
            requests.push(json!({ "requestType": "GetSceneList", "requestId": format!("b{i}") }));
        }
        c.send_raw(json!({ "op": 8, "d": { "requestId": "batch1", "requests": requests } })).await;
        gate.notified().await;
        let _ = e.obsws_regenerate().await.unwrap();
        assert_eq!(c.closed_with().await, Some(CLOSE_INVALIDATED));
        let done = fake.with(|s| s.acts.len());
        assert!(done < 10, "batch must stop when generation bumps, ran {done} acts");
    }

    #[tokio::test]
    async fn r1_6_uuids_record_pause_event_preview_offscene_and_bounded_fails() {
        let (e, fake, port, pw, _dir) = on().await;
        let mut c = Client::open(port).await.unwrap();
        c.identify(&pw, None).await;

        // 1. inputUuid & sourceUuid resolution
        assert_eq!(
            c.req("GetInputMute", json!({ "inputUuid": uuid_of("Mic") })).await["requestStatus"]["result"],
            true
        );
        assert_eq!(
            c.req("GetSourceFilterList", json!({ "sourceUuid": uuid_of("Cam") })).await["requestStatus"]["result"],
            true
        );
        assert_eq!(
            c.req(
                "SetSourceFilterEnabled",
                json!({
                    "sourceUuid": uuid_of("Cam"),
                    "filterName": "Blur",
                    "filterEnabled": false
                })
            )
            .await["requestStatus"]["result"],
            true
        );

        // 2. RecordStateChanged emitted for PAUSED and RESUMED.
        c.req("StartRecord", json!({})).await;
        assert_eq!(c.event("RecordStateChanged").await["outputState"], "OBS_WEBSOCKET_OUTPUT_STARTED");
        c.req("PauseRecord", json!({})).await;
        assert_eq!(c.event("RecordStateChanged").await["outputState"], "OBS_WEBSOCKET_OUTPUT_PAUSED");
        c.req("ResumeRecord", json!({})).await;
        assert_eq!(c.event("RecordStateChanged").await["outputState"], "OBS_WEBSOCKET_OUTPUT_RESUMED");

        // 3. Off-scene input volume/mute & Studio Mode preview item visibility changes emit events.
        c.req("SetStudioModeEnabled", json!({ "studioModeEnabled": true })).await;
        c.req("SetCurrentPreviewScene", json!({ "sceneName": "BRB" })).await;
        assert_eq!(c.event("CurrentPreviewSceneChanged").await["sceneName"], "BRB");
        fake.with(|s| {
            s.aux.0 = true;
            s.preview_cam_visible = false;
        });
        assert_eq!(c.event("InputMuteStateChanged").await["inputName"], "Aux");
        let item_ev = c.event("SceneItemEnableStateChanged").await;
        assert_eq!((item_ev["sceneName"].clone(), item_ev["sceneItemId"].clone()), (json!("BRB"), json!(19)));

        // 4. fails map capped against spoofed LAN floods.
        for i in 0..1500u32 {
            let ip = IpAddr::V4(Ipv4Addr::new(10, 0, ((i >> 8) & 0xff) as u8, (i & 0xff) as u8));
            e.ows_note_fail(ip);
        }
        let map_len = e.inner.obsws.fails.lock().unwrap().len();
        assert!(map_len <= 1024, "fails IP table must be capped, got {map_len}");
    }

    #[tokio::test]
    async fn extended_p3_developer_platform_requests_and_events() {
        let (_e, fake, port, pw, _dir) = on().await;
        let mut c = Client::open(port).await.unwrap();
        c.identify(&pw, None).await;

        // Scenes create / duplicate guard / remove + events
        let created = c.req("CreateScene", json!({ "sceneName": "Outro" })).await;
        assert_eq!(created["requestStatus"]["result"], true);
        assert!(created["responseData"]["sceneUuid"].is_string());
        assert_eq!(c.event("SceneCreated").await["sceneName"], "Outro");
        assert_eq!(c.req("CreateScene", json!({ "sceneName": "Outro" })).await["requestStatus"]["code"], 702);
        assert_eq!(c.req("RemoveScene", json!({ "sceneName": "Outro" })).await["requestStatus"]["result"], true);
        assert_eq!(c.event("SceneRemoved").await["sceneName"], "Outro");

        // Transitions & T-bar
        assert!(
            c.req("GetTransitionKindList", json!({})).await["responseData"]["transitionKinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "fade_transition")
        );
        assert_eq!(c.req("GetCurrentSceneTransition", json!({})).await["responseData"]["transitionDuration"], 300);
        assert_eq!(
            c.req("SetCurrentSceneTransition", json!({ "transitionName": "swipe_transition" })).await["requestStatus"]
                ["result"],
            true
        );
        assert_eq!(c.event("CurrentSceneTransitionChanged").await["transitionName"], "swipe_transition");
        assert_eq!(
            c.req("SetCurrentSceneTransitionDuration", json!({ "transitionDuration": 650 })).await["requestStatus"]["result"],
            true
        );
        assert_eq!(c.event("CurrentSceneTransitionDurationChanged").await["transitionDuration"], 650);
        assert_eq!(
            c.req("SetCurrentSceneTransitionSettings", json!({ "transitionSettings": { "direction": "left" } })).await
                ["requestStatus"]["result"],
            true
        );
        assert_eq!(
            c.req("SetTBarPosition", json!({ "position": 0.45, "release": false })).await["requestStatus"]["result"],
            true
        );
        assert!((fake.with(|s| s.tbar) - 0.45).abs() < 1e-4);
        assert_eq!(c.req("SetTBarPosition", json!({ "position": 2.0 })).await["requestStatus"]["code"], 402);

        // Collections, profiles & hotkeys
        let cols = c.req("GetSceneCollectionList", json!({})).await;
        assert_eq!(cols["responseData"]["currentSceneCollectionName"], "Main");
        c.req("CreateSceneCollection", json!({ "sceneCollectionName": "Podcast" })).await;
        c.req("SetCurrentSceneCollection", json!({ "sceneCollectionName": "IRL" })).await;
        assert_eq!(
            c.req("GetSceneCollectionList", json!({})).await["responseData"]["currentSceneCollectionName"],
            "IRL"
        );
        c.req("SetCurrentProfile", json!({ "profileName": "720p30" })).await;
        assert_eq!(c.req("GetProfileList", json!({})).await["responseData"]["currentProfileName"], "720p30");
        assert!(
            c.req("GetHotkeyList", json!({})).await["responseData"]["hotkeys"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h == "go_live")
        );

        // Scene item transform, lock, index, create, duplicate, remove
        let tf = c.req("GetSceneItemTransform", json!({ "sceneName": "Game", "sceneItemId": 7 })).await;
        assert_eq!(tf["responseData"]["sceneItemTransform"]["positionX"], 100.0);
        assert_eq!(
            c.req(
                "SetSceneItemTransform",
                json!({ "sceneName": "Game", "sceneItemId": 7, "sceneItemTransform": { "positionX": 220.0, "boundsType": "OBS_BOUNDS_SCALE_INNER" } })
            )
            .await["requestStatus"]["result"],
            true
        );
        assert_eq!(fake.with(|s| s.cam_transform.x), 220.0);
        assert_eq!(fake.with(|s| s.cam_transform.bounds.clone()), "fit");
        c.req("SetSceneItemLocked", json!({ "sceneName": "Game", "sceneItemId": 7, "sceneItemLocked": true })).await;
        assert_eq!(c.event("SceneItemLockStateChanged").await["sceneItemLocked"], true);
        assert_eq!(
            c.req("GetSceneItemLocked", json!({ "sceneName": "Game", "sceneItemId": 7 })).await["responseData"]["sceneItemLocked"],
            true
        );
        c.req("SetSceneItemIndex", json!({ "sceneName": "Game", "sceneItemId": 7, "sceneItemIndex": 0 })).await;
        assert_eq!(
            c.req("GetSceneItemIndex", json!({ "sceneName": "Game", "sceneItemId": 7 })).await["responseData"]["sceneItemIndex"],
            0
        );
        let dup = c.req("DuplicateSceneItem", json!({ "sceneName": "Game", "sceneItemId": 7 })).await;
        let dup_id = dup["responseData"]["sceneItemId"].as_i64().unwrap();
        assert_ne!(dup_id, 7);
        assert_eq!(
            c.req("RemoveSceneItem", json!({ "sceneName": "Game", "sceneItemId": dup_id })).await["requestStatus"]["result"],
            true
        );

        // Inputs, settings, audio monitor/sync/balance/tracks
        assert!(
            c.req("GetInputKindList", json!({})).await["responseData"]["inputKinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k == "av_capture_input")
        );
        let ci = c
            .req(
                "CreateInput",
                json!({ "sceneName": "Game", "inputName": "Banner", "inputKind": "image_source", "inputSettings": { "file": "/a.png" }, "sceneItemEnabled": false }),
            )
            .await;
        assert_eq!(ci["requestStatus"]["result"], true);
        let banner_id = ci["responseData"]["sceneItemId"].as_i64().unwrap();
        assert_eq!(
            c.req("GetInputSettings", json!({ "inputName": "Banner" })).await["responseData"]["inputSettings"]["file"],
            "/a.png"
        );
        c.req("SetInputSettings", json!({ "inputName": "Banner", "inputSettings": { "file": "/b.png" } })).await;
        c.req("SetInputName", json!({ "inputName": "Banner", "newInputName": "LogoBanner" })).await;
        c.req("RemoveSceneItem", json!({ "sceneName": "Game", "sceneItemId": banner_id })).await;

        c.req(
            "SetInputAudioMonitorType",
            json!({ "inputName": "Mic", "monitorType": "OBS_MONITORING_TYPE_MONITOR_AND_OUTPUT" }),
        )
        .await;
        assert_eq!(
            c.req("GetInputAudioMonitorType", json!({ "inputName": "Mic" })).await["responseData"]["monitorType"],
            "OBS_MONITORING_TYPE_MONITOR_AND_OUTPUT"
        );
        c.req("SetInputAudioSyncOffset", json!({ "inputName": "Mic", "inputAudioSyncOffset": 250_000_000 })).await;
        assert_eq!(
            c.req("GetInputAudioSyncOffset", json!({ "inputName": "Mic" })).await["responseData"]["inputAudioSyncOffset"],
            250_000_000
        );
        c.req("SetInputAudioBalance", json!({ "inputName": "Mic", "inputAudioBalance": 0.75 })).await;
        assert_eq!(
            c.req("GetInputAudioBalance", json!({ "inputName": "Mic" })).await["responseData"]["inputAudioBalance"],
            0.75
        );
        c.req(
            "SetInputAudioTracks",
            json!({ "inputName": "Mic", "inputAudioTracks": { "1": true, "2": false, "3": true } }),
        )
        .await;
        assert_eq!(
            c.req("GetInputAudioTracks", json!({ "inputName": "Mic" })).await["responseData"]["inputAudioTracks"]["3"],
            true
        );

        // Filters CRUD & index
        c.req("CreateSourceFilter", json!({ "sourceName": "Cam", "filterName": "Color", "filterKind": "color_filter", "filterSettings": { "gamma": 1.2 } })).await;
        assert_eq!(
            c.req("GetSourceFilter", json!({ "sourceName": "Cam", "filterName": "Color" })).await["responseData"]["filterSettings"]
                ["gamma"],
            1.2
        );
        c.req(
            "SetSourceFilterSettings",
            json!({ "sourceName": "Cam", "filterName": "Color", "filterSettings": { "gamma": 1.5 } }),
        )
        .await;
        c.req("SetSourceFilterIndex", json!({ "sourceName": "Cam", "filterName": "Color", "filterIndex": 0 })).await;
        assert_eq!(
            c.req("GetSourceFilter", json!({ "sourceName": "Cam", "filterName": "Color" })).await["responseData"]["filterIndex"],
            0
        );
        assert_eq!(
            c.req("RemoveSourceFilter", json!({ "sourceName": "Cam", "filterName": "Color" })).await["requestStatus"]["result"],
            true
        );

        // Media, screenshots & recording split/chapters
        assert_eq!(
            c.req("GetMediaInputStatus", json!({ "inputName": "Cam" })).await["responseData"]["mediaState"],
            "OBS_MEDIA_STATE_PLAYING"
        );
        c.req(
            "TriggerMediaInputAction",
            json!({ "inputName": "Cam", "mediaAction": "OBS_WEBSOCKET_MEDIA_INPUT_ACTION_PAUSE" }),
        )
        .await;
        c.req("SetMediaInputCursor", json!({ "inputName": "Cam", "mediaCursor": 5000 })).await;
        let ms = c.req("GetMediaInputStatus", json!({ "inputName": "Cam" })).await;
        assert_eq!(
            (ms["responseData"]["mediaState"].clone(), ms["responseData"]["mediaCursor"].clone()),
            (json!("OBS_MEDIA_STATE_PAUSED"), json!(5000))
        );
        assert!(
            c.req("GetSourceScreenshot", json!({ "sourceName": "Cam", "imageFormat": "png" })).await["responseData"]["imageData"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(
            c.req("SaveSourceScreenshot", json!({ "sourceName": "Cam", "imageFormat": "png" })).await["requestStatus"]
                ["result"],
            true
        );

        assert_eq!(c.req("SplitRecordFile", json!({})).await["requestStatus"]["code"], 501);
        c.req("StartRecord", json!({})).await;
        assert_eq!(c.req("SplitRecordFile", json!({})).await["requestStatus"]["result"], true);
        assert_eq!(
            c.req("CreateRecordChapter", json!({ "chapterName": "Boss Fight" })).await["requestStatus"]["result"],
            true
        );
        assert_eq!(fake.with(|s| (s.splits, s.chapters.clone())), (1, vec!["Boss Fight".to_string()]));
    }
}
