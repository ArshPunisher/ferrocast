//! A fake streaming platform (an RTMP server that records what arrives) and a fake OBS (an RTMP
//! publisher), for Go live everywhere tests across crates. Both are rml_rtmp sessions, so the
//! bytes on the wire are real RTMP.

use std::time::Duration;

use bytes::Bytes;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{
    ClientSession, ClientSessionConfig, ClientSessionEvent, ClientSessionResult, PublishRequestType, ServerSession,
    ServerSessionConfig, ServerSessionEvent, ServerSessionResult,
};
use rml_rtmp::time::RtmpTimestamp;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

/// What a fake platform saw, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Got {
    Publish(String),
    Video(u32, Bytes),
    Audio(u32, Bytes),
    Closed,
}

async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, peer: PeerType) -> Option<Vec<u8>> {
    let client = peer == PeerType::Client;
    let mut hs = Handshake::new(peer);
    if client {
        let p = hs.generate_outbound_p0_and_p1().ok()?;
        s.write_all(&p).await.ok()?;
    }
    let mut buf = vec![0u8; 4096];
    loop {
        let n = s.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        match hs.process_bytes(&buf[..n]).ok()? {
            HandshakeProcessResult::InProgress { response_bytes } => s.write_all(&response_bytes).await.ok()?,
            HandshakeProcessResult::Completed { response_bytes, remaining_bytes } => {
                s.write_all(&response_bytes).await.ok()?;
                return Some(remaining_bytes);
            }
        }
    }
}

/// A fake platform on 127.0.0.1: any app, publishing only with `key` (a wrong key is refused
/// and the connection closed, as Twitch does). Returns its port and what it saw.
pub async fn platform(key: &'static str) -> (u16, mpsc::UnboundedReceiver<Got>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = serve(&mut sock, key, &tx).await;
                let _ = tx.send(Got::Closed);
            });
        }
    });
    (port, rx)
}

async fn serve(sock: &mut TcpStream, key: &str, tx: &mpsc::UnboundedSender<Got>) -> Option<()> {
    let rest = handshake(sock, PeerType::Server).await?;
    let (mut s, first) = ServerSession::new(ServerSessionConfig::new()).ok()?;
    let mut queue = first;
    queue.extend(s.handle_input(&rest).ok()?);
    let mut buf = vec![0u8; 65536];
    loop {
        while !queue.is_empty() {
            for r in std::mem::take(&mut queue) {
                match r {
                    ServerSessionResult::OutboundResponse(p) => sock.write_all(&p.bytes).await.ok()?,
                    ServerSessionResult::RaisedEvent(ev) => match ev {
                        ServerSessionEvent::ConnectionRequested { request_id, .. } => {
                            queue.extend(s.accept_request(request_id).ok()?)
                        }
                        ServerSessionEvent::PublishStreamRequested { request_id, stream_key, .. } => {
                            if stream_key != key {
                                for r in s.reject_request(request_id, "NetStream.Publish.BadName", "bad key").ok()? {
                                    if let ServerSessionResult::OutboundResponse(p) = r {
                                        sock.write_all(&p.bytes).await.ok()?;
                                    }
                                }
                                return None;
                            }
                            let _ = tx.send(Got::Publish(stream_key));
                            queue.extend(s.accept_request(request_id).ok()?);
                        }
                        ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
                            let _ = tx.send(Got::Video(timestamp.value, data));
                        }
                        ServerSessionEvent::AudioDataReceived { data, timestamp, .. } => {
                            let _ = tx.send(Got::Audio(timestamp.value, data));
                        }
                        _ => {}
                    },
                    ServerSessionResult::UnhandleableMessageReceived(_) => {}
                }
            }
        }
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        queue.extend(s.handle_input(&buf[..n]).ok()?);
    }
}

/// The next thing a fake platform saw, skipping bare connections that closed.
pub async fn next(rx: &mut mpsc::UnboundedReceiver<Got>) -> Got {
    loop {
        let g = timeout(WAIT, rx.recv()).await.expect("the platform got something in time").expect("platform open");
        if g != Got::Closed {
            return g;
        }
    }
}

/// A fake OBS, publishing to `rtmp://127.0.0.1:<port>/<app>` with `key`.
pub struct Obs {
    sock: TcpStream,
    c: ClientSession,
}

impl std::fmt::Debug for Obs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Obs").finish_non_exhaustive()
    }
}

impl Obs {
    pub async fn publish(port: u16, app: &str, key: &str) -> Result<Self, String> {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        let rest = handshake(&mut sock, PeerType::Client).await.ok_or("handshake failed")?;
        let (c, first) = ClientSession::new(ClientSessionConfig::new()).map_err(|e| e.to_string())?;
        let mut obs = Self { sock, c };
        obs.send(first).await?;
        let r = obs.c.handle_input(&rest).map_err(|e| e.to_string())?;
        obs.send(r).await?;
        let r = obs.c.request_connection(app.to_string()).map_err(|e| e.to_string())?;
        obs.send(vec![r]).await?;
        obs.until(|e| matches!(e, ClientSessionEvent::ConnectionRequestAccepted)).await?;
        let r = obs.c.request_publishing(key.to_string(), PublishRequestType::Live).map_err(|e| e.to_string())?;
        obs.send(vec![r]).await?;
        obs.until(|e| matches!(e, ClientSessionEvent::PublishRequestAccepted)).await?;
        Ok(obs)
    }

    async fn send(&mut self, results: Vec<ClientSessionResult>) -> Result<Vec<ClientSessionEvent>, String> {
        let mut events = Vec::new();
        for r in results {
            match r {
                ClientSessionResult::OutboundResponse(p) => {
                    self.sock.write_all(&p.bytes).await.map_err(|e| e.to_string())?
                }
                ClientSessionResult::RaisedEvent(e) => events.push(e),
                ClientSessionResult::UnhandleableMessageReceived(_) => {}
            }
        }
        Ok(events)
    }

    async fn until(&mut self, want: impl Fn(&ClientSessionEvent) -> bool) -> Result<(), String> {
        let mut buf = vec![0u8; 65536];
        loop {
            let n = timeout(WAIT, self.sock.read(&mut buf))
                .await
                .map_err(|_| "timed out".to_string())?
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("closed".into());
            }
            let r = self.c.handle_input(&buf[..n]).map_err(|e| e.to_string())?;
            for e in self.send(r).await? {
                if want(&e) {
                    return Ok(());
                }
                if let ClientSessionEvent::ConnectionRequestRejected { description } = &e {
                    return Err(format!("rejected: {description}"));
                }
            }
        }
    }

    /// Sends one video packet (`0x17…` keyframe, `0x27…` inter frame).
    pub async fn video(&mut self, ts: u32, data: &'static [u8]) {
        let r = self.c.publish_video_data(Bytes::from_static(data), RtmpTimestamp::new(ts), false).unwrap();
        self.send(vec![r]).await.unwrap();
    }

    /// Keyframes every 100 ms of stream time until `got` sees one, like OBS keeps sending.
    pub async fn keyframes_until_seen(&mut self, got: &mut mpsc::UnboundedReceiver<Got>) -> Got {
        let mut ts = 0;
        loop {
            self.video(ts, &[0x17, 0x01, 0, 0, 0, 0xAA]).await;
            ts += 100;
            if let Ok(Some(g)) = timeout(Duration::from_millis(200), got.recv()).await
                && g != Got::Closed
            {
                return g;
            }
            assert!(ts < 10_000, "no keyframe got through");
        }
    }
}
