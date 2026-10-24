//! Where OBS sends its stream: an RTMP server on 127.0.0.1 only (R-43). Each stream key is a
//! route to its own [`Hub`] (the normal picture, and OBS's vertical canvas on a second key);
//! each route takes one publisher at a time. OBS's stream setting points here
//! (`rtmp://127.0.0.1:<port>/live` + the key).

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rml_rtmp::handshake::PeerType;
use rml_rtmp::sessions::{ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::time::timeout;

use crate::conn::{self, READ_BUF};
use crate::hub::Hub;

/// The RTMP application OBS publishes to.
pub const APP: &str = "live";
/// The usual RTMP port; taken by something else, the engine binds another.
pub const DEFAULT_PORT: u16 = 1935;
/// A handshake must finish quickly; anything slower isn't OBS.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);
/// OBS sends packets many times a second; silence this long means it's gone.
const IDLE: Duration = Duration::from_secs(30);
/// Local connections at once (OBS uses one; a few more for retries and the upload check).
const MAX_CONNECTIONS: usize = 4;

/// One stream key and where its stream goes.
#[derive(Debug)]
struct Route {
    key: Arc<str>,
    hub: Arc<Hub>,
    busy: AtomicBool,
}

/// The local RTMP server, bound and ready to run.
#[derive(Debug)]
pub struct Ingest {
    listener: TcpListener,
    routes: Vec<Arc<Route>>,
}

impl Ingest {
    /// Binds 127.0.0.1:`port` (0 picks a free one), with `key` sending to `hub`.
    pub async fn bind(port: u16, key: impl Into<Arc<str>>, hub: Arc<Hub>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await?;
        Ok(Self { listener, routes: vec![Arc::new(Route { key: key.into(), hub, busy: AtomicBool::new(false) })] })
    }

    /// Another stream key, with its own hub (OBS's vertical canvas).
    #[must_use]
    pub fn with_route(mut self, key: impl Into<Arc<str>>, hub: Arc<Hub>) -> Self {
        self.routes.push(Arc::new(Route { key: key.into(), hub, busy: AtomicBool::new(false) }));
        self
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// The server address OBS streams to (the key goes in OBS's stream key field).
    pub fn server_url(&self) -> String {
        format!("rtmp://127.0.0.1:{}/{APP}", self.port())
    }

    /// Accepts connections until `stop` turns true.
    pub async fn run(self, mut stop: watch::Receiver<bool>) {
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS * self.routes.len()));
        let routes: Arc<[Arc<Route>]> = self.routes.clone().into();
        loop {
            tokio::select! {
                _ = stop.wait_for(|s| *s) => return,
                accepted = self.listener.accept() => {
                    let Ok((sock, _)) = accepted else { continue };
                    let Ok(slot) = slots.clone().try_acquire_owned() else {
                        tracing::warn!("golive ingest: too many local connections, refused one");
                        continue;
                    };
                    let routes = routes.clone();
                    tokio::spawn(async move {
                        if let Err(err) = serve(sock, &routes).await {
                            tracing::info!(error = %err, "golive ingest: a connection ended");
                        }
                        drop(slot);
                    });
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum IngestError {
    #[error("handshake: {0}")]
    Handshake(#[from] conn::HandshakeFail),
    #[error("handshake took too long")]
    Slow,
    #[error("no data for {0:?}")]
    Idle(Duration),
    #[error("RTMP: {0}")]
    Session(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// While this lives, the connection is its route's publisher; dropping it ends that stream.
struct Publishing<'a> {
    route: &'a Route,
}

impl Drop for Publishing<'_> {
    fn drop(&mut self) {
        self.route.hub.end();
        self.route.busy.store(false, Ordering::Release);
    }
}

async fn serve(mut sock: TcpStream, routes: &[Arc<Route>]) -> Result<(), IngestError> {
    let _ = sock.set_nodelay(true);
    let rest = timeout(HANDSHAKE_DEADLINE, conn::handshake(&mut sock, PeerType::Server))
        .await
        .map_err(|_| IngestError::Slow)??;
    let session_err = |e: rml_rtmp::sessions::ServerSessionError| IngestError::Session(e.to_string());
    let (mut session, first) = ServerSession::new(ServerSessionConfig::new()).map_err(session_err)?;
    let mut queue: VecDeque<ServerSessionResult> = first.into();
    queue.extend(session.handle_input(&rest).map_err(session_err)?);
    let mut publishing: Option<Publishing<'_>> = None;
    let mut buf = vec![0u8; READ_BUF];
    loop {
        let mut refused = false;
        while let Some(result) = queue.pop_front() {
            match result {
                ServerSessionResult::OutboundResponse(p) => sock.write_all(&p.bytes).await?,
                ServerSessionResult::RaisedEvent(ev) => {
                    let (more, no) = on_event(&mut session, ev, routes, &mut publishing).map_err(session_err)?;
                    refused |= no;
                    queue.extend(more);
                }
                ServerSessionResult::UnhandleableMessageReceived(_) => {}
            }
        }
        // Like the platforms: a refused publisher is told why, then disconnected.
        if refused {
            let _ = sock.shutdown().await;
            return Ok(());
        }
        let n = timeout(IDLE, sock.read(&mut buf)).await.map_err(|_| IngestError::Idle(IDLE))??;
        if n == 0 {
            return Ok(());
        }
        queue.extend(session.handle_input(&buf[..n]).map_err(session_err)?);
    }
}

fn on_event<'a>(
    session: &mut ServerSession,
    ev: ServerSessionEvent,
    routes: &'a [Arc<Route>],
    publishing: &mut Option<Publishing<'a>>,
) -> Result<(Vec<ServerSessionResult>, bool), rml_rtmp::sessions::ServerSessionError> {
    let ok = |r: Vec<ServerSessionResult>| Ok((r, false));
    let refuse = |r: Vec<ServerSessionResult>| Ok((r, true));
    match ev {
        ServerSessionEvent::ConnectionRequested { request_id, app_name } => {
            if app_name == APP {
                ok(session.accept_request(request_id)?)
            } else {
                refuse(session.reject_request(request_id, "NetConnection.Connect.Rejected", "Unknown application")?)
            }
        }
        ServerSessionEvent::PublishStreamRequested { request_id, stream_key, .. } => {
            // Every key is compared (in constant time), so timing doesn't tell which one is close.
            let mut found = None;
            for r in routes {
                if conn::same_secret(&stream_key, &r.key) {
                    found = Some(r.as_ref());
                }
            }
            let Some(route) = found else {
                tracing::warn!("golive ingest: refused a publisher with the wrong stream key");
                return refuse(session.reject_request(request_id, "NetStream.Publish.BadName", "Wrong stream key")?);
            };
            if route.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
                return refuse(session.reject_request(
                    request_id,
                    "NetStream.Publish.BadName",
                    "Already publishing",
                )?);
            }
            route.hub.start();
            *publishing = Some(Publishing { route });
            ok(session.accept_request(request_id)?)
        }
        ServerSessionEvent::StreamMetadataChanged { metadata, .. } => {
            if let Some(p) = publishing {
                p.route.hub.metadata(metadata);
            }
            ok(Vec::new())
        }
        ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
            if let Some(p) = publishing {
                p.route.hub.video(timestamp.value, data);
            }
            ok(Vec::new())
        }
        ServerSessionEvent::AudioDataReceived { data, timestamp, .. } => {
            if let Some(p) = publishing {
                p.route.hub.audio(timestamp.value, data);
            }
            ok(Vec::new())
        }
        ServerSessionEvent::PublishStreamFinished { .. } => {
            *publishing = None;
            ok(Vec::new())
        }
        _ => ok(Vec::new()),
    }
}
