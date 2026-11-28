//! A fake streaming platform (an RTMP server that records what arrives) and a fake OBS (an RTMP
//! publisher), for Go live everywhere tests across crates. Both are rml_rtmp sessions, so the
//! bytes on the wire are real RTMP.

use std::time::Duration;

use bytes::Bytes;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{
    ClientSession, ClientSessionConfig, ClientSessionEvent, ClientSessionResult, PublishRequestType, ServerSession,
    ServerSessionConfig, ServerSessionEvent, ServerSessionResult, StreamMetadata,
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
    platform_on(0, key).await
}

/// `platform` on a chosen port (0: any): a server that comes back where it was.
pub async fn platform_on(port: u16, key: &'static str) -> (u16, mpsc::UnboundedReceiver<Got>) {
    let (port, got, _meta) = platform_on_with_meta(port, key).await;
    (port, got)
}

/// Like [`platform_on`], also forwarding decoded AMF0 [`StreamMetadata`] events.
pub async fn platform_with_meta(
    key: &'static str,
) -> (u16, mpsc::UnboundedReceiver<Got>, mpsc::UnboundedReceiver<StreamMetadata>) {
    platform_on_with_meta(0, key).await
}

async fn platform_on_with_meta(
    port: u16,
    key: &'static str,
) -> (u16, mpsc::UnboundedReceiver<Got>, mpsc::UnboundedReceiver<StreamMetadata>) {
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    let (mtx, mrx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            let mtx = mtx.clone();
            tokio::spawn(async move {
                let _ = serve_with(&mut sock, key, &tx, Some(&mtx), |_| None).await;
                let _ = tx.send(Got::Closed);
            });
        }
    });
    (port, rx, mrx)
}

fn exceeds_dimensions(w: u32, h: u32, max_w: u32, max_h: u32) -> bool {
    w.max(h) > max_w.max(max_h) || w.min(h) > max_w.min(max_h)
}

fn is_video_sequence_header(data: &[u8]) -> bool {
    let Some(&b) = data.first() else { return false };
    if b & 0x80 != 0 { b & 0x0F == 0 } else { matches!(b & 0x0F, 7 | 12) && data.get(1) == Some(&0) }
}

/// Fake ingest enforcing `(max_w, max_h, max_kbps)`: inspects AMF0 `onMetaData`, H.264 `avcC` SPS
/// dimensions via `sps_dims`, and RTMP-timestamped video rate, closing TCP ~200 ms after an
/// offending header or metadata payload arrives. Also forwards decoded [`StreamMetadata`].
pub async fn platform_rejecting_above(
    key: &'static str,
    max_w: u32,
    max_h: u32,
    max_kbps: u32,
    sps_dims: fn(&[u8]) -> Option<(u32, u32)>,
) -> (u16, mpsc::UnboundedReceiver<Got>, mpsc::UnboundedReceiver<StreamMetadata>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    let (mtx, mrx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            let mtx = mtx.clone();
            tokio::spawn(async move {
                let mut first_ts: Option<u32> = None;
                let mut total_bytes: u64 = 0;
                let _ = serve_with(&mut sock, key, &tx, Some(&mtx), |ev| {
                    let over = match ev {
                        ServerSessionEvent::StreamMetadataChanged { metadata, .. } => {
                            let over_dims = match (metadata.video_width, metadata.video_height) {
                                (Some(w), Some(h)) => exceeds_dimensions(w, h, max_w, max_h),
                                (Some(w), None) => w > max_w.max(max_h),
                                (None, Some(h)) => h > max_w.min(max_h),
                                (None, None) => false,
                            };
                            let over_kbps = metadata.video_bitrate_kbps.is_some_and(|k| k > max_kbps);
                            over_dims || over_kbps
                        }
                        ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
                            if is_video_sequence_header(data) {
                                sps_dims(data).is_some_and(|(w, h)| exceeds_dimensions(w, h, max_w, max_h))
                            } else if let Some((w, h)) = sps_dims(data) {
                                exceeds_dimensions(w, h, max_w, max_h)
                            } else {
                                let start = *first_ts.get_or_insert(timestamp.value);
                                total_bytes = total_bytes.saturating_add(data.len() as u64);
                                let span = timestamp.value.wrapping_sub(start);
                                if (1000..u32::MAX / 2).contains(&span) {
                                    (total_bytes * 8 / u64::from(span)) > u64::from(max_kbps)
                                } else {
                                    false
                                }
                            }
                        }
                        _ => false,
                    };
                    over.then_some(Duration::from_millis(200))
                })
                .await;
                let _ = tx.send(Got::Closed);
            });
        }
    });
    (port, rx, mrx)
}

/// A fake platform that accepts publishing with `key` and closes the socket after `after`
/// (reproducing Kick rejecting multistreaming right after accepting publish).
pub async fn platform_dropping(key: &'static str, after: Duration) -> (u16, mpsc::UnboundedReceiver<Got>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let (itx, mut irx) = mpsc::unbounded_channel();
                let serve_fut = serve(&mut sock, key, &itx);
                let drop_fut = async {
                    while let Some(g) = irx.recv().await {
                        let published = matches!(g, Got::Publish(_));
                        let _ = tx.send(g);
                        if published {
                            break;
                        }
                    }
                    tokio::time::sleep(after).await;
                };
                tokio::select! {
                    _ = serve_fut => {}
                    _ = drop_fut => {}
                }
                let _ = tx.send(Got::Closed);
            });
        }
    });
    (port, rx)
}

/// A fake platform on 127.0.0.1 that enforces single-publisher exclusivity on `key`:
/// an overlapping `publish` before the previous TCP session closes is rejected with `BadName`
/// and recorded as `Err("overlap")`.
pub async fn platform_exclusive(key: &'static str) -> (u16, mpsc::UnboundedReceiver<Result<Got, &'static str>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            let busy = busy.clone();
            tokio::spawn(async move {
                let _ = serve_exclusive(&mut sock, key, &busy, &tx).await;
                let _ = tx.send(Ok(Got::Closed));
            });
        }
    });
    (port, rx)
}

async fn serve_exclusive(
    sock: &mut TcpStream,
    key: &str,
    busy: &std::sync::atomic::AtomicBool,
    tx: &mpsc::UnboundedSender<Result<Got, &'static str>>,
) -> Option<()> {
    use std::sync::atomic::Ordering;
    struct Release<'a>(&'a std::sync::atomic::AtomicBool, bool);
    impl Drop for Release<'_> {
        fn drop(&mut self) {
            if self.1 {
                self.0.store(false, Ordering::Release);
            }
        }
    }
    let rest = handshake(sock, PeerType::Server).await?;
    let (mut s, first) = ServerSession::new(ServerSessionConfig::new()).ok()?;
    let mut queue = first;
    queue.extend(s.handle_input(&rest).ok()?);
    let mut buf = vec![0u8; 65536];
    let mut guard = Release(busy, false);
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
                            if busy.swap(true, Ordering::AcqRel) {
                                let _ = tx.send(Err("overlap"));
                                for r in
                                    s.reject_request(request_id, "NetStream.Publish.BadName", "already live").ok()?
                                {
                                    if let ServerSessionResult::OutboundResponse(p) = r {
                                        sock.write_all(&p.bytes).await.ok()?;
                                    }
                                }
                                return None;
                            }
                            guard.1 = true;
                            let _ = tx.send(Ok(Got::Publish(stream_key)));
                            queue.extend(s.accept_request(request_id).ok()?);
                        }
                        ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
                            let _ = tx.send(Ok(Got::Video(timestamp.value, data)));
                        }
                        ServerSessionEvent::AudioDataReceived { data, timestamp, .. } => {
                            let _ = tx.send(Ok(Got::Audio(timestamp.value, data)));
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

async fn serve(sock: &mut TcpStream, key: &str, tx: &mpsc::UnboundedSender<Got>) -> Option<()> {
    serve_with(sock, key, tx, None, |_| None).await
}

async fn serve_with(
    sock: &mut TcpStream,
    key: &str,
    tx: &mpsc::UnboundedSender<Got>,
    meta_tx: Option<&mpsc::UnboundedSender<StreamMetadata>>,
    mut reject_after: impl FnMut(&ServerSessionEvent) -> Option<Duration>,
) -> Option<()> {
    let rest = handshake(sock, PeerType::Server).await?;
    let (mut s, first) = ServerSession::new(ServerSessionConfig::new()).ok()?;
    let mut queue = first;
    queue.extend(s.handle_input(&rest).ok()?);
    let mut buf = vec![0u8; 65536];
    let mut close_deadline: Option<tokio::time::Instant> = None;
    loop {
        while !queue.is_empty() {
            for r in std::mem::take(&mut queue) {
                match r {
                    ServerSessionResult::OutboundResponse(p) => sock.write_all(&p.bytes).await.ok()?,
                    ServerSessionResult::RaisedEvent(ev) => {
                        if close_deadline.is_none()
                            && let Some(delay) = reject_after(&ev)
                        {
                            close_deadline = Some(tokio::time::Instant::now() + delay);
                        }
                        match ev {
                            ServerSessionEvent::ConnectionRequested { request_id, .. } => {
                                queue.extend(s.accept_request(request_id).ok()?)
                            }
                            ServerSessionEvent::PublishStreamRequested { request_id, stream_key, .. } => {
                                if stream_key != key {
                                    for r in
                                        s.reject_request(request_id, "NetStream.Publish.BadName", "bad key").ok()?
                                    {
                                        if let ServerSessionResult::OutboundResponse(p) = r {
                                            sock.write_all(&p.bytes).await.ok()?;
                                        }
                                    }
                                    return None;
                                }
                                let _ = tx.send(Got::Publish(stream_key));
                                queue.extend(s.accept_request(request_id).ok()?);
                            }
                            ServerSessionEvent::StreamMetadataChanged { metadata, .. } => {
                                if let Some(m) = meta_tx {
                                    let _ = m.send(metadata);
                                }
                            }
                            ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
                                let _ = tx.send(Got::Video(timestamp.value, data));
                            }
                            ServerSessionEvent::AudioDataReceived { data, timestamp, .. } => {
                                let _ = tx.send(Got::Audio(timestamp.value, data));
                            }
                            _ => {}
                        }
                    }
                    ServerSessionResult::UnhandleableMessageReceived(_) => {}
                }
            }
        }
        let n = if let Some(deadline) = close_deadline {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return None,
                res = sock.read(&mut buf) => res.ok()?,
            }
        } else {
            sock.read(&mut buf).await.ok()?
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_dims(data: &[u8]) -> Option<(u32, u32)> {
        if data.get(..2) == Some(&[0x17, 0x00]) && data.len() >= 10 {
            let w = u32::from(u16::from_be_bytes([data[6], data[7]]));
            let h = u32::from(u16::from_be_bytes([data[8], data[9]]));
            return Some((w, h));
        }
        None
    }

    #[tokio::test]
    async fn platform_with_meta_forwards_decoded_metadata() {
        let (port, mut got, mut meta_rx) = platform_with_meta("mkey").await;
        let mut obs = Obs::publish(port, "live", "mkey").await.unwrap();
        let mut m = StreamMetadata::new();
        m.video_width = Some(2560);
        m.video_height = Some(1440);
        m.video_bitrate_kbps = Some(6000);
        let r = obs.c.publish_metadata(&m).unwrap();
        obs.send(vec![r]).await.unwrap();
        obs.video(0, &[0x17, 0x00, 0, 0, 0]).await;

        assert_eq!(next(&mut got).await, Got::Publish("mkey".into()));
        let seen = timeout(WAIT, meta_rx.recv()).await.unwrap().unwrap();
        assert_eq!(seen.video_width, Some(2560));
        assert_eq!(seen.video_height, Some(1440));
        assert_eq!(seen.video_bitrate_kbps, Some(6000));
    }

    #[tokio::test]
    async fn platform_rejecting_above_closes_on_oversized_sps_callback() {
        let (port, mut got, _meta) = platform_rejecting_above("rkey", 1920, 1080, 8000, stub_dims).await;
        let mut obs = Obs::publish(port, "live", "rkey").await.unwrap();
        let mut hdr = vec![0x17, 0x00, 0, 0, 0, 1];
        hdr.extend_from_slice(&2560u16.to_be_bytes());
        hdr.extend_from_slice(&1440u16.to_be_bytes());
        let r = obs.c.publish_video_data(Bytes::from(hdr), RtmpTimestamp::new(0), false).unwrap();
        obs.send(vec![r]).await.unwrap();

        assert_eq!(next(&mut got).await, Got::Publish("rkey".into()));
        loop {
            match timeout(WAIT, got.recv()).await.unwrap().unwrap() {
                Got::Closed => break,
                Got::Video(..) | Got::Audio(..) | Got::Publish(..) => {}
            }
        }
    }

    #[tokio::test]
    async fn platform_rejecting_above_does_not_count_non_avc_sequence_header_as_coded_frame() {
        let (port, mut got, _meta) = platform_rejecting_above("hkey", 1920, 1080, 1000, stub_dims).await;
        let mut obs = Obs::publish(port, "live", "hkey").await.unwrap();
        assert_eq!(next(&mut got).await, Got::Publish("hkey".into()));

        // Enhanced RTMP HEVC sequence header (stub_dims returns None) large enough to blow 1000 kbps
        // over 1.1s if erroneously added to coded video byte totals at ts=0.
        let mut hevc_hdr = vec![0x90, b'h', b'v', b'c', b'1'];
        hevc_hdr.resize(200_000, 0xAA);
        let r = obs.c.publish_video_data(Bytes::from(hevc_hdr), RtmpTimestamp::new(0), false).unwrap();
        obs.send(vec![r]).await.unwrap();

        // Two tiny coded frames 1.1 s and 1.4 s later (< 1 kbps coded rate).
        let coded = vec![0xA1, b'h', b'v', b'c', b'1', 0x00, 0x01, 0x02];
        let r1 = obs.c.publish_video_data(Bytes::from(coded.clone()), RtmpTimestamp::new(1100), false).unwrap();
        obs.send(vec![r1]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let r2 = obs.c.publish_video_data(Bytes::from(coded), RtmpTimestamp::new(1400), false).unwrap();
        obs.send(vec![r2]).await.unwrap();

        let mut saw_coded_1400 = false;
        while let Ok(Some(msg)) = timeout(WAIT, got.recv()).await {
            match msg {
                Got::Closed => panic!("non-AVC sequence header must not trip coded bitrate limit"),
                Got::Video(1400, _) => {
                    saw_coded_1400 = true;
                    break;
                }
                Got::Video(..) | Got::Audio(..) | Got::Publish(..) => {}
            }
        }
        assert!(saw_coded_1400, "expected coded frame at ts=1400");
    }
}
