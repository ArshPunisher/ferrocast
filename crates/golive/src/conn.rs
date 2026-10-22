//! The RTMP handshake over any byte stream (plain TCP or TLS), for both ends.

use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read buffer for RTMP traffic: big enough for a few chunks of a 1080p keyframe per read.
pub(crate) const READ_BUF: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum HandshakeFail {
    #[error("the connection closed during the handshake")]
    Closed,
    #[error("the other side doesn't speak RTMP")]
    NotRtmp,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Runs the handshake; returns the bytes that arrived after it (the start of the session).
pub(crate) async fn handshake<S>(s: &mut S, peer: PeerType) -> Result<Vec<u8>, HandshakeFail>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let client = peer == PeerType::Client;
    let mut hs = Handshake::new(peer);
    if client {
        let p0p1 = hs.generate_outbound_p0_and_p1().map_err(|_| HandshakeFail::NotRtmp)?;
        s.write_all(&p0p1).await?;
    }
    let mut buf = vec![0u8; 4096];
    loop {
        let n = s.read(&mut buf).await?;
        if n == 0 {
            return Err(HandshakeFail::Closed);
        }
        match hs.process_bytes(&buf[..n]).map_err(|_| HandshakeFail::NotRtmp)? {
            HandshakeProcessResult::InProgress { response_bytes } => {
                if !response_bytes.is_empty() {
                    s.write_all(&response_bytes).await?;
                }
            }
            HandshakeProcessResult::Completed { response_bytes, remaining_bytes } => {
                if !response_bytes.is_empty() {
                    s.write_all(&response_bytes).await?;
                }
                return Ok(remaining_bytes);
            }
        }
    }
}

/// Compares two secrets in time that doesn't depend on where they differ.
pub(crate) fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
