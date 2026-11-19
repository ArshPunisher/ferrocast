//! RIST sends (VSF TR-06): MPEG-TS in RTP, packets the receiver says it lost (RTCP NACKs,
//! either kind) sent again from the last two seconds.
//!
//! - Simple profile (TR-06-1): RTP to the receiver's even port, the sender's reports (RTCP) to
//!   the port after it.
//! - Main profile (TR-06-2, as librist sends it): RTP and RTCP both in GRE to one port, and with
//!   a passphrase encrypted (AES-CTR, the key made from the passphrase and a random nonce by
//!   PBKDF2-HMAC-SHA256). Picked by `profile=1`, or by a passphrase (the key, or `secret=`);
//!   `aes-type=` 128 or 256 (the default, as librist).
//!
//! The main profile also logs in with `username=` (the key its password), as librist's EAP-SRP
//! does: the stream waits for the login, and a `secret=` beside it still encrypts.
//!
//! Either profile goes over two connections at once with `via=` (`via=en0,en7`): every packet
//! by each, the receiver keeping the first copy (SMPTE 2022-7 style; librist merges them), so
//! the stream rides out one of them dropping.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, watch};

use crate::direct::{Address, Direct, DirectError, TsSink, feed_ts};
use crate::hub::Hub;
use crate::push::{Ended, Health, Link};

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
    /// One socket per connection the stream goes over (`via=`), each packet sent by all.
    rtp: Vec<Arc<UdpSocket>>,
    to: SocketAddr,
    kept: Mutex<VecDeque<Sent>>,
    ssrc: u32,
    /// Stream packets sent, asked for again, and sent again (for the link's health).
    sent: AtomicU64,
    asked: AtomicU64,
    resent: AtomicU64,
    /// The main profile's GRE (and encryption); None for the simple profile.
    main: Option<Main>,
}

impl Shared {
    /// An RTP packet as it goes on the wire, over every connection: fine while one of them
    /// takes it.
    async fn send_rtp(&self, p: &[u8]) -> std::io::Result<()> {
        let wrapped;
        let p = match &self.main {
            // Not a packet before logging in: none goes out in the clear.
            Some(m) if !m.ready() => return Ok(()),
            Some(m) => {
                wrapped = m.wrap(DATA_SRC_PORT, DATA_DST_PORT, p);
                &wrapped[..]
            }
            None => p,
        };
        let mut last = Ok(());
        let mut any = false;
        for s in &self.rtp {
            match s.send_to(p, self.to).await {
                Ok(_) => any = true,
                Err(e) => last = Err(e),
            }
        }
        if any { Ok(()) } else { last }
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
    let main = Main::of(d, &a);
    // The main profile sends its reports on the same port; the simple one on the next.
    let rtcp_to = if main.is_some() { to } else { SocketAddr::new(to.ip(), to.port() + 1) };
    let (mut rtp, mut rtcp) = (Vec::new(), Vec::new());
    for via in vias(&a) {
        let pair = async {
            let r = Arc::new(socket(via.as_deref(), to).await?);
            let c = if main.is_some() { r.clone() } else { Arc::new(socket(via.as_deref(), to).await?) };
            Ok::<_, std::io::Error>((r, c))
        };
        match pair.await {
            Ok((r, c)) => {
                rtp.push(r);
                rtcp.push(c);
            }
            // A connection that isn't up now (the hotspot unplugged): the others carry it.
            Err(e) => {
                tracing::info!(dest = %d.id, via = via.as_deref().unwrap_or("default"), error = %e, "RIST: a connection isn't there")
            }
        }
    }
    if rtp.is_empty() {
        return Err(DirectError::Unreachable(format!("{name} (none of the connections in via= is up)")));
    }
    // Retransmissions use the same SSRC with its lowest bit set (TR-06-1).
    let ssrc = fastrand::u32(..) & !1;
    let shared = Arc::new(Shared {
        rtp,
        to,
        kept: Mutex::new(VecDeque::with_capacity(KEPT)),
        ssrc,
        sent: AtomicU64::new(0),
        asked: AtomicU64::new(0),
        resent: AtomicU64::new(0),
        main,
    });
    let mut sink = RistSink { shared: shared.clone(), seq: fastrand::u16(..), started: Instant::now(), name };
    // Answering runs beside sending, and stops with it however the session ends (or ends it,
    // when the receiver refuses the login).
    tokio::select! {
        out = feed_ts(hub, health, stream, &mut sink) => out,
        e = answer(shared.clone(), rtcp, rtcp_to, health) => Err(e),
    }
}

/// The connections to send over: `via=` names them (or their addresses), comma between;
/// without it, whichever the system picks (`None`).
fn vias(a: &Address) -> Vec<Option<String>> {
    let named: Vec<Option<String>> = a
        .param("via")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| Some(v.to_string()))
        .collect();
    if named.is_empty() { vec![None] } else { named }
}

/// A UDP socket to reach `to`, over `via` or the system's choice.
async fn socket(via: Option<&str>, to: SocketAddr) -> std::io::Result<UdpSocket> {
    match via {
        Some(v) => crate::links::socket_via(v, to),
        None => UdpSocket::bind(if to.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }).await,
    }
}

/// Pings a RIST receiver (an echo request with our report, as the stream would send) and
/// waits for its answer: the round trip. With a passphrase, an answer that won't open says the
/// passphrase is wrong.
pub(crate) async fn reach(d: &Direct) -> Result<Duration, DirectError> {
    let a = Address::parse(&d.url)?;
    check(d, &a)?;
    let name = format!("{}:{}", a.host, a.port);
    let to = tokio::net::lookup_host((a.host.as_str(), a.port))
        .await
        .ok()
        .and_then(|mut it| it.next())
        .ok_or_else(|| DirectError::Unreachable(name.clone()))?;
    let main = Main::of(d, &a);
    // Reports go to the port after the stream's in the simple profile.
    let to = if main.is_some() { to } else { SocketAddr::new(to.ip(), to.port() + 1) };
    let any = if to.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
    let sock = UdpSocket::bind(any).await.map_err(|_| DirectError::Unreachable(name.clone()))?;
    let here = sock.local_addr().map(|l| l.port()).unwrap_or(0);
    let ssrc = fastrand::u32(..) & !1;
    let started = Instant::now();
    // A login takes a few round trips more.
    let wait = if main.as_ref().is_some_and(|m| m.login.is_some()) { 6 } else { 3 };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    let mut again = tokio::time::interval(Duration::from_millis(500));
    let (mut buf, mut unread) = (vec![0u8; 2048], 0u32);
    loop {
        tokio::select! {
            _ = again.tick() => {
                let mut r = report(ssrc, started);
                r.extend_from_slice(&echo_request(ssrc));
                let r = match &main {
                    Some(m) => m.wrap(here, to.port(), &r),
                    None => r,
                };
                let _ = sock.send_to(&r, to).await;
            }
            r = sock.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                if from.ip() != to.ip() {
                    continue;
                }
                // The login first, as the stream does (librist answers nothing else before it).
                if let Some(m) = main.as_ref().filter(|m| m.login.is_some())
                    && let Some(eapol) = Main::login_packet(&buf[..n])
                {
                    for r in m.on_login(&eapol)? {
                        let _ = sock.send_to(&r, to).await;
                    }
                    continue;
                }
                let got = match &main {
                    Some(m) => m.unwrap(&buf[..n]),
                    None => Some(buf[..n].to_vec()),
                };
                match got.as_deref().and_then(echo_round_trip) {
                    Some(rtt) => return Ok(rtt),
                    None => unread += 1,
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                let encrypted = main.as_ref().is_some_and(|m| m.crypt.read().unwrap_or_else(|e| e.into_inner()).is_some());
                return Err(if unread > 0 && encrypted {
                    DirectError::Refused(format!("{name} answered, but its answers don't open: check the passphrase."))
                } else {
                    DirectError::Unreachable(format!("{name} (no answer: is the receiver listening, on this profile?)"))
                });
            }
        }
    }
}

/// The login: `username=` in the address, its password the key (kept in the keychain).
fn login_of<'a>(d: &'a Direct, a: &'a Address) -> Option<(&'a str, &'a str)> {
    a.param("username").filter(|u| !u.is_empty()).map(|u| (u, d.key.trim()))
}

/// The passphrase: the key, or `secret=` in the address (with a login only `secret=`: the key
/// is its password).
fn passphrase<'a>(d: &'a Direct, a: &'a Address) -> Option<&'a str> {
    if login_of(d, a).is_some() {
        return a.param("secret").filter(|s| !s.is_empty());
    }
    Some(d.key.trim()).filter(|k| !k.is_empty()).or_else(|| a.param("secret").filter(|s| !s.is_empty()))
}

/// Whether the address asks for the main profile (or a passphrase or login needs it).
fn wants_main(d: &Direct, a: &Address) -> bool {
    passphrase(d, a).is_some()
        || login_of(d, a).is_some()
        || a.param("profile").is_some_and(|p| p == "1" || p.eq_ignore_ascii_case("main"))
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
    if let Some((_, password)) = login_of(d, a) {
        if password.is_empty() {
            return Err(DirectError::Address("A RIST username needs its password: put it in the key.".into()));
        }
        if a.param("profile").is_some_and(|p| p == "0" || p.eq_ignore_ascii_case("simple")) {
            return Err(DirectError::Address("A RIST login needs the main profile: leave profile=0 out.".into()));
        }
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

/// GRE's protocol for EAPOL (the login).
const GRE_EAPOL: u16 = 0x888E;

/// The main profile's GRE wrapping, and with a passphrase (or a login's key) its encryption.
struct Main {
    crypt: std::sync::RwLock<Option<Crypt>>,
    /// A username and password: no stream goes out until logged in, then the login's key
    /// encrypts it (a new one each time the receiver logs us in again).
    login: Option<std::sync::Mutex<crate::rist_eap::Login>>,
    /// With a login and no secret, librist encrypts its answers under the session key: ours to
    /// read them (the stream itself goes in the clear).
    answers: std::sync::RwLock<Option<Crypt>>,
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

impl Crypt {
    /// `pass` with a fresh random nonce (top bit clear: the "even" key).
    fn new(pass: &[u8], size: usize) -> Self {
        let mut nonce = [0u8; 4];
        while nonce == [0; 4] {
            nonce = fastrand::u32(..).to_be_bytes();
            nonce[0] &= 0x7F;
        }
        let key = aes_key(pass, &nonce, size);
        Crypt { pass: pass.to_vec(), size, tx: (nonce, key), rx: std::sync::Mutex::new(None) }
    }
}

impl Main {
    fn of(d: &Direct, a: &Address) -> Option<Self> {
        if !wants_main(d, a) {
            return None;
        }
        // 256 bits unless told otherwise, as librist.
        let size = if a.param("aes-type") == Some("128") { 16 } else { 32 };
        let crypt = passphrase(d, a).map(|pass| Crypt::new(pass.as_bytes(), size));
        let secret = passphrase(d, a).map(str::as_bytes);
        let login = login_of(d, a).map(|(u, p)| std::sync::Mutex::new(crate::rist_eap::Login::new(u, p, secret)));
        Some(Self {
            crypt: std::sync::RwLock::new(crypt),
            login,
            answers: std::sync::RwLock::new(None),
            seq: AtomicU32::new(fastrand::u32(..)),
        })
    }

    /// The stream may go out: no login, or logged in.
    fn ready(&self) -> bool {
        self.login.is_none() || self.logged_in()
    }

    /// A login packet (EAPOL) as GRE: never encrypted, as librist sends it.
    fn wrap_login(&self, eapol: &[u8]) -> Vec<u8> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut p = vec![0x10, 1 << 3];
        p.extend_from_slice(&GRE_EAPOL.to_be_bytes());
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(eapol);
        p
    }

    /// What a GRE packet carries when it's the login (EAPOL, in the clear).
    fn login_packet(p: &[u8]) -> Option<Vec<u8>> {
        let (flags1, flags2) = (*p.first()?, *p.get(1)?);
        if u16::from_be_bytes([*p.get(2)?, *p.get(3)?]) != GRE_EAPOL || flags1 & 0x60 != 0 || flags2 & 0x07 != 0 {
            return None;
        }
        let at = 4 + if flags1 & 0x80 != 0 { 4 } else { 0 } + if flags1 & 0x10 != 0 { 4 } else { 0 };
        p.get(at..).map(<[u8]>::to_vec)
    }

    /// A login packet from the receiver: what to send back (as GRE), a new key taken; Err when
    /// it refused us.
    fn on_login(&self, eapol: &[u8]) -> Result<Vec<Vec<u8>>, DirectError> {
        let Some(login) = &self.login else { return Ok(Vec::new()) };
        let (step, key) = {
            let mut l = login.lock().unwrap_or_else(|e| e.into_inner());
            (l.handle(eapol).map_err(DirectError::Refused)?, l.key())
        };
        // No secret: the receiver's answers come under the session key (256 bits).
        if let Some(k) = key
            && self.crypt.read().unwrap_or_else(|e| e.into_inner()).is_none()
        {
            let mut answers = self.answers.write().unwrap_or_else(|e| e.into_inner());
            if answers.as_ref().is_none_or(|c| c.pass != k) {
                *answers = Some(Crypt::new(&k, 32));
            }
        }
        Ok(step.replies.iter().map(|r| self.wrap_login(r)).collect())
    }

    /// Logged in (the receiver proved itself).
    fn logged_in(&self) -> bool {
        self.login.as_ref().is_some_and(|l| l.lock().unwrap_or_else(|e| e.into_inner()).authenticated)
    }

    /// `inner` (RTP or RTCP) as a GRE packet (version 1, reduced header with the virtual ports).
    fn wrap(&self, src_port: u16, dst_port: u16, inner: &[u8]) -> Vec<u8> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut p = Vec::with_capacity(16 + inner.len());
        // Flags: sequence number (bit 4), key (bit 5); version 1, and H for a 256-bit key.
        let crypt = self.crypt.read().unwrap_or_else(|e| e.into_inner());
        let (flags1, flags2) = match crypt.as_ref() {
            Some(c) => (0x30, 1 << 3 | if c.size == 32 { 0x40 } else { 0 }),
            None => (0x10, 1 << 3),
        };
        p.extend_from_slice(&[flags1, flags2]);
        p.extend_from_slice(&GRE_REDUCED.to_be_bytes());
        if let Some(c) = crypt.as_ref() {
            p.extend_from_slice(&c.tx.0);
        }
        p.extend_from_slice(&seq.to_be_bytes());
        let body = p.len();
        p.extend_from_slice(&src_port.to_be_bytes());
        p.extend_from_slice(&dst_port.to_be_bytes());
        p.extend_from_slice(inner);
        if let Some(c) = crypt.as_ref() {
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
        let (crypt, answers) = (
            self.crypt.read().unwrap_or_else(|e| e.into_inner()),
            self.answers.read().unwrap_or_else(|e| e.into_inner()),
        );
        match (crypt.as_ref().or(answers.as_ref()), nonce, seq) {
            (Some(c), Some(nonce), Some(seq)) if nonce != [0; 4] => {
                // The receiver's own key size: H set is 256 bits (librist's default), else ours.
                let size = if flags2 & 0x40 != 0 { 32 } else { c.size };
                let mut rx = c.rx.lock().unwrap_or_else(|e| e.into_inner());
                if rx.as_ref().is_none_or(|(n, key, _)| *n != nonce || key.len() != size) {
                    // A new key at most every 10 ms, as librist (PBKDF2 isn't free).
                    if rx.as_ref().is_some_and(|(_, _, at)| at.elapsed() < REKEY_EVERY) {
                        return None;
                    }
                    *rx = Some((nonce, aes_key(&c.pass, &nonce, size), Instant::now()));
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
        self.shared.sent.fetch_add(1, Ordering::Relaxed);
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
/// The time now in NTP's form: seconds since 1900, and the fraction of a second (of 2^32).
fn ntp_now() -> (u64, u32) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (now.as_secs() + 2_208_988_800, ((u64::from(now.subsec_nanos()) << 32) / 1_000_000_000) as u32)
}

/// NTP time as one number (seconds in the top half, the fraction in the bottom).
fn ntp64() -> u64 {
    let (secs, frac) = ntp_now();
    (secs << 32) | u64::from(frac)
}

/// RIST's echo request (APP 204 "RIST", subtype 2): our time now, which the receiver sends
/// straight back for the round trip.
fn echo_request(ssrc: u32) -> Vec<u8> {
    let mut p = vec![0x82, 204, 0, 5];
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(b"RIST");
    p.extend_from_slice(&ntp64().to_be_bytes());
    // The receiver's wait before answering (ours to fill only in a response).
    p.extend_from_slice(&[0; 4]);
    p
}

/// The round trip an echo response (subtype 3) in a compound RTCP packet shows.
pub(crate) fn echo_round_trip(buf: &[u8]) -> Option<Duration> {
    let mut at = 0;
    while at + 4 <= buf.len() {
        let (b0, pt) = (buf[at], buf[at + 1]);
        let words = usize::from(u16::from_be_bytes([buf[at + 2], buf[at + 3]]));
        if pt == 204 && b0 & 0x1F == 3 && buf.get(at + 8..at + 12) == Some(b"RIST") {
            let sent = u64::from_be_bytes(buf.get(at + 12..at + 20)?.try_into().ok()?);
            let delay = u64::from(u32::from_be_bytes(buf.get(at + 20..at + 24)?.try_into().ok()?));
            // NTP fractions (of 2^32 s) to microseconds; the delay is in microseconds.
            let rtt = ntp64().checked_sub(sent)?.saturating_mul(1_000_000) >> 32;
            let rtt = rtt.saturating_sub(delay);
            // More than a minute: a clock or answer that doesn't add up.
            return (rtt < 60_000_000).then(|| Duration::from_micros(rtt));
        }
        if words == 0 {
            break;
        }
        at += 4 + words * 4;
    }
    None
}

fn report(ssrc: u32, started: Instant) -> Vec<u8> {
    let (secs, frac) = ntp_now();
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
/// How long the login may take before the session gives up.
const LOGIN_DEADLINE: Duration = Duration::from_secs(10);

async fn answer(
    shared: Arc<Shared>,
    rtcp: Vec<Arc<UdpSocket>>,
    rtcp_to: SocketAddr,
    health: &watch::Sender<Health>,
) -> DirectError {
    let started = Instant::now();
    // Every connection's answers into one queue (the readers stop when this does).
    let (tx, mut answers) = tokio::sync::mpsc::channel::<(Vec<u8>, SocketAddr)>(256);
    let mut readers = tokio::task::JoinSet::new();
    for s in &rtcp {
        let (s, tx) = (s.clone(), tx.clone());
        readers.spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                match s.recv_from(&mut buf).await {
                    Ok((n, from)) => {
                        if tx.send((buf[..n].to_vec(), from)).await.is_err() {
                            break;
                        }
                    }
                    // A connection gone for now: don't spin on it.
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
        });
    }
    let mut tick = tokio::time::interval(REPORT_EVERY);
    // The link's health: the last round trip, and the counts a second ago.
    let mut rtt = None::<Duration>;
    let mut every_second = tokio::time::interval(Duration::from_secs(1));
    let mut counted = (0u64, 0u64, 0u64);
    loop {
        tokio::select! {
            _ = every_second.tick() => {
                // Logging in: hello again each second until the receiver answers, for so long.
                if let Some(m) = shared.main.as_ref().filter(|m| m.login.is_some() && !m.logged_in()) {
                    if started.elapsed() > LOGIN_DEADLINE {
                        return DirectError::Refused(format!(
                            "{rtcp_to} didn't answer the login: is it set for a username (the main profile)?"
                        ));
                    }
                    let hello = m.wrap_login(&crate::rist_eap::Login::start());
                    for s in &rtcp {
                        let _ = s.send_to(&hello, rtcp_to).await;
                    }
                }
                let load = |n: &AtomicU64| n.load(Ordering::Relaxed);
                let now = (load(&shared.sent), load(&shared.asked), load(&shared.resent));
                let sent = now.0.saturating_sub(counted.0).max(1) as f32;
                // Lost: what the receiver asked for again (its reports don't count loss).
                let link = Link {
                    rtt_ms: rtt.map_or(0, |d| d.as_millis() as u32),
                    lost_pct: now.1.saturating_sub(counted.1) as f32 * 100.0 / sent,
                    resent_pct: now.2.saturating_sub(counted.2) as f32 * 100.0 / sent,
                };
                counted = now;
                if rtt.is_some() || link.resent_pct > 0.0 {
                    health.send_modify(|h| h.link = Some(link));
                }
            }
            _ = tick.tick() => {
                // Before logging in too: a report is what makes librist meet us and start the
                // login (the stream itself waits for it).
                let mut r = report(shared.ssrc, started);
                r.extend_from_slice(&echo_request(shared.ssrc));
                // Over every connection, so the receiver knows each way back.
                for s in &rtcp {
                    let wire = match &shared.main {
                        // The main profile's reports go in GRE, from and to the real ports.
                        Some(m) => m.wrap(s.local_addr().map(|a| a.port()).unwrap_or(0), rtcp_to.port(), &r),
                        None => r.clone(),
                    };
                    let _ = s.send_to(&wire, rtcp_to).await;
                }
            }
            Some((buf, from)) = answers.recv() => {
                // Only the receiver's answers (a stranger's packets could make us work for nothing).
                if from.ip() != rtcp_to.ip() {
                    continue;
                }
                // The login (again every minute, for a new key): answered over every connection.
                if let Some(m) = shared.main.as_ref().filter(|m| m.login.is_some())
                    && let Some(eapol) = Main::login_packet(&buf)
                {
                    match m.on_login(&eapol) {
                        Ok(replies) => {
                            for r in &replies {
                                for s in &rtcp {
                                    let _ = s.send_to(r, rtcp_to).await;
                                }
                            }
                        }
                        Err(e) => return e,
                    }
                    continue;
                }
                let got = match &shared.main {
                    Some(m) => match m.unwrap(&buf) {
                        Some(inner) => inner,
                        None => continue,
                    },
                    None => buf,
                };
                rtt = echo_round_trip(&got).or(rtt);
                let want = lost(&got);
                shared.asked.fetch_add(want.len() as u64, Ordering::Relaxed);
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
                    shared.resent.fetch_add(1, Ordering::Relaxed);
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
    fn an_echo_answer_gives_the_round_trip() {
        // Our request, answered as librist does (subtype 3, our time back, no wait), after a
        // receiver report in the same compound packet.
        let mut req = echo_request(7);
        req[0] = 0x83;
        let mut both = vec![0x80, 201, 0, 1, 0, 0, 0, 9];
        both.extend_from_slice(&req);
        std::thread::sleep(Duration::from_millis(20));
        let rtt = echo_round_trip(&both).unwrap_or_default();
        assert!(rtt >= Duration::from_millis(20) && rtt < Duration::from_secs(2), "{rtt:?}");
        // A request isn't an answer.
        assert_eq!(echo_round_trip(&echo_request(7)), None);
    }

    fn direct(url: &str, key: &str) -> Direct {
        Direct {
            id: "r".into(),
            kind: Kind::Rist,
            url: url.into(),
            key: key.into(),
            retry: Retry::default(),
            layer: None,
        }
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
        let (health, h) = watch::channel(Health {
            state: PushState::Waiting,
            kbps: 0,
            dropped: 0,
            sent: 0,
            link: None,
            via: None,
            since_ms: 0,
        });
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
        let (health, h) = watch::channel(Health {
            state: PushState::Waiting,
            kbps: 0,
            dropped: 0,
            sent: 0,
            link: None,
            via: None,
            since_ms: 0,
        });
        let (stop_tx, stop) = watch::channel(false);
        let d = Direct {
            id: "r".into(),
            kind: Kind::Rist,
            url: format!("rist://127.0.0.1:{port}"),
            key: String::new(),
            retry: Retry::default(),
            layer: None,
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
