//! Ferrocast Bond (Pro). The Studio sends its stream here over every connection at once
//! (SRTLA, or plain SRT), and this sends it on to the streamer's platforms by RTMP. The
//! computer uploads one stream however many platforms there are, and a dropped connection
//! (or a dead computer) doesn't end the stream: after a few seconds with nothing arriving, a
//! Be right back picture goes out instead, for up to ten minutes, and the stream picks up
//! again when the computer is back.
//!
//! How a stream starts: the app gets a Bond token from the relay (it checks Pro and signs it
//! with the secret it shares with this service), creates a session here with the platforms'
//! addresses and keys (`POST /v1/sessions`), and sends to the SRTLA port with the stream id
//! the session gave. The keys are only ever held in memory, for the session.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrocast_golive::demux::{Tag, TagKind, TsDemuxer};
use ferrocast_golive::{Destination, Health, Hub, PushState, Target};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::watch;

pub mod irl;
pub mod push;
pub mod token;

/// The service's settings (from the environment in `main`).
#[derive(Clone)]
pub struct Config {
    /// The secret Bond tokens are signed with (shared with the relay).
    pub secret: Vec<u8>,
    /// Where senders send SRTLA (UDP).
    pub srtla: SocketAddr,
    /// Where SRT arrives (UDP): from the SRTLA receiver here, or straight from a sender.
    pub srt: SocketAddr,
    /// How long the Be right back picture holds a stream whose computer went quiet.
    pub keepalive: Duration,
    pub max_sessions: usize,
    /// ffmpeg, to make each session's Be right back picture (none: no picture, the stream ends
    /// when the computer drops).
    pub ffmpeg: Option<std::path::PathBuf>,
    /// Platforms on private networks allowed (tests only: otherwise a streamer could point
    /// the service at the server's own network).
    pub allow_private: bool,
    /// The service's own HTTPS address (the phone page's link starts with it).
    pub public_url: String,
    /// Phones' WebRTC: the address to take it on (None: the one traffic leaves by) and the
    /// UDP ports open for it.
    pub rtc_ip: Option<std::net::IpAddr>,
    pub rtc_ports: (u16, u16),
    /// The key pushes to phones are signed with (PKCS#8, base64url; None: no pushes).
    pub vapid: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret never goes in a log.
        f.debug_struct("Config")
            .field("srtla", &self.srtla)
            .field("srt", &self.srt)
            .field("keepalive", &self.keepalive)
            .field("max_sessions", &self.max_sessions)
            .finish_non_exhaustive()
    }
}

/// After this long with nothing arriving, the Be right back picture goes out.
const QUIET: Duration = Duration::from_secs(3);
/// A session nobody sends to in this long is forgotten (a phone's waits longer: it's made
/// before setting out).
const UNUSED: Duration = Duration::from_secs(120);
const UNUSED_IRL: Duration = Duration::from_secs(12 * 3600);
/// An ended session's health can still be read this long.
const ENDED_KEPT: Duration = Duration::from_secs(300);
const MAX_DESTINATIONS: usize = 8;

/// Where the stream's input is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Input {
    /// Made, nothing sent yet.
    Waiting,
    Live,
    /// The computer went quiet: Be right back is out.
    Away,
    Ended,
}

struct Dest {
    id: String,
    health: watch::Receiver<Health>,
    stop: watch::Sender<bool>,
}

/// Times going out keep rising across the computer's stream, a reconnection and the Be right
/// back picture: each run of tags (a segment) starts just after the last one sent.
#[derive(Debug, Default)]
pub struct Clock {
    segment: u64,
    offset: i64,
    last_out: u32,
    started: bool,
}

/// The gap left between one segment and the next (a frame at 25 fps).
const SEGMENT_GAP_MS: i64 = 40;

impl Clock {
    /// The time `ms` of segment `segment` has going out.
    pub fn map(&mut self, segment: u64, ms: u32) -> u32 {
        if !self.started || segment != self.segment {
            let start = if self.started { i64::from(self.last_out) + SEGMENT_GAP_MS } else { 0 };
            self.offset = start - i64::from(ms);
            self.segment = segment;
            self.started = true;
        }
        let out = (i64::from(ms) + self.offset).max(0) as u32;
        self.last_out = self.last_out.max(out);
        out
    }
}

pub(crate) struct Session {
    id: String,
    account: String,
    hub: Arc<Hub>,
    dests: Vec<Dest>,
    created: Instant,
    /// The picture's size and rate, said to the platforms when the stream starts.
    picture: (u32, u32, u32),
    feed: Mutex<Feed>,
    /// The Be right back picture as tags, made once the session starts (None: none here).
    brb: Mutex<Option<Arc<Vec<Tag>>>>,
    /// Streamed from a phone (the IRL page): its secret and its connection now.
    irl: Option<irl::Irl>,
    /// Sends pushes to that phone (when this service has a key).
    pusher: Option<Arc<push::Pusher>>,
}

struct Feed {
    input: Input,
    last_input: Option<Instant>,
    away_since: Option<Instant>,
    ended_at: Option<Instant>,
    clock: Clock,
    /// Segment ids: each SRT connection and each Be right back run gets the next.
    next_segment: u64,
    /// The segment allowed to send now (a newer connection takes over from an older one).
    current: u64,
}

impl Session {
    /// A run of tags starts (a connection, a Be right back loop) → its segment, now the one sent.
    fn begin(&self) -> u64 {
        let mut f = self.feed.lock().unwrap_or_else(|p| p.into_inner());
        f.next_segment += 1;
        f.current = f.next_segment;
        f.current
    }

    /// One tag of `segment`, sent if that segment is still the current one → whether it was.
    fn send(&self, segment: u64, tag: &Tag) -> bool {
        let ms = {
            let mut f = self.feed.lock().unwrap_or_else(|p| p.into_inner());
            if f.current != segment || f.input == Input::Ended {
                return false;
            }
            f.clock.map(segment, tag.ms)
        };
        match tag.kind {
            TagKind::Video => self.hub.video(ms, tag.body.clone()),
            TagKind::Audio => self.hub.audio(ms, tag.body.clone()),
        }
        true
    }

    /// The computer's stream arrived: live (and the hub started on the first).
    fn heard(&self) {
        let mut f = self.feed.lock().unwrap_or_else(|p| p.into_inner());
        if f.input == Input::Ended {
            return;
        }
        if f.input == Input::Waiting {
            self.hub.start();
            let (w, h, fps) = self.picture;
            let mut m = rml_rtmp::sessions::StreamMetadata::new();
            m.video_width = Some(w);
            m.video_height = Some(h);
            m.video_frame_rate = Some(fps as f32);
            // H.264 and AAC, FLV's ids.
            m.video_codec_id = Some(7);
            m.audio_codec_id = Some(10);
            m.encoder = Some("Ferrocast Bond".into());
            self.hub.metadata(m);
        }
        let back = f.input == Input::Away;
        f.input = Input::Live;
        f.last_input = Some(Instant::now());
        f.away_since = None;
        drop(f);
        if back {
            self.notify("You're live again", "Your stream is back on every platform.");
        }
    }

    /// A push to the phone the session streams from (when it asked for them).
    pub(crate) fn notify(&self, title: &str, body: &str) {
        let (Some(irl), Some(p)) = (&self.irl, &self.pusher) else { return };
        let subs = irl.subscriptions();
        if subs.is_empty() {
            return;
        }
        let (p, m) = (p.clone(), json!({ "title": title, "body": body, "tag": format!("ferrocast-{}", self.id) }));
        tokio::spawn(async move {
            for s in subs {
                match p.send(&s, &m).await {
                    Ok(()) => tracing::info!("Bond: a push went to the phone"),
                    Err(e) => tracing::info!(error = %e, "Bond: a push didn't go"),
                }
            }
        });
    }

    fn end(&self) {
        let mut f = self.feed.lock().unwrap_or_else(|p| p.into_inner());
        if f.input == Input::Ended {
            return;
        }
        f.input = Input::Ended;
        f.ended_at = Some(Instant::now());
        drop(f);
        if let Some(i) = &self.irl {
            i.stop();
        }
        self.hub.end();
        for d in &self.dests {
            let _ = d.stop.send(true);
        }
    }

    fn view(&self) -> serde_json::Value {
        let f = self.feed.lock().unwrap_or_else(|p| p.into_inner());
        json!({
            "id": self.id,
            "input": f.input,
            "away_for_s": f.away_since.map(|t| t.elapsed().as_secs()),
            "destinations": self.dests.iter().map(|d| json!({ "id": d.id, "health": *d.health.borrow() })).collect::<Vec<_>>(),
        })
    }
}

/// The running service.
#[derive(Clone)]
pub struct Bond {
    cfg: Arc<Config>,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    pusher: Option<Arc<push::Pusher>>,
}

impl std::fmt::Debug for Bond {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bond").field("cfg", &self.cfg).finish_non_exhaustive()
    }
}

/// A failure, as the API says it: a status, a code and words.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError { status, code, message: message.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": { "code": self.code, "message": self.message } }))).into_response()
    }
}

/// One platform to send to.
#[derive(Debug, Clone, Deserialize)]
pub struct NewDestination {
    pub id: String,
    pub url: String,
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewSession {
    pub token: String,
    pub destinations: Vec<NewDestination>,
    /// The picture's size and rate, for the Be right back picture (it matches).
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Sent from a phone (the IRL page, its link in the answer): waits up to 12 hours for it.
    #[serde(default)]
    pub irl: bool,
}

/// Where `host` is, when every address it has is on the internet (not this server, its
/// network, or a cloud's own addresses): the service only ever sends to platforms.
async fn public(host: &str, port: u16) -> Option<SocketAddr> {
    use std::net::IpAddr;
    let ok = |ip: IpAddr| match ip {
        IpAddr::V4(v) => {
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_multicast()
                || v.is_broadcast()
                || v.is_documentation()
                // 100.64/10 (carrier-grade NAT), 0/8, 192.0.0/24, 198.18/15 (benchmarks),
                // 240/4 (reserved).
                || (v.octets()[0] == 100 && v.octets()[1] & 0xC0 == 64)
                || v.octets()[0] == 0
                || v.octets()[..3] == [192, 0, 0]
                || (v.octets()[0] == 198 && v.octets()[1] & 0xFE == 18)
                || v.octets()[0] >= 240)
        }
        IpAddr::V6(v) => {
            let s = v.segments()[0];
            // Unique-local, link-local, NAT64 (64:ff9b::/96) and 6to4 (2002::/16) too: they lead
            // to IPv4 addresses that aren't checked here.
            !(v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || s & 0xFE00 == 0xFC00
                || s & 0xFFC0 == 0xFE80
                || (s == 0x64 && v.segments()[1] == 0xff9b)
                || s == 0x2002)
                && v.to_ipv4_mapped()
                    .is_none_or(|m| !(m.is_private() || m.is_loopback() || m.is_link_local() || m.is_unspecified()))
        }
    };
    let all: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await.ok()?.collect();
    if all.is_empty() || !all.iter().all(|a| ok(a.ip())) {
        return None;
    }
    all.first().copied()
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn random_id() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Bond {
    pub fn new(cfg: Config) -> Self {
        let pusher = cfg.vapid.as_deref().and_then(push::Pusher::new).map(Arc::new);
        Bond { cfg: Arc::new(cfg), sessions: Arc::new(Mutex::new(HashMap::new())), pusher }
    }

    /// The HTTP API.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/v1/sessions", post(create))
            .route("/v1/sessions/{id}", get(read).delete(stop))
            .merge(irl::routes())
            .with_state(self.clone())
    }

    fn account_of(&self, headers: &HeaderMap) -> Result<String, ApiError> {
        let t = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| {
                ApiError::new(StatusCode::UNAUTHORIZED, "no_token", "Send the Bond token as a Bearer token.")
            })?;
        self.verify(t)
    }

    fn verify(&self, t: &str) -> Result<String, ApiError> {
        token::verify(&self.cfg.secret, t, now_ms()).ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "bad_token",
                "That Bond token isn't valid or has expired: ask for a new one.",
            )
        })
    }

    fn session(&self, id: &str, account: &str) -> Result<Arc<Session>, ApiError> {
        let s = self.sessions.lock().unwrap_or_else(|p| p.into_inner()).get(id).cloned();
        s.filter(|s| s.account == account).ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "no_session", "That Bond stream doesn't exist (or ended a while ago).")
        })
    }

    /// Makes a session: checks, starts a sender per platform, and its Be right back picture.
    pub async fn create_session(&self, n: NewSession) -> Result<serde_json::Value, ApiError> {
        let account = self.verify(&n.token)?;
        let bad = |m: &str| ApiError::new(StatusCode::BAD_REQUEST, "invalid", m.to_string());
        if n.destinations.is_empty() || n.destinations.len() > MAX_DESTINATIONS {
            return Err(bad("destinations: 1 to 8 platforms."));
        }
        if !(16..=7680).contains(&n.width) || !(16..=7680).contains(&n.height) || !(1..=120).contains(&n.fps) {
            return Err(bad("width, height and fps: the picture's size (16 to 7680) and rate (1 to 120)."));
        }
        let mut targets = Vec::new();
        for d in &n.destinations {
            if d.id.is_empty() || d.id.len() > 64 || d.id.chars().any(char::is_control) {
                return Err(bad("destinations[].id: 1 to 64 characters."));
            }
            if d.key.trim().is_empty()
                || d.key.len() > 512
                || d.key.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(bad("destinations[].key: the platform's stream key."));
            }
            let mut t = Target::parse(&d.url).map_err(|e| bad(&format!("destinations[].url: {e}")))?;
            if !self.cfg.allow_private {
                // The address checked is the one used: the name can't lead elsewhere later.
                let at = public(&t.host, t.port)
                    .await
                    .ok_or_else(|| bad("destinations[].url: a platform's address on the internet."))?;
                t.pin = Some(at);
            }
            targets.push((d.id.clone(), t, d.key.trim().to_string()));
        }
        let id = random_id();
        let hub = Hub::new();
        let dests = targets
            .into_iter()
            .map(|(did, target, key)| {
                let (htx, hrx) = watch::channel(Health {
                    state: PushState::Waiting,
                    kbps: 0,
                    dropped: 0,
                    sent: 0,
                    link: None,
                    via: None,
                    since_ms: 0,
                });
                let (stop, stop_rx) = watch::channel(false);
                let dest = Destination { id: did.clone(), target, key, retry: Default::default(), extra_audio: false };
                tokio::spawn(ferrocast_golive::push::run(dest, hub.clone(), htx, stop_rx));
                Dest { id: did, health: hrx, stop }
            })
            .collect();
        let session = Arc::new(Session {
            id: id.clone(),
            account: account.clone(),
            hub,
            dests,
            created: Instant::now(),
            picture: (n.width, n.height, n.fps),
            irl: n.irl.then(irl::Irl::new),
            pusher: self.pusher.clone(),
            feed: Mutex::new(Feed {
                input: Input::Waiting,
                last_input: None,
                away_since: None,
                ended_at: None,
                clock: Clock::default(),
                next_segment: 0,
                current: 0,
            }),
            brb: Mutex::new(None),
        });
        {
            let mut all = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            // One stream per account: a new one ends the old.
            let old: Vec<String> = all.values().filter(|s| s.account == account).map(|s| s.id.clone()).collect();
            for o in old {
                if let Some(s) = all.remove(&o) {
                    s.end();
                }
            }
            // Ended ones are kept a while to be read: they don't count.
            let open =
                all.values().filter(|s| s.feed.lock().unwrap_or_else(|p| p.into_inner()).input != Input::Ended).count();
            if open >= self.cfg.max_sessions {
                session.end();
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "full",
                    "Ferrocast Bond is full right now: stream from your computer this time, or try again in a few minutes.",
                ));
            }
            all.insert(id.clone(), session.clone());
        }
        if let Some(ffmpeg) = self.cfg.ffmpeg.clone() {
            let s = session.clone();
            tokio::spawn(async move {
                match brb_tags(&ffmpeg, n.width, n.height, n.fps).await {
                    Ok(tags) => *s.brb.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(tags)),
                    Err(e) => tracing::warn!(error = %e, "no Be right back picture for this session"),
                }
            });
        }
        tracing::info!(session = %id, %account, irl = n.irl, "Bond: a session started");
        let mut out = json!({ "id": id, "stream_id": format!("bond:{id}"), "srtla_port": self.cfg.srtla.port(), "srt_port": self.cfg.srt.port() });
        if let Some(i) = &session.irl {
            // The secret rides in the link's #fragment: it never reaches a server's log.
            out["phone_url"] = json!(format!("{}/irl#{id}.{}", self.cfg.public_url, i.secret));
        }
        Ok(out)
    }

    /// The media side: the SRTLA receiver, the SRT listener, and the keep-alive watch. Runs
    /// until the service stops.
    pub async fn run_media(&self) -> std::io::Result<()> {
        let srtla = tokio::net::UdpSocket::bind(self.cfg.srtla).await?;
        let to_srt = SocketAddr::from(([127, 0, 0, 1], self.cfg.srt.port()));
        tokio::spawn(async move {
            if let Err(e) = ferrocast_golive::srtla::receive(srtla, to_srt).await {
                tracing::error!(error = %e, "Bond: the SRTLA receiver stopped");
            }
        });
        let (_listener, mut incoming) =
            srt_tokio::SrtListener::builder().latency(Duration::from_millis(500)).bind(self.cfg.srt).await?;
        let watch = self.clone();
        tokio::spawn(async move { watch.keepalive().await });
        while let Some(req) = incoming.incoming().next().await {
            let sid = req.stream_id().map(|s| s.to_string()).unwrap_or_default();
            let session = sid
                .strip_prefix("bond:")
                .and_then(|id| self.sessions.lock().unwrap_or_else(|p| p.into_inner()).get(id).cloned())
                .filter(|s| s.feed.lock().unwrap_or_else(|p| p.into_inner()).input != Input::Ended);
            let Some(session) = session else {
                let _ = req
                    .reject(srt_tokio::access::RejectReason::Server(srt_tokio::access::ServerRejectReason::Forbidden))
                    .await;
                continue;
            };
            tokio::spawn(async move {
                match req.accept(None).await {
                    Ok(sock) => take_input(session, sock).await,
                    Err(e) => tracing::info!(error = %e, "Bond: a connection didn't finish"),
                }
            });
        }
        Ok(())
    }

    /// Twice a second: Be right back for quiet streams, ends for long-quiet ones, and old
    /// sessions forgotten.
    async fn keepalive(&self) {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tick.tick().await;
            let all: Vec<Arc<Session>> =
                self.sessions.lock().unwrap_or_else(|p| p.into_inner()).values().cloned().collect();
            for s in all {
                let (input, quiet, away, ended) = {
                    let f = s.feed.lock().unwrap_or_else(|p| p.into_inner());
                    (f.input, f.last_input.map(|t| t.elapsed()), f.away_since.map(|t| t.elapsed()), f.ended_at)
                };
                match input {
                    Input::Waiting if s.created.elapsed() > if s.irl.is_some() { UNUSED_IRL } else { UNUSED } => {
                        s.end()
                    }
                    Input::Live if quiet.is_some_and(|q| q > QUIET) => {
                        let brb = s.brb.lock().unwrap_or_else(|p| p.into_inner()).clone();
                        match brb {
                            Some(tags) => {
                                {
                                    let mut f = s.feed.lock().unwrap_or_else(|p| p.into_inner());
                                    f.input = Input::Away;
                                    f.away_since = Some(Instant::now());
                                }
                                tracing::info!(session = %s.id, "Bond: the computer went quiet, Be right back is out");
                                s.notify(
                                    "Signal lost",
                                    "Your channels show Be right back. Open Ferrocast IRL to carry on.",
                                );
                                tokio::spawn(play_brb(s.clone(), tags));
                            }
                            // Nothing to show: the stream ends as it would have.
                            None => s.end(),
                        }
                    }
                    Input::Away if away.is_some_and(|a| a > self.cfg.keepalive) => {
                        tracing::info!(session = %s.id, "Bond: the computer didn't come back, the stream ends");
                        s.notify("Stream ended", "Your phone didn't come back in time, so the stream ended.");
                        s.end();
                    }
                    _ => {}
                }
                if ended.is_some_and(|e| e.elapsed() > ENDED_KEPT) {
                    self.sessions.lock().unwrap_or_else(|p| p.into_inner()).remove(&s.id);
                }
            }
        }
    }
}

/// The computer's stream: TS from SRT into tags, into the hub, while this connection is the
/// current one.
async fn take_input(session: Arc<Session>, mut sock: srt_tokio::SrtSocket) {
    let segment = session.begin();
    let mut demux = TsDemuxer::new();
    tracing::info!(session = %session.id, "Bond: the computer's stream arrived");
    while let Some(Ok((_, bytes))) = sock.next().await {
        let tags = demux.push(&bytes);
        session.heard();
        for t in &tags {
            if !session.send(segment, t) {
                // A newer connection took over (or the stream ended): this one goes.
                use futures::SinkExt;
                let _ = sock.close().await;
                return;
            }
        }
        if let Some(e) = demux.refused() {
            tracing::warn!(session = %session.id, error = %e, "Bond: this stream can't be carried");
            session.end();
            return;
        }
    }
}

/// The Be right back picture, looped in real time while the computer is away.
async fn play_brb(session: Arc<Session>, tags: Arc<Vec<Tag>>) {
    let Some(last) = tags.last().map(|t| t.ms) else { return };
    let loop_ms = last + 40;
    let mut round = 0u32;
    let start = tokio::time::Instant::now();
    loop {
        // Each loop is its own segment: its times start after what went before.
        let segment = session.begin();
        for t in tags.iter() {
            let due = start + Duration::from_millis(u64::from(round * loop_ms + t.ms));
            tokio::time::sleep_until(due).await;
            {
                let f = session.feed.lock().unwrap_or_else(|p| p.into_inner());
                if f.input != Input::Away {
                    return;
                }
            }
            if !session.send(segment, t) {
                return;
            }
        }
        round += 1;
    }
}

/// The Be right back card (`assets/brb.html`, rendered at 1920x1080), shown at each stream's
/// size: no fonts needed where this runs.
const BRB_CARD: &[u8] = include_bytes!("../assets/brb.jpg");

/// A few seconds of Be right back at the stream's size and rate (H.264 and AAC, a keyframe
/// every second), made by ffmpeg from the card and read back as tags.
async fn brb_tags(ffmpeg: &std::path::Path, w: u32, h: u32, fps: u32) -> Result<Vec<Tag>, String> {
    use tokio::io::AsyncWriteExt;
    let (w, h) = (w & !1, h & !1);
    let vf = format!(
        "loop=loop=-1:size=1:start=0,fps={fps},scale={w}:{h}:force_original_aspect_ratio=decrease,\
        pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=0x0d1014,format=yuv420p"
    );
    let mut child = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-f", "image2pipe", "-i", "-"])
        .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-t", "4", "-vf", &vf])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-bf", "0", "-g", &fps.to_string(), "-b:v", "1500k"])
        .args(["-c:a", "aac", "-b:a", "96k", "-f", "mpegts", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut input) = child.stdin.take() {
        input.write_all(BRB_CARD).await.map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let mut d = TsDemuxer::new();
    let mut tags = d.push(&out.stdout);
    // The last PES of each kind finishes with one more packet: a null one.
    let mut pad = vec![0x47, 0x1F, 0xFF, 0x10];
    pad.resize(188, 0xFF);
    tags.extend(d.push(&pad));
    if !tags.iter().any(|t| t.kind == TagKind::Video) {
        return Err("ffmpeg made no picture".into());
    }
    Ok(tags)
}

async fn create(
    State(b): State<Bond>,
    body: Result<Json<NewSession>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let Json(n) = body.map_err(|e| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid", format!("The request isn't right: {}", e.body_text()))
    })?;
    Ok((StatusCode::CREATED, Json(b.create_session(n).await?)))
}

async fn read(
    State(b): State<Bond>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let account = b.account_of(&h)?;
    Ok(Json(b.session(&id, &account)?.view()))
}

async fn stop(State(b): State<Bond>, Path(id): Path<String>, h: HeaderMap) -> Result<StatusCode, ApiError> {
    let account = b.account_of(&h)?;
    b.session(&id, &account)?.end();
    Ok(StatusCode::NO_CONTENT)
}

/// Raw bytes as a tag body (tests).
#[cfg(test)]
fn tag(kind: TagKind, ms: u32) -> Tag {
    Tag { kind, ms, body: bytes::Bytes::from_static(&[0x17, 1, 0, 0, 0]) }
}

#[cfg(test)]
mod tests;
