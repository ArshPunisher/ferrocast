//! SRTLA: one SRT stream over several connections at once (Wi-Fi, a phone's hotspot, Ethernet),
//! as BELABOX's srtla does it, so IRL servers that take SRTLA take ours. Written from the
//! protocol, not their code.
//!
//! The sender sits between our SRT socket and the network: SRT talks to it on this computer,
//! and each packet goes out over the connection with the most room (its window over what it
//! has in flight). The receiver acknowledges what each connection brought, a NAK shrinks the
//! window of the one that lost it, so a weak connection gets less. The receiver puts it all
//! back together into one SRT stream to the real server.
//!
//! Registration: the sender makes a 256-byte group id and sends it (REG2) on every connection;
//! the receiver doesn't know it (NGP), so one connection asks for a group (REG1), the
//! receiver answers with the id, its second half its own (REG2), and every connection joins
//! with it (REG2 → REG3). Keep-alives hold idle connections open; one silent for 4 s is
//! retried.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::links;

const MTU: usize = 1500;
const ID_LEN: usize = 256;

const SRT_ACK: u16 = 0x8002;
const SRT_NAK: u16 = 0x8003;
const KEEPALIVE: u16 = 0x9000;
const ACK: u16 = 0x9100;
const REG1: u16 = 0x9200;
const REG2: u16 = 0x9201;
const REG3: u16 = 0x9202;
const REG_ERR: u16 = 0x9210;
const REG_NGP: u16 = 0x9211;
/// SRT's smallest packet (its header).
const SRT_MIN: usize = 16;

/// A connection silent this long is retried (sender).
const LINK_TIMEOUT: Duration = Duration::from_secs(4);
/// A REG1 or REG2 unanswered this long is given up.
const REG_TIMEOUT: Duration = Duration::from_secs(4);
/// A keep-alive goes on a connection that sent nothing this long.
const IDLE: Duration = Duration::from_secs(1);
const HOUSEKEEPING: Duration = Duration::from_millis(250);

/// The window, in thousandths of a packet: starts at 20, from 1 to 60.
const WINDOW_MULT: i32 = 1000;
const WINDOW_MIN: i32 = WINDOW_MULT;
const WINDOW_DEF: i32 = 20 * WINDOW_MULT;
const WINDOW_MAX: i32 = 60 * WINDOW_MULT;
const WINDOW_DECR: i32 = 100;
const WINDOW_INCR: i32 = 30;
/// Sequence numbers remembered per connection (to know which one a NAK or ACK is about).
const LOG: usize = 256;
/// A NAK's range is looked at up to this many packets (a forged huge range costs nothing).
const MAX_NAK_RANGE: u32 = 1024;

/// The receiver acknowledges every this many packets per connection.
const ACK_EVERY: usize = 10;
/// Receiver limits.
const MAX_CONNS: usize = 8;
const MAX_GROUPS: usize = 200;
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

fn kind(p: &[u8]) -> u16 {
    if p.len() < 2 { 0 } else { u16::from_be_bytes([p[0], p[1]]) }
}

/// An SRT data packet's sequence number (control packets have none).
fn data_seq(p: &[u8]) -> Option<i32> {
    let sn = u32::from_be_bytes(p.get(..4)?.try_into().ok()?);
    (sn & 0x8000_0000 == 0).then_some(sn as i32)
}

fn word(p: &[u8], i: usize) -> Option<u32> {
    p.get(i * 4..i * 4 + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn packet(k: u16, id: Option<&[u8; ID_LEN]>) -> Vec<u8> {
    let mut v = k.to_be_bytes().to_vec();
    if let Some(id) = id {
        v.extend_from_slice(id);
    }
    v
}

// ---------------------------------------------------------------- sender

/// One connection of the bond.
#[derive(Debug)]
struct Link {
    name: String,
    sock: Arc<UdpSocket>,
    last_rcvd: Option<Instant>,
    last_sent: Instant,
    window: i32,
    in_flight: i32,
    log: [i32; LOG],
    at: usize,
    sent: u64,
}

impl Link {
    fn new(name: String, sock: Arc<UdpSocket>) -> Self {
        Link {
            name,
            sock,
            last_rcvd: None,
            last_sent: Instant::now(),
            window: WINDOW_DEF,
            in_flight: 0,
            log: [-1; LOG],
            at: 0,
            sent: 0,
        }
    }

    fn up(&self, now: Instant) -> bool {
        self.last_rcvd.is_some_and(|t| now.duration_since(t) < LINK_TIMEOUT)
    }

    fn reset(&mut self) {
        self.last_rcvd = None;
        self.window = WINDOW_MIN;
        self.in_flight = 0;
        self.log = [-1; LOG];
    }

    fn sent_seq(&mut self, sn: i32) {
        self.log[self.at] = sn;
        self.at = (self.at + 1) % LOG;
        self.in_flight += 1;
    }

    /// Where `sn` is in the log, newest first.
    fn find(&self, sn: i32) -> Option<usize> {
        (1..=LOG).map(|back| (self.at + LOG - back) % LOG).find(|&i| self.log[i] == sn)
    }
}

/// How each connection of a bond is doing.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LinkStats {
    pub name: String,
    pub up: bool,
    /// Packets sent over it.
    pub sent: u64,
    /// Its share of the window (how much it's trusted), packets.
    pub window: u32,
}

/// The sending state: the links and the registration.
struct Sender {
    links: Vec<Link>,
    id: [u8; ID_LEN],
    pending: Option<(usize, Instant)>,
    quiet_until: Instant,
}

impl Sender {
    /// The connection with the most room: its window over what it has in flight.
    fn pick(&mut self, now: Instant) -> Option<usize> {
        let mut best: Option<(usize, i32)> = None;
        for (i, l) in self.links.iter().enumerate() {
            if !l.up(now) {
                continue;
            }
            let score = l.window / (l.in_flight + 1);
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((i, score));
            }
        }
        best.map(|(i, _)| i)
    }

    fn nak(&mut self, sn: i32) {
        for l in &mut self.links {
            if let Some(i) = l.find(sn) {
                l.log[i] = -1;
                l.window = (l.window - WINDOW_DECR).max(WINDOW_MIN);
                return;
            }
        }
    }

    fn srtla_ack(&mut self, sn: i32) {
        let mut found = false;
        for l in &mut self.links {
            if !found && let Some(i) = l.find(sn) {
                found = true;
                l.in_flight = (l.in_flight - 1).max(0);
                l.log[i] = -1;
                if l.in_flight * WINDOW_MULT > l.window {
                    l.window += WINDOW_INCR - 1;
                }
            }
            if l.last_rcvd.is_some() {
                l.window = (l.window + 1).min(WINDOW_MAX);
            }
        }
    }

    /// SRT's ACK: everything before `ack` arrived.
    fn srt_ack(&mut self, ack: i32) {
        for l in &mut self.links {
            let mut n = 0;
            for v in &mut l.log {
                if *v >= 0 && *v < ack {
                    *v = -1;
                } else if *v >= 0 {
                    n += 1;
                }
            }
            l.in_flight = n;
        }
    }

    /// A packet from the receiver on link `i` → what goes on to our SRT (None: SRTLA's own).
    fn heard<'a>(&mut self, i: usize, p: &'a [u8], now: Instant) -> Option<&'a [u8]> {
        match kind(p) {
            REG_NGP => {
                // It doesn't know the group: one connection asks for one (when none works yet).
                let active = self.links.iter().any(|l| l.up(now));
                if !active && self.pending.is_none() && now >= self.quiet_until {
                    self.pending = Some((i, now + REG_TIMEOUT));
                    let _ = self.links[i].sock.try_send(&packet(REG1, Some(&self.id)));
                }
                return None;
            }
            REG2 if p.len() == 2 + ID_LEN => {
                if self.pending.is_some_and(|(at, _)| at == i) && p[2..2 + ID_LEN / 2] == self.id[..ID_LEN / 2] {
                    self.id.copy_from_slice(&p[2..]);
                    for l in &self.links {
                        let _ = l.sock.try_send(&packet(REG2, Some(&self.id)));
                    }
                    self.pending = None;
                    self.quiet_until = now + REG_TIMEOUT;
                }
                return None;
            }
            _ => {}
        }
        self.links[i].last_rcvd = Some(now);
        match kind(p) {
            SRT_ACK => {
                if let Some(ack) = word(p, 4) {
                    self.srt_ack(ack as i32);
                }
            }
            SRT_NAK => {
                let mut w = 4;
                while let Some(v) = word(p, w) {
                    if v & 0x8000_0000 != 0 {
                        let from = v & 0x7FFF_FFFF;
                        let to = word(p, w + 1).unwrap_or(from).min(from.saturating_add(MAX_NAK_RANGE));
                        for sn in from..=to {
                            self.nak(sn as i32);
                        }
                        w += 2;
                    } else {
                        self.nak(v as i32);
                        w += 1;
                    }
                }
            }
            ACK => {
                let mut w = 1;
                while let Some(v) = word(p, w) {
                    self.srtla_ack(v as i32);
                    w += 1;
                }
                return None;
            }
            KEEPALIVE | REG3 | REG_ERR => return None,
            _ => {}
        }
        Some(p)
    }

    /// Every quarter second: dead connections retried, idle ones kept alive.
    fn housekeeping(&mut self, now: Instant) {
        if self.pending.is_some_and(|(_, until)| now > until) {
            self.pending = None;
        }
        for i in 0..self.links.len() {
            let l = &mut self.links[i];
            if !l.up(now) {
                if l.last_rcvd.is_some() {
                    tracing::info!(link = %l.name, "SRTLA: a connection went quiet, trying it again");
                    l.reset();
                }
                // Retried no more than once a second.
                if now.duration_since(l.last_sent) < IDLE {
                    continue;
                }
                l.last_sent = now;
                let p = match self.pending {
                    None => packet(REG2, Some(&self.id)),
                    Some((at, _)) if at == i => packet(REG1, Some(&self.id)),
                    Some(_) => continue,
                };
                let _ = l.sock.try_send(&p);
            } else if now.duration_since(l.last_sent) >= IDLE {
                l.last_sent = now;
                let _ = l.sock.try_send(&packet(KEEPALIVE, None));
            }
        }
    }
}

/// A bond running: our SRT connects to `local`; dropping it stops.
pub struct Bond {
    pub local: SocketAddr,
    task: JoinHandle<()>,
    stats: Arc<Mutex<Vec<LinkStats>>>,
}

impl std::fmt::Debug for Bond {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bond").field("local", &self.local).finish_non_exhaustive()
    }
}

impl Drop for Bond {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Bond {
    /// How each connection is doing now.
    pub fn stats(&self) -> Vec<LinkStats> {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// Starts a bond to the SRTLA receiver `server`, over the connections `via` (names or
/// addresses; none: every real connection here, or the default way when there's one).
pub async fn bond(server: SocketAddr, via: &[String]) -> io::Result<Bond> {
    let names: Vec<String> =
        if via.is_empty() { links::fallbacks().into_iter().map(|n| n.name).collect() } else { via.to_vec() };
    let mut list = Vec::new();
    for n in &names {
        match links::socket_via(n, server) {
            Ok(s) => {
                s.connect(server).await?;
                list.push(Link::new(n.clone(), Arc::new(s)));
            }
            Err(e) => tracing::info!(link = %n, error = %e, "SRTLA: a connection can't be used"),
        }
    }
    if list.is_empty() {
        // Nothing named works: the system's own way, so the stream still goes.
        let s = UdpSocket::bind(if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).await?;
        s.connect(server).await?;
        list.push(Link::new("default".into(), Arc::new(s)));
    }
    let local = UdpSocket::bind("127.0.0.1:0").await?;
    let local_addr = local.local_addr()?;
    let mut id = [0u8; ID_LEN];
    getrandom::fill(&mut id).map_err(|e| io::Error::other(e.to_string()))?;
    let stats = Arc::new(Mutex::new(Vec::new()));
    let task = tokio::spawn(run_sender(
        local,
        Sender { links: list, id, pending: None, quiet_until: Instant::now() },
        stats.clone(),
    ));
    Ok(Bond { local: local_addr, task, stats })
}

async fn run_sender(local: UdpSocket, mut s: Sender, stats: Arc<Mutex<Vec<LinkStats>>>) {
    // Each connection's packets come to one place.
    let (tx, mut rx) = mpsc::channel::<(usize, Vec<u8>)>(1024);
    let mut readers = Vec::new();
    for (i, l) in s.links.iter().enumerate() {
        let (sock, tx) = (l.sock.clone(), tx.clone());
        readers.push(tokio::spawn(async move {
            let mut buf = vec![0u8; MTU];
            loop {
                match sock.recv(&mut buf).await {
                    Ok(n) => {
                        if tx.send((i, buf[..n].to_vec())).await.is_err() {
                            return;
                        }
                    }
                    // A connection that dropped says so on each read (ICMP): don't spin.
                    Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
                }
            }
        }));
    }
    drop(tx);
    struct Abort(Vec<JoinHandle<()>>);
    impl Drop for Abort {
        fn drop(&mut self) {
            for r in &self.0 {
                r.abort();
            }
        }
    }
    let _readers = Abort(readers);
    let mut srt: Option<SocketAddr> = None;
    let mut buf = vec![0u8; MTU];
    let mut tick = tokio::time::interval(HOUSEKEEPING);
    s.housekeeping(Instant::now());
    loop {
        tokio::select! {
            r = local.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                // Only the SRT that started talking here: another program on this computer
                // can't take the stream's answers.
                if srt.is_some_and(|s| s != from) {
                    continue;
                }
                srt = Some(from);
                let now = Instant::now();
                let Some(i) = s.pick(now) else { continue };
                let l = &mut s.links[i];
                match l.sock.try_send(&buf[..n]) {
                    Ok(_) => {
                        l.last_sent = now;
                        l.sent += 1;
                        if let Some(sn) = data_seq(&buf[..n]) {
                            l.sent_seq(sn);
                        }
                    }
                    // It can't send: off until the receiver is heard on it again.
                    Err(_) => l.last_rcvd = None,
                }
            }
            got = rx.recv() => {
                let Some((i, p)) = got else { return };
                if let (Some(to), Some(fwd)) = (srt, s.heard(i, &p, Instant::now())) {
                    let _ = local.send_to(fwd, to).await;
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                s.housekeeping(now);
                if let Ok(mut st) = stats.lock() {
                    *st = s.links.iter().map(|l| LinkStats {
                        name: l.name.clone(),
                        up: l.up(now),
                        sent: l.sent,
                        window: (l.window / WINDOW_MULT) as u32,
                    }).collect();
                }
            }
        }
    }
}

// ---------------------------------------------------------------- receiver

struct Conn {
    addr: SocketAddr,
    last: Instant,
    got: Vec<u32>,
}

struct Group {
    id: [u8; ID_LEN],
    conns: Vec<Conn>,
    created: Instant,
    last_addr: SocketAddr,
    /// To the SRT server, opened on the first packet; its reader sends answers back.
    srt: Option<(Arc<UdpSocket>, JoinHandle<()>)>,
}

impl Drop for Group {
    fn drop(&mut self) {
        if let Some((_, t)) = &self.srt {
            t.abort();
        }
    }
}

type Groups = Arc<Mutex<Vec<Group>>>;

/// An SRTLA receiver on `sock`: each sender's connections put back into one SRT stream to
/// `srt` (the SRT server, a caller as far as it knows). Runs until dropped (abort the task).
pub async fn receive(sock: UdpSocket, srt: SocketAddr) -> io::Result<()> {
    let sock = Arc::new(sock);
    let groups: Groups = Arc::new(Mutex::new(Vec::new()));
    let mut buf = vec![0u8; MTU];
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                let fwd = handle(&sock, &groups, &buf[..n], from, srt);
                if let Some((to, p)) = fwd {
                    let _ = to.send(&p).await;
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                if let Ok(mut g) = groups.lock() {
                    for gr in g.iter_mut() {
                        gr.conns.retain(|c| now.duration_since(c.last) < RECV_TIMEOUT);
                    }
                    g.retain(|gr| !gr.conns.is_empty() || now.duration_since(gr.created) < RECV_TIMEOUT);
                }
            }
        }
    }
}

/// One packet from a sender → what goes to the SRT server (its socket and the bytes).
fn handle(
    sock: &Arc<UdpSocket>,
    groups: &Groups,
    p: &[u8],
    from: SocketAddr,
    srt: SocketAddr,
) -> Option<(Arc<UdpSocket>, Vec<u8>)> {
    let reply = |k: u16, id: Option<&[u8; ID_LEN]>| {
        let _ = sock.try_send_to(&packet(k, id), from);
    };
    let mut gs = groups.lock().ok()?;
    let now = Instant::now();
    let member = |g: &Group| g.conns.iter().any(|c| c.addr == from);
    if kind(p) == REG1 && p.len() == 2 + ID_LEN {
        if gs.len() >= MAX_GROUPS || gs.iter().any(|g| member(g) || g.last_addr == from) {
            reply(REG_ERR, None);
            return None;
        }
        let mut id = [0u8; ID_LEN];
        id[..ID_LEN / 2].copy_from_slice(&p[2..2 + ID_LEN / 2]);
        if getrandom::fill(&mut id[ID_LEN / 2..]).is_err() {
            reply(REG_ERR, None);
            return None;
        }
        gs.push(Group { id, conns: Vec::new(), created: now, last_addr: from, srt: None });
        reply(REG2, Some(&id));
        return None;
    }
    if kind(p) == REG2 && p.len() == 2 + ID_LEN {
        let Some(gi) = gs.iter().position(|g| g.id[..] == p[2..]) else {
            reply(REG_NGP, None);
            return None;
        };
        // In another group already: no.
        if gs.iter().enumerate().any(|(i, g)| i != gi && member(g)) {
            reply(REG_ERR, None);
            return None;
        }
        let g = &mut gs[gi];
        if !member(g) {
            if g.conns.len() >= MAX_CONNS {
                reply(REG_ERR, None);
                return None;
            }
            g.conns.push(Conn { addr: from, last: now, got: Vec::with_capacity(ACK_EVERY) });
            tracing::info!(%from, conns = g.conns.len(), "SRTLA: a connection joined");
        }
        g.last_addr = from;
        reply(REG3, None);
        return None;
    }
    let g = gs.iter_mut().find(|g| member(g))?;
    let c = g.conns.iter_mut().find(|c| c.addr == from)?;
    c.last = now;
    if kind(p) == KEEPALIVE {
        let _ = sock.try_send_to(p, from);
        return None;
    }
    if p.len() < SRT_MIN {
        return None;
    }
    g.last_addr = from;
    if let Some(sn) = data_seq(p) {
        c.got.push(sn as u32);
        if c.got.len() == ACK_EVERY {
            let mut ack = (u32::from(ACK) << 16).to_be_bytes().to_vec();
            for v in c.got.drain(..) {
                ack.extend_from_slice(&v.to_be_bytes());
            }
            let _ = sock.try_send_to(&ack, from);
        }
    }
    if g.srt.is_none() {
        let s = std::net::UdpSocket::bind(if srt.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).ok()?;
        s.connect(srt).ok()?;
        s.set_nonblocking(true).ok()?;
        let s = Arc::new(UdpSocket::from_std(s).ok()?);
        let task = tokio::spawn(answers(s.clone(), sock.clone(), groups.clone(), g.id));
        g.srt = Some((s, task));
    }
    g.srt.as_ref().map(|(s, _)| (s.clone(), p.to_vec()))
}

/// The SRT server's answers to a group: ACKs on every connection (so they arrive soon), the
/// rest on the one heard from last.
async fn answers(srt: Arc<UdpSocket>, sock: Arc<UdpSocket>, groups: Groups, id: [u8; ID_LEN]) {
    let mut buf = vec![0u8; MTU];
    loop {
        let Ok(n) = srt.recv(&mut buf).await else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };
        let to: Vec<SocketAddr> = {
            let Ok(gs) = groups.lock() else { return };
            let Some(g) = gs.iter().find(|g| g.id == id) else { return };
            if kind(&buf[..n]) == SRT_ACK { g.conns.iter().map(|c| c.addr).collect() } else { vec![g.last_addr] }
        };
        for a in to {
            let _ = sock.send_to(&buf[..n], a).await;
        }
    }
}

/// The receiver's map, for tests: how many groups and connections.
#[cfg(test)]
fn count(groups: &Groups) -> (usize, usize) {
    let g = groups.lock().unwrap();
    (g.len(), g.iter().map(|g| g.conns.len()).sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_roomiest_connection_gets_the_next_packet() {
        let now = Instant::now();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let sock = || {
                let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                s.set_nonblocking(true).unwrap();
                Arc::new(UdpSocket::from_std(s).unwrap())
            };
            let mut s = Sender {
                links: vec![Link::new("a".into(), sock()), Link::new("b".into(), sock())],
                id: [7; ID_LEN],
                pending: None,
                quiet_until: now,
            };
            assert_eq!(s.pick(now), None, "nothing heard yet: nowhere to send");
            for l in &mut s.links {
                l.last_rcvd = Some(now);
            }
            // b loses a packet: its window shrinks, a gets more.
            s.links[1].sent_seq(41);
            s.nak(41);
            assert!(s.links[1].window < s.links[0].window);
            assert_eq!(s.pick(now), Some(0));
            // a has 20 in flight: b now has more room.
            for sn in 100..120 {
                s.links[0].sent_seq(sn);
            }
            assert_eq!(s.pick(now), Some(1));
            // SRT's ACK past them clears a's flight.
            s.srt_ack(120);
            assert_eq!(s.links[0].in_flight, 0);
            // A forged NAK range is bounded, and SRTLA's own packets don't reach SRT.
            let mut nak = vec![0x80, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            nak.extend_from_slice(&0x8000_0000u32.to_be_bytes());
            nak.extend_from_slice(&u32::MAX.to_be_bytes());
            assert!(s.heard(0, &nak, now).is_some());
            assert!(s.heard(0, &packet(KEEPALIVE, None), now).is_none());
        });
    }

    /// The whole way over loopback: our SRT → a bond of two connections → our receiver → an
    /// SRT server, and the bytes come out the far end, having used both connections.
    #[tokio::test]
    async fn srt_goes_through_a_bond_of_two_connections() {
        // srt-tokio can't listen on port 0 and say which: a free one first.
        let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let listen = tokio::spawn(async move {
            let mut rx = srt_tokio::SrtSocket::builder().listen_on(port).await.unwrap();
            use futures::StreamExt;
            let mut got = 0usize;
            while let Some(Ok((_, b))) = rx.next().await {
                got += b.len();
                if got >= 200 * 1000 {
                    break;
                }
            }
            got
        });
        let rsock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let raddr = rsock.local_addr().unwrap();
        let groups_probe = tokio::spawn(receive(rsock, format!("127.0.0.1:{port}").parse().unwrap()));
        let b = bond(raddr, &["127.0.0.1".into(), "127.0.0.1".into()]).await.unwrap();
        let mut tx = tokio::time::timeout(
            Duration::from_secs(10),
            srt_tokio::SrtSocket::builder().call(b.local, Some("live/test")),
        )
        .await
        .expect("connects within 10 s")
        .unwrap();
        use futures::SinkExt;
        for _ in 0..200 {
            tx.send((Instant::now(), bytes::Bytes::from(vec![0x47u8; 1000]))).await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let got = tokio::time::timeout(Duration::from_secs(10), listen).await.expect("arrives").unwrap();
        assert!(got >= 200 * 1000, "{got}");
        let st = b.stats();
        assert_eq!(st.len(), 2);
        assert!(st.iter().all(|l| l.up && l.sent > 0), "both connections carried it: {st:?}");
        let _ = tx.close().await;
        groups_probe.abort();
    }

    #[test]
    fn the_receiver_registers_groups_and_refuses_strangers() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let groups: Groups = Arc::new(Mutex::new(Vec::new()));
            let srt: SocketAddr = "127.0.0.1:9".parse().unwrap();
            let a: SocketAddr = "127.0.0.1:5001".parse().unwrap();
            let b: SocketAddr = "127.0.0.1:5002".parse().unwrap();
            // Data from someone not in a group goes nowhere.
            assert!(handle(&sock, &groups, &[0u8; 100], a, srt).is_none());
            let mine = [3u8; ID_LEN];
            handle(&sock, &groups, &packet(REG1, Some(&mine)), a, srt);
            assert_eq!(count(&groups), (1, 0));
            let id = groups.lock().unwrap()[0].id;
            assert_eq!(id[..ID_LEN / 2], mine[..ID_LEN / 2], "the first half is the sender's");
            // Both connections join with the full id; a wrong one doesn't.
            handle(&sock, &groups, &packet(REG2, Some(&id)), a, srt);
            handle(&sock, &groups, &packet(REG2, Some(&id)), b, srt);
            handle(&sock, &groups, &packet(REG2, Some(&[9u8; ID_LEN])), "127.0.0.1:5003".parse().unwrap(), srt);
            assert_eq!(count(&groups), (1, 2));
            // A member's data goes on to SRT.
            assert!(handle(&sock, &groups, &[0u8; 100], b, srt).is_some());
        });
    }
}
