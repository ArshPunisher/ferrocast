//! RIST sends, simple profile (VSF TR-06-1): MPEG-TS in RTP over UDP to the receiver's even
//! port, the sender's reports (RTCP) to the port after it, and packets the receiver says it
//! lost (RTCP NACKs, either kind) sent again from the last two seconds. The main profile's
//! encryption isn't here: a passphrase is refused with that said.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, watch};

use crate::direct::{Address, Direct, DirectError, TsSink, feed_ts};
use crate::hub::Hub;
use crate::push::{Ended, Health};

/// RTP's payload type for MPEG-TS.
const MP2T: u8 = 33;
/// Packets kept to send again: about two seconds at 20 Mbps.
const KEPT: usize = 4_096;
/// How often the sender reports (RTCP SR and its name).
const REPORT_EVERY: Duration = Duration::from_millis(250);

/// One RTP packet: its number and its bytes.
type Sent = (u16, Bytes);

/// What both the sending and the answering side reach.
struct Shared {
    rtp: UdpSocket,
    to: SocketAddr,
    kept: Mutex<VecDeque<Sent>>,
    ssrc: u32,
}

pub(crate) async fn session(
    d: &Direct,
    hub: &Hub,
    health: &watch::Sender<Health>,
    stream: u64,
) -> Result<Ended, DirectError> {
    let a = Address::parse(&d.url)?;
    check(d, &a)?;
    let name = format!("{}:{}", a.host, a.port);
    let to = tokio::net::lookup_host((a.host.as_str(), a.port))
        .await
        .ok()
        .and_then(|mut it| it.next())
        .ok_or_else(|| DirectError::Unreachable(name.clone()))?;
    let any: SocketAddr = if to.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }
        .parse()
        .map_err(|_| DirectError::Unreachable(name.clone()))?;
    let rtp = UdpSocket::bind(any).await.map_err(|_| DirectError::Unreachable(name.clone()))?;
    let rtcp = UdpSocket::bind(any).await.map_err(|_| DirectError::Unreachable(name.clone()))?;
    let rtcp_to = SocketAddr::new(to.ip(), to.port() + 1);
    // Retransmissions use the same SSRC with its lowest bit set (TR-06-1).
    let ssrc = fastrand::u32(..) & !1;
    let shared = Arc::new(Shared { rtp, to, kept: Mutex::new(VecDeque::with_capacity(KEPT)), ssrc });
    let mut sink = RistSink { shared: shared.clone(), seq: fastrand::u16(..), started: Instant::now(), name };
    // Stopped however the session ends (a stop drops this future mid-send).
    let _answers = AbortOnDrop(tokio::spawn(answer(shared.clone(), rtcp, rtcp_to)));
    feed_ts(hub, health, stream, &mut sink).await
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What can't work whatever the network does: a passphrase, the main profile, an odd port.
pub(crate) fn check(d: &Direct, a: &Address) -> Result<(), DirectError> {
    if !d.key.trim().is_empty() {
        return Err(DirectError::Address(
            "RIST with a passphrase needs RIST's main profile, which Ferrocast doesn't send yet. Leave the key empty and set the receiver to the simple profile.".into(),
        ));
    }
    if let Some(p) = a.param("profile").filter(|p| *p != "0" && !p.eq_ignore_ascii_case("simple")) {
        return Err(DirectError::Address(format!(
            "Ferrocast sends RIST's simple profile (profile=0), not profile={p} yet."
        )));
    }
    if a.port % 2 == 1 {
        return Err(DirectError::Address(
            "RIST's simple profile sends to an even port (the next one is for reports).".into(),
        ));
    }
    Ok(())
}

struct RistSink {
    shared: Arc<Shared>,
    seq: u16,
    started: Instant,
    name: String,
}

impl TsSink for RistSink {
    async fn send(&mut self, chunk: Bytes) -> Result<(), DirectError> {
        let ts = (self.started.elapsed().as_micros() * 9 / 100) as u32;
        let p = rtp(MP2T, self.seq, ts, self.shared.ssrc, &chunk);
        self.shared.rtp.send_to(&p, self.shared.to).await.map_err(|_| DirectError::Broken(self.name.clone()))?;
        {
            let mut kept = self.shared.kept.lock().await;
            if kept.len() == KEPT {
                kept.pop_front();
            }
            kept.push_back((self.seq, Bytes::from(p)));
        }
        self.seq = self.seq.wrapping_add(1);
        Ok(())
    }
}

/// An RTP packet.
fn rtp(pt: u8, seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + payload.len());
    p.extend_from_slice(&[0x80, pt & 0x7F]);
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

/// The sender's report (an empty SR, as RIST receivers need only the clock pairing and its
/// name) and SDES with its CNAME, in one compound packet.
fn report(ssrc: u32, started: Instant) -> Vec<u8> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    // NTP time: seconds since 1900, and the fraction.
    let secs = now.as_secs() + 2_208_988_800;
    let frac = ((u64::from(now.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    let rtp_ts = (started.elapsed().as_micros() * 9 / 100) as u32;
    let mut p = vec![0x80, 200, 0, 6];
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(&(secs as u32).to_be_bytes());
    p.extend_from_slice(&frac.to_be_bytes());
    p.extend_from_slice(&rtp_ts.to_be_bytes());
    p.extend_from_slice(&[0; 8]);
    let cname = b"ferrocast";
    // SDES: one chunk, CNAME, padded to a 32-bit boundary.
    let mut chunk = ssrc.to_be_bytes().to_vec();
    chunk.extend_from_slice(&[1, cname.len() as u8]);
    chunk.extend_from_slice(cname);
    chunk.push(0);
    while !chunk.len().is_multiple_of(4) {
        chunk.push(0);
    }
    p.extend_from_slice(&[0x81, 202]);
    p.extend_from_slice(&((chunk.len() / 4) as u16).to_be_bytes());
    p.extend_from_slice(&chunk);
    p
}

/// The packet numbers an RTCP compound packet asks for again: generic NACKs (205, FMT 1:
/// a number and a mask of the 16 after it) and RIST's range NACKs (APP 204 "RIST": a number
/// and how many follow it).
pub(crate) fn lost(buf: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= buf.len() {
        let (b0, pt) = (buf[at], buf[at + 1]);
        let words = usize::from(u16::from_be_bytes([buf[at + 2], buf[at + 3]]));
        let end = (at + 4 + words * 4).min(buf.len());
        let body = &buf[at + 4..end];
        match (pt, b0 & 0x1F) {
            (205, 1) if body.len() >= 8 => {
                for fci in body[8..].as_chunks::<4>().0 {
                    let pid = u16::from_be_bytes([fci[0], fci[1]]);
                    let blp = u16::from_be_bytes([fci[2], fci[3]]);
                    out.push(pid);
                    out.extend((0..16).filter(|i| blp & (1 << i) != 0).map(|i| pid.wrapping_add(i + 1)));
                }
            }
            (204, 0) if body.len() >= 8 && &body[4..8] == b"RIST" => {
                for r in body[8..].as_chunks::<4>().0 {
                    let start = u16::from_be_bytes([r[0], r[1]]);
                    let more = u16::from_be_bytes([r[2], r[3]]).min(512);
                    out.extend((0..=more).map(|i| start.wrapping_add(i)));
                }
            }
            _ => {}
        }
        if words == 0 && end == at + 4 && pt == 0 {
            break;
        }
        at = end;
    }
    out
}

/// Sends reports, and sends again what the receiver asks for.
async fn answer(shared: Arc<Shared>, rtcp: UdpSocket, rtcp_to: SocketAddr) {
    let started = Instant::now();
    let mut tick = tokio::time::interval(REPORT_EVERY);
    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let _ = rtcp.send_to(&report(shared.ssrc, started), rtcp_to).await;
            }
            r = rtcp.recv_from(&mut buf) => {
                let Ok((n, _from)) = r else { continue };
                let want = lost(&buf[..n]);
                if want.is_empty() {
                    continue;
                }
                let again: Vec<Bytes> = {
                    let kept = shared.kept.lock().await;
                    want.iter().filter_map(|s| kept.iter().find(|(seq, _)| seq == s).map(|(_, p)| p.clone())).collect()
                };
                for p in again {
                    // The same packet, its SSRC's lowest bit set: a retransmission.
                    let mut p = p.to_vec();
                    p[11] |= 1;
                    let _ = shared.rtp.send_to(&p, shared.to).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direct::{Kind, run};
    use crate::push::{PushState, Retry};

    #[test]
    fn both_kinds_of_nack_are_read() {
        // Generic NACK: packet 100 and the 1st and 3rd after it.
        let mut nack = vec![0x81, 205, 0, 3, 0, 0, 0, 1, 0, 0, 0, 2];
        nack.extend_from_slice(&[0, 100, 0b0000_0000, 0b0000_0101]);
        assert_eq!(lost(&nack), [100, 101, 103]);
        // RIST's range NACK: 65534 and the 3 after it (round the wrap).
        let mut range = vec![0x80, 204, 0, 3, 0, 0, 0, 1];
        range.extend_from_slice(b"RIST");
        range.extend_from_slice(&[0xFF, 0xFE, 0, 3]);
        assert_eq!(lost(&range), [65534, 65535, 0, 1]);
        // In a compound packet after a receiver report.
        let mut both = vec![0x80, 201, 0, 1, 0, 0, 0, 9];
        both.extend_from_slice(&nack);
        assert_eq!(lost(&both), [100, 101, 103]);
        assert!(lost(&[0x80, 200, 0, 0]).is_empty());
    }

    #[test]
    fn a_passphrase_or_main_profile_is_refused_plainly() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap_or_else(|e| panic!("{e}"));
        let hub = Hub::new();
        let (health, _) =
            watch::channel(Health { state: PushState::Waiting, kbps: 0, dropped: 0, sent: 0, since_ms: 0 });
        let d = |url: &str, key: &str| Direct {
            id: "r".into(),
            kind: Kind::Rist,
            url: url.into(),
            key: key.into(),
            retry: Retry::default(),
        };
        let e = rt.block_on(session(&d("rist://127.0.0.1:5000", "secret"), &hub, &health, 1)).err();
        assert!(e.is_some_and(|e| e.to_string().contains("main profile")));
        let e = rt.block_on(session(&d("rist://127.0.0.1:5000?profile=1", ""), &hub, &health, 1)).err();
        assert!(e.is_some_and(|e| e.to_string().contains("simple profile")));
        let e = rt.block_on(session(&d("rist://127.0.0.1:5001", ""), &hub, &health, 1)).err();
        assert!(e.is_some_and(|e| e.to_string().contains("even port")));
    }

    /// A receiver that loses every tenth packet asks for it back, and ends with the whole
    /// stream: TS FFmpeg reads.
    #[tokio::test]
    async fn lost_packets_come_back() {
        let Some(tags) = crate::ts::tests::sample_tags() else { return };
        // An even port and the one after it.
        let (rtp, rtcp) = loop {
            let Ok(a) = std::net::UdpSocket::bind("127.0.0.1:0") else { return };
            let Ok(port) = a.local_addr().map(|a| a.port()) else { return };
            if port % 2 == 1 || port == u16::MAX {
                continue;
            }
            if let Ok(b) = std::net::UdpSocket::bind(("127.0.0.1", port + 1)) {
                let _ = (a.set_nonblocking(true), b.set_nonblocking(true));
                match (UdpSocket::from_std(a), UdpSocket::from_std(b)) {
                    (Ok(a), Ok(b)) => break (a, b),
                    _ => return,
                }
            }
        };
        let port = rtp.local_addr().map(|a| a.port()).unwrap_or(0);
        let receiver = tokio::spawn(async move {
            let mut got: std::collections::BTreeMap<u16, Vec<u8>> = Default::default();
            let (mut buf, mut cbuf) = (vec![0u8; 2048], vec![0u8; 2048]);
            let mut sender_rtcp = None;
            // Packet numbers counted from the first (they may wrap round 65535).
            let mut first: Option<u16> = None;
            let (mut dropped, mut retx, mut n) = (0u32, 0u32, 0u32);
            // Done 1.5 s after the last picture or sound packet (reports keep coming).
            let mut quiet = Box::pin(tokio::time::sleep(Duration::from_secs(10)));
            loop {
                tokio::select! {
                    r = rtp.recv_from(&mut buf) => {
                        let Ok((len, _)) = r else { break };
                        quiet.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(1500));
                        let p = &buf[..len];
                        let seq = u16::from_be_bytes([p[2], p[3]]);
                        let at = seq.wrapping_sub(*first.get_or_insert(seq));
                        if p[11] & 1 == 1 {
                            retx += 1;
                            got.insert(at, p[12..].to_vec());
                            continue;
                        }
                        n += 1;
                        if n % 10 == 0 {
                            dropped += 1;
                            if let Some(to) = sender_rtcp {
                                let mut nack = vec![0x81, 205, 0, 3, 0, 0, 0, 1];
                                nack.extend_from_slice(&p[8..12]);
                                nack.extend_from_slice(&[p[2], p[3], 0, 0]);
                                let _ = rtcp.send_to(&nack, to).await;
                            }
                            continue;
                        }
                        got.insert(at, p[12..].to_vec());
                    }
                    r = rtcp.recv_from(&mut cbuf) => {
                        if let Ok((_, from)) = r { sender_rtcp = Some(from); }
                    }
                    _ = &mut quiet => break,
                }
            }
            (got, dropped, retx)
        });
        let hub = Hub::new();
        hub.start();
        let (health, h) =
            watch::channel(Health { state: PushState::Waiting, kbps: 0, dropped: 0, sent: 0, since_ms: 0 });
        let (stop_tx, stop) = watch::channel(false);
        let d = Direct {
            id: "r".into(),
            kind: Kind::Rist,
            url: format!("rist://127.0.0.1:{port}"),
            key: String::new(),
            retry: Retry::default(),
        };
        let sender = tokio::spawn(run(d, hub.clone(), health, stop));
        let mut h2 = h.clone();
        let _ = tokio::time::timeout(Duration::from_secs(5), h2.wait_for(|h| h.state == PushState::Live)).await;
        // Reports first, so the receiver knows where to ask.
        tokio::time::sleep(Duration::from_millis(400)).await;
        for (kind, ms, body) in tags {
            match kind {
                9 => hub.video(ms, body.into()),
                8 => hub.audio(ms, body.into()),
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let (got, dropped, retx) = receiver.await.unwrap_or_default();
        hub.end();
        let _ = stop_tx.send(true);
        let _ = sender.await;
        assert!(dropped > 3, "some were lost on purpose: {dropped}");
        assert_eq!(retx, dropped, "every lost packet came back");
        let ts: Vec<u8> = got.into_values().flatten().collect();
        if let Some(p) = crate::ts::tests::probe(&ts) {
            assert!(p.starts_with("h264,90") && p.contains("aac,") && !p.contains(" / "), "{p}");
        }
    }
}
