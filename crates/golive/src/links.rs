//! The computer's network connections, and UDP sockets held to one of them: RIST sends every
//! packet over each connection named (Wi-Fi and a phone's hotspot, say), and the receiver keeps
//! whichever copy arrives first, so the stream rides out one of them dropping. An RTMP
//! destination that keeps failing tries the next connection instead (Wi-Fi down: the phone's
//! hotspot).

use std::io;
use std::net::{IpAddr, SocketAddr};

use serde::Serialize;

/// One network connection that can reach the internet (its IPv4 address).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Network {
    /// The system's name for it (`en0`, `Wi-Fi`, `eth0`).
    pub name: String,
    pub ip: IpAddr,
}

/// The connections up now, loopback and link-local ones left out.
pub fn networks() -> Vec<Network> {
    let mut out: Vec<Network> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            IpAddr::V4(v4) if !v4.is_link_local() && !v4.is_unspecified() => {
                Some(Network { name: i.name.clone(), ip: IpAddr::V4(v4) })
            }
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

/// Connections a stream can move to, by name: real ones (Ethernet, Wi-Fi, a phone by USB or
/// hotspot), not VPNs, bridges or the system's own.
pub fn fallbacks() -> Vec<Network> {
    const NOT: [&str; 16] = [
        "utun", "awdl", "llw", "bridge", "anpi", "ap", "gif", "stf", "vmnet", "docker", "veth", "tun", "tap", "ipsec",
        "ppp", "lo",
    ];
    networks().into_iter().filter(|n| !NOT.iter().any(|p| n.name.starts_with(p))).collect()
}

/// The address the system sends from by default (to the internet), when it has one. Nothing
/// is sent: a UDP socket only picks its route.
pub fn default_ip() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
}

/// After `cur` (None: the system's default way), the connection to try next: each of `list`
/// that isn't the default one, then the default again.
pub fn next_after(cur: Option<&Network>, list: &[Network], default: Option<IpAddr>) -> Option<Network> {
    let others: Vec<&Network> = list.iter().filter(|n| Some(n.ip) != default).collect();
    let at = cur.and_then(|c| others.iter().position(|n| n.name == c.name));
    match (cur, at) {
        (None, _) => others.first().map(|n| (*n).clone()),
        (Some(_), Some(i)) => others.get(i + 1).map(|n| (*n).clone()),
        // Gone since: start over from the default.
        (Some(_), None) => None,
    }
}

/// What people call a connection (macOS: "Wi-Fi", "iPhone USB", "Thunderbolt Ethernet");
/// its system name where that isn't known.
pub fn label(name: &str) -> String {
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("/usr/sbin/networksetup").arg("-listallhardwareports").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut port = None;
        for line in text.lines() {
            if let Some(p) = line.strip_prefix("Hardware Port: ") {
                port = Some(p.trim().to_string());
            } else if line.strip_prefix("Device: ").map(str::trim) == Some(name)
                && let Some(p) = port.take()
            {
                return p;
            }
        }
    }
    name.to_string()
}

/// A TCP connection to `host`:`port` over `via` only (held to it where the system allows).
pub async fn tcp_via(via: &Network, host: &str, port: u16) -> io::Result<tokio::net::TcpStream> {
    use socket2::{Domain, Protocol, Socket, Type};
    // Its address now, not the one it had when the list was made: a DHCP renewal or a replugged
    // cable gives the same connection a new one, and binding the old one fails on every try.
    let Some(ip) = if_addrs::get_if_addrs()?
        .into_iter()
        .filter(|i| i.name == via.name)
        .map(|i| i.ip())
        .find(|ip| ip.is_ipv4() && !ip.is_unspecified())
    else {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("no connection named {}", via.name)));
    };
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address {} can reach", via.name));
    for to in tokio::net::lookup_host((host, port)).await?.filter(|a| a.is_ipv4()) {
        let sock = Socket::new(Domain::for_address(to), Type::STREAM, Some(Protocol::TCP))?;
        hold_to(&sock, &via.name)?;
        sock.bind(&SocketAddr::new(ip, 0).into())?;
        sock.set_nonblocking(true)?;
        // Each address gets its own short turn: one that blackholes (a route that changed under
        // it) mustn't use up the whole connect deadline before the next is tried.
        let tried =
            tokio::time::timeout(PER_ADDRESS, tokio::net::TcpSocket::from_std_stream(sock.into()).connect(to)).await;
        match tried {
            Ok(Ok(t)) => return Ok(t),
            Ok(Err(e)) => last = e,
            Err(_) => last = io::Error::new(io::ErrorKind::TimedOut, format!("{to} didn't answer")),
        }
    }
    Err(last)
}

/// How long one address of a host gets when connecting over a chosen connection.
const PER_ADDRESS: std::time::Duration = std::time::Duration::from_secs(4);

/// A UDP socket that sends over `via` (a connection's name, or one of its addresses), to reach
/// `to`'s family. Held to that connection where the system allows (macOS, Linux), so packets
/// leave by it even when another is the default route.
pub fn socket_via(via: &str, to: SocketAddr) -> io::Result<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let via = via.trim();
    let (name, ip) = match via.parse::<IpAddr>() {
        Ok(ip) => (None, ip),
        Err(_) => {
            let n = networks()
                .into_iter()
                .find(|n| n.name == via)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no connection named {via}")))?;
            (Some(n.name), n.ip)
        }
    };
    if ip.is_ipv4() != to.is_ipv4() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{via} can't reach {to}")));
    }
    let sock = Socket::new(Domain::for_address(to), Type::DGRAM, Some(Protocol::UDP))?;
    if let Some(name) = &name {
        hold_to(&sock, name)?;
    }
    sock.bind(&SocketAddr::new(ip, 0).into())?;
    sock.set_nonblocking(true)?;
    tokio::net::UdpSocket::from_std(sock.into())
}

/// Sends only by the connection `name` (by name: an address alone may still leave by the default).
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn hold_to(sock: &socket2::Socket, name: &str) -> io::Result<()> {
    let index = if_addrs::get_if_addrs()?
        .into_iter()
        .find(|i| i.name == name)
        .and_then(|i| i.index)
        .and_then(std::num::NonZeroU32::new)
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    sock.bind_device_by_index_v4(Some(index))
}

#[cfg(target_os = "linux")]
fn hold_to(sock: &socket2::Socket, name: &str) -> io::Result<()> {
    // Needs CAP_NET_RAW; without it the address alone picks the way (with source routing).
    let _ = sock.bind_device(Some(name.as_bytes()));
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "linux")))]
fn hold_to(_sock: &socket2::Socket, _name: &str) -> io::Result<()> {
    // Windows sends by the connection that owns the address it's bound to.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(name: &str, ip: &str) -> Network {
        Network { name: name.into(), ip: ip.parse().unwrap() }
    }

    #[test]
    fn a_failing_stream_tries_each_other_connection_then_the_default() {
        let list = [net("en0", "192.168.1.5"), net("en8", "172.20.10.2"), net("en9", "10.0.0.3")];
        let def = Some("192.168.1.5".parse().unwrap());
        // en0 is the default: skipped.
        let a = next_after(None, &list, def);
        assert_eq!(a.as_ref().map(|n| n.name.as_str()), Some("en8"));
        let b = next_after(a.as_ref(), &list, def);
        assert_eq!(b.as_ref().map(|n| n.name.as_str()), Some("en9"));
        assert_eq!(next_after(b.as_ref(), &list, def), None);
        // A connection gone since starts over; only one connection: nothing else to try.
        assert_eq!(next_after(Some(&net("en5", "10.1.1.1")), &list, def), None);
        assert_eq!(next_after(None, &list[..1], def), None);
    }

    #[test]
    fn tcp_goes_over_the_connection_asked() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            let lo = networks_with_loopback().into_iter().find(|n| n.ip.is_loopback()).unwrap();
            let (c, a) = tokio::join!(tcp_via(&lo, "127.0.0.1", port), l.accept());
            assert_eq!(c.unwrap().local_addr().unwrap().ip(), lo.ip);
            assert!(a.is_ok());
            assert!(tcp_via(&net("no-such-network0", "127.0.0.1"), "127.0.0.1", port).await.is_err());
        });
    }

    fn networks_with_loopback() -> Vec<Network> {
        if_addrs::get_if_addrs()
            .unwrap()
            .into_iter()
            .filter(|i| i.ip().is_ipv4())
            .map(|i| Network { name: i.name.clone(), ip: i.ip() })
            .collect()
    }

    #[test]
    fn the_connections_here_are_listed_and_usable() {
        let nets = networks();
        assert!(nets.iter().all(|n| !n.ip.is_loopback()));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            // By address: any of them, to an IPv4 place.
            let to: SocketAddr = "127.0.0.1:9".parse().unwrap();
            assert!(socket_via("127.0.0.1", to).is_ok());
            assert!(socket_via("no-such-network0", to).is_err());
            assert!(socket_via("::1", to).is_err(), "an IPv6 address can't reach an IPv4 place");
            if let Some(n) = nets.first() {
                assert!(socket_via(&n.name, "192.0.2.1:9".parse().unwrap()).is_ok(), "{n:?}");
            }
        });
    }
}

#[cfg(test)]
mod tcp_via_tests {
    use super::*;

    /// A connection that got a new address while the stream ran (a DHCP renewal, Ethernet replugged):
    /// the `Network` kept from before still names it, with the old address. Binding to that address
    /// failed (EADDRNOTAVAIL) on every try until the failover list moved on, using up the tries;
    /// the connection's current address is looked up each time.
    #[tokio::test]
    async fn a_connection_that_changed_address_is_used_by_its_current_one() {
        let Some(lo) =
            if_addrs::get_if_addrs().unwrap_or_default().into_iter().find(|i| i.is_loopback() && i.ip().is_ipv4())
        else {
            return;
        };
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { while l.accept().await.is_ok() {} });
        let stale = Network { name: lo.name.clone(), ip: "10.254.254.254".parse().unwrap() };
        let got = tcp_via(&stale, "127.0.0.1", port).await;
        assert!(got.is_ok(), "the connection's own current address should be bound, not the stale one: {got:?}");
    }
}
