//! RIST sends (VSF TR-06): MPEG-TS in RTP, packets the receiver says it lost (RTCP NACKs,
//! either kind) sent again from the last two seconds.
//!
//! - Simple profile (TR-06-1): RTP to the receiver's even port, the sender's reports (RTCP) to
//!   the port after it.
//! - Main profile (TR-06-2, as librist sends it): RTP and RTCP both in GRE to one port, and with
//!   a passphrase encrypted (AES-CTR, the key made from the passphrase and a random nonce by
//!   PBKDF2-HMAC-SHA256). Picked by `profile=1`, or by a passphrase (the key, or `secret=`);
//!   `aes-type=` 128 (the default) or 256.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
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
    rtp: Arc<UdpSocket>,
    to: SocketAddr,
    kept: Mutex<VecDeque<Sent>>,
    ssrc: u32,
    /// The main profile's GRE (and encryption); None for the simple profile.
    main: Option<Main>,
}

impl Shared {
    /// An RTP packet as it goes on the wire.
    async fn send_rtp(&self, p: &[u8]) -> std::io::Result<usize> {
        match &self.main {
            Some(m) => self.rtp.send_to(&m.wrap(DATA_SRC_PORT, DATA_DST_PORT, p), self.to).await,
            None => self.rtp.send_to(p, self.to).await,
        }
    }
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
    let main = Main::of(d, &a);
    let rtp = Arc::new(UdpSocket::bind(any).await.map_err(|_| DirectError::Unreachable(name.clone()))?);
    // The main profile sends its reports on the same port; the simple one on the next.
    let (rtcp, rtcp_to) = if main.is_some() {
        (rtp.clone(), to)
    } else {
        let rtcp = UdpSocket::bind(any).await.map_err(|_| DirectError::Unreachable(name.clone()))?;
        (Arc::new(rtcp), SocketAddr::new(to.ip(), to.port() + 1))
    };
    // Retransmissions use the same SSRC with its lowest bit set (TR-06-1).
    let ssrc = fastrand::u32(..) & !1;
    let shared = Arc::new(Shared { rtp, to, kept: Mutex::new(VecDeque::with_capacity(KEPT)), ssrc, main });
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

/// The passphrase: the key, or `secret=` in the address.
fn passphrase<'a>(d: &'a Direct, a: &'a Address) -> Option<&'a str> {
    Some(d.key.trim()).filter(|k| !k.is_empty()).or_else(|| a.param("secret").filter(|s| !s.is_empty()))
}

/// Whether the address asks for the main profile (or a passphrase needs it).
fn wants_main(d: &Direct, a: &Address) -> bool {
    passphrase(d, a).is_some() || a.param("profile").is_some_and(|p| p == "1" || p.eq_ignore_ascii_case("main"))
}

/// What can't work whatever the network does: an unknown profile, a bad key size or passphrase,
/// an odd port for the simple profile.
pub(crate) fn check(d: &Direct, a: &Address) -> Result<(), DirectError> {
    if let Some(p) =
        a.param("profile").filter(|p| !matches!(p.to_ascii_lowercase().as_str(), "0" | "1" | "simple" | "main"))
    {
        return Err(DirectError::Address(format!(
            "Ferrocast sends RIST's simple (profile=0) or main (profile=1) profile, not profile={p}."
        )));
    }
    if let Some(bits) = a.param("aes-type").filter(|b| !matches!(*b, "128" | "256")) {
        return Err(DirectError::Address(format!("RIST's aes-type is 128 or 256, not {bits}.")));
    }
    if passphrase(d, a).is_some_and(|p| p.len() > 127) {
        return Err(DirectError::Address("A RIST passphrase is at most 127 letters long.".into()));
    }
    if a.param("profile").is_some_and(|p| p == "0" || p.eq_ignore_ascii_case("simple")) && passphrase(d, a).is_some() {
        return Err(DirectError::Address(
            "RIST's simple profile can't be encrypted: leave profile=0 out to send the main profile with the passphrase.".into(),
        ));
    }
    if !wants_main(d, a) && a.port % 2 == 1 {
        return Err(DirectError::Address(
            "RIST's simple profile sends to an even port (the next one is for reports).".into(),
        ));
    }
    Ok(())
}

/// The main profile's virtual ports for the stream (librist's defaults).
const DATA_SRC_PORT: u16 = 1971;
const DATA_DST_PORT: u16 = 1968;
/// GRE's protocol for RIST's reduced-overhead header, and the VSF one newer senders use.
const GRE_REDUCED: u16 = 0x88B6;
const GRE_VSF: u16 = 0xCCE0;
/// librist's PBKDF2 rounds for the AES key.
const PBKDF2_ROUNDS: u32 = 1024;
/// The shortest time between two keys made for the receiver's nonces.
const REKEY_EVERY: Duration = Duration::from_millis(10);

/// The main profile's GRE wrapping, and with a passphrase its encryption.
struct Main {
    crypt: Option<Crypt>,
    /// GRE's sequence number: one count over every packet sent (the AES counter's top half).
    seq: AtomicU32,
}

/// A nonce, its key, and when the key was made.
type RxKey = ([u8; 4], Vec<u8>, Instant);

struct Crypt {
    pass: Vec<u8>,
    /// AES key size in bytes (16 or 32).
    size: usize,
    /// Ours: a random nonce (top bit clear: the "even" key) and its key.
    tx: ([u8; 4], Vec<u8>),
    /// The receiver's last nonce and its key (its reports come encrypted with its own), and when
    /// it was made.
    rx: std::sync::Mutex<Option<RxKey>>,
}

impl Main {
    fn of(d: &Direct, a: &Address) -> Option<Self> {
        if !wants_main(d, a) {
            return None;
        }
        let crypt = passphrase(d, a).map(|pass| {
            let size = if a.param("aes-type") == Some("256") { 32 } else { 16 };
            let mut nonce = [0u8; 4];
            while nonce == [0; 4] {
                nonce = fastrand::u32(..).to_be_bytes();
                nonce[0] &= 0x7F;
            }
            let key = aes_key(pass.as_bytes(), &nonce, size);
            Crypt { pass: pass.as_bytes().to_vec(), size, tx: (nonce, key), rx: std::sync::Mutex::new(None) }
        });
        Some(Self { crypt, seq: AtomicU32::new(fastrand::u32(..)) })
    }

    /// `inner` (RTP or RTCP) as a GRE packet (version 1, reduced header with the virtual ports).
    fn wrap(&self, src_port: u16, dst_port: u16, inner: &[u8]) -> Vec<u8> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut p = Vec::with_capacity(16 + inner.len());
        // Flags: sequence number (bit 4), key (bit 5); version 1, and H for a 256-bit key.
        let (flags1, flags2) = match &self.crypt {
            Some(c) => (0x30, 1 << 3 | if c.size == 32 { 0x40 } else { 0 }),
            None => (0x10, 1 << 3),
        };
        p.extend_from_slice(&[flags1, flags2]);
        p.extend_from_slice(&GRE_REDUCED.to_be_bytes());
        if let Some(c) = &self.crypt {
            p.extend_from_slice(&c.tx.0);
        }
        p.extend_from_slice(&seq.to_be_bytes());
        let body = p.len();
        p.extend_from_slice(&src_port.to_be_bytes());
        p.extend_from_slice(&dst_port.to_be_bytes());
        p.extend_from_slice(inner);
        if let Some(c) = &self.crypt {
            aes_ctr(&c.tx.1, seq, &mut p[body..]);
        }
        p
    }

    /// What a GRE packet from the receiver carries (its RTCP), decrypted; None for anything
    /// else (keep-alives, other protocols, what doesn't decrypt).
    fn unwrap(&self, p: &[u8]) -> Option<Vec<u8>> {
        let (flags1, flags2) = (*p.first()?, *p.get(1)?);
        let proto = u16::from_be_bytes([*p.get(2)?, *p.get(3)?]);
        if flags1 & 0x40 != 0 || flags2 & 0x07 != 0 {
            return None;
        }
        let mut at = 4 + if flags1 & 0x80 != 0 { 4 } else { 0 };
        let nonce: Option<[u8; 4]> = if flags1 & 0x20 != 0 {
            let n = p.get(at..at + 4)?.try_into().ok()?;
            at += 4;
            Some(n)
        } else {
            None
        };
        let seq = if flags1 & 0x10 != 0 {
            let s = u32::from_be_bytes(p.get(at..at + 4)?.try_into().ok()?);
            at += 4;
            Some(s)
        } else {
            None
        };
        let mut body = p.get(at..)?.to_vec();
        match (&self.crypt, nonce, seq) {
            (Some(c), Some(nonce), Some(seq)) if nonce != [0; 4] => {
                let mut rx = c.rx.lock().unwrap_or_else(|e| e.into_inner());
                if rx.as_ref().is_none_or(|(n, _, _)| *n != nonce) {
                    // A new key at most every 10 ms, as librist (PBKDF2 isn't free).
                    if rx.as_ref().is_some_and(|(_, _, at)| at.elapsed() < REKEY_EVERY) {
                        return None;
                    }
                    *rx = Some((nonce, aes_key(&c.pass, &nonce, c.size), Instant::now()));
                }
                let (_, key, _) = rx.as_ref()?;
                aes_ctr(key, seq, &mut body);
            }
            // Clear when we expect encryption (or the other way round): not ours.
            (Some(_), _, _) | (None, Some(_), _) => return None,
            (None, None, _) => {}
        }
        let body = match proto {
            GRE_REDUCED => body,
            // VSF: type 0 (RIST), subtype 0 (reduced); control messages aren't for us.
            GRE_VSF if body.get(..4) == Some(&[0, 0, 0, 0]) => body.split_off(4),
            _ => return None,
        };
        // The reduced header: the virtual ports.
        (body.len() > 4).then(|| body[4..].to_vec())
    }
}

/// librist's AES key: PBKDF2-HMAC-SHA256 of the passphrase, salted with the nonce.
fn aes_key(pass: &[u8], nonce: &[u8; 4], size: usize) -> Vec<u8> {
    let mut key = vec![0u8; size];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(pass, nonce, PBKDF2_ROUNDS, &mut key);
    key
}

/// AES-CTR in place, the counter starting at the GRE sequence number in its top four bytes.
fn aes_ctr(key: &[u8], seq: u32, data: &mut [u8]) {
    use ctr::cipher::{KeyIvInit, StreamCipher};
    let mut iv = [0u8; 16];
    iv[..4].copy_from_slice(&seq.to_be_bytes());
    match key.len() {
        32 => ctr::Ctr128BE::<aes::Aes256>::new(key.into(), &iv.into()).apply_keystream(data),
        _ => ctr::Ctr128BE::<aes::Aes128>::new(key.into(), &iv.into()).apply_keystream(data),
    }
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
        self.shared.send_rtp(&p).await.map_err(|_| DirectError::Broken(self.name.clone()))?;
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
async fn answer(shared: Arc<Shared>, rtcp: Arc<UdpSocket>, rtcp_to: SocketAddr) {
    let started = Instant::now();
    let mut tick = tokio::time::interval(REPORT_EVERY);
    let mut buf = vec![0u8; 2048];
    // The main profile's reports go in GRE, from and to the real ports.
    let here = rtcp.local_addr().map(|a| a.port()).unwrap_or(0);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let r = report(shared.ssrc, started);
                let r = match &shared.main {
                    Some(m) => m.wrap(here, rtcp_to.port(), &r),
                    None => r,
                };
                let _ = rtcp.send_to(&r, rtcp_to).await;
            }
            r = rtcp.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                // Only the receiver's answers (a stranger's packets could make us work for nothing).
                if from.ip() != rtcp_to.ip() {
                    continue;
                }
                let got = match &shared.main {
                    Some(m) => match m.unwrap(&buf[..n]) {
                        Some(inner) => inner,
                        None => continue,
                    },
                    None => buf[..n].to_vec(),
                };
                let want = lost(&got);
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
                    let _ = shared.send_rtp(&p).await;
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

    fn direct(url: &str, key: &str) -> Direct {
        Direct { id: "r".into(), kind: Kind::Rist, url: url.into(), key: key.into(), retry: Retry::default() }
    }

    #[test]
    fn what_cant_work_is_refused_plainly() {
        let c =
            |url: &str, key: &str| check(&direct(url, key), &Address::parse(url).unwrap()).err().map(|e| e.to_string());
        assert_eq!(c("rist://127.0.0.1:5000", "secret"), None, "a passphrase sends the main profile");
        assert_eq!(c("rist://127.0.0.1:5001?profile=1", ""), None, "the main profile takes any port");
        assert!(c("rist://127.0.0.1:5001", "").is_some_and(|e| e.contains("even port")));
        assert!(c("rist://127.0.0.1:5000?profile=2", "").is_some_and(|e| e.contains("profile=2")));
        assert!(c("rist://127.0.0.1:5000?secret=abc&aes-type=192", "").is_some_and(|e| e.contains("aes-type")));
        assert!(c("rist://127.0.0.1:5000?profile=0", "secret").is_some_and(|e| e.contains("simple profile")));
    }

    #[test]
    fn main_profile_packets_open_with_the_same_passphrase_only() {
        let a = |url: &str| Address::parse(url).unwrap();
        for bits in ["128", "256"] {
            let url = format!("rist://h:5000?secret=pass-word-1&aes-type={bits}");
            let (ours, theirs) =
                (Main::of(&direct(&url, ""), &a(&url)).unwrap(), Main::of(&direct(&url, ""), &a(&url)).unwrap());
            let inner = b"an RTCP packet, more or less".to_vec();
            let wire = ours.wrap(1971, 1968, &inner);
            assert_eq!(wire[0], 0x30, "sequence and key");
            assert_eq!(wire[1] & 0x40 != 0, bits == "256", "H says a 256-bit key");
            assert!(!wire.windows(inner.len()).any(|w| w == inner), "encrypted");
            assert_eq!(theirs.unwrap(&wire), Some(inner.clone()));
            let wrong = format!("rist://h:5000?secret=other-word&aes-type={bits}");
            let other = Main::of(&direct(&wrong, ""), &a(&wrong)).unwrap();
            assert_ne!(other.unwrap(&wire), Some(inner.clone()));
        }
        // No passphrase: GRE in the clear.
        let url = "rist://h:5000?profile=1";
        let m = Main::of(&direct(url, ""), &a(url)).unwrap();
        let wire = m.wrap(1, 2, b"x1234");
        assert_eq!(&wire[..4], &[0x10, 0x08, 0x88, 0xB6]);
        assert_eq!(m.unwrap(&wire), Some(b"x1234".to_vec()));
    }

    /// The main profile with a passphrase: one port, everything encrypted, and what the
    /// receiver loses (asked for in encrypted reports) comes back.
    #[tokio::test]
    async fn main_profile_lost_packets_come_back_encrypted() {
        let Some(tags) = crate::ts::tests::sample_tags() else { return };
        let Ok(sock) = UdpSocket::bind("127.0.0.1:0").await else { return };
        let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
        let url = format!("rist://127.0.0.1:{port}?aes-type=256");
        let them = Main::of(&direct(&url, "pass-word-1"), &Address::parse(&url).unwrap()).unwrap();
        let receiver = tokio::spawn(async move {
            let mut got: std::collections::BTreeMap<u16, Vec<u8>> = Default::default();
            let mut buf = vec![0u8; 2048];
            let mut first: Option<u16> = None;
            let (mut dropped, mut retx, mut n) = (0u32, 0u32, 0u32);
            let mut quiet = Box::pin(tokio::time::sleep(Duration::from_secs(10)));
            loop {
                tokio::select! {
                    r = sock.recv_from(&mut buf) => {
                        let Ok((len, from)) = r else { break };
                        let Some(p) = them.unwrap(&buf[..len]) else { continue };
                        // Reports (RTCP 200-204) aren't the stream.
                        if (200..=204).contains(&p[1]) {
                            continue;
                        }
                        quiet.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(1500));
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
                            let mut nack = vec![0x81, 205, 0, 3, 0, 0, 0, 1];
                            nack.extend_from_slice(&p[8..12]);
                            nack.extend_from_slice(&[p[2], p[3], 0, 0]);
                            let _ = sock.send_to(&them.wrap(1968, 1971, &nack), from).await;
                            continue;
                        }
                        got.insert(at, p[12..].to_vec());
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
        let sender = tokio::spawn(run(direct(&url, "pass-word-1"), hub.clone(), health, stop));
        let mut h2 = h.clone();
        let _ = tokio::time::timeout(Duration::from_secs(5), h2.wait_for(|h| h.state == PushState::Live)).await;
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
