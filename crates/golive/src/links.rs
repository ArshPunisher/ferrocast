//! The computer's network connections, and UDP sockets held to one of them: RIST sends every
//! packet over each connection named (Wi-Fi and a phone's hotspot, say), and the receiver keeps
//! whichever copy arrives first, so the stream rides out one of them dropping.

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
