//! "Test connection": tries a destination before going live, as its stream would connect
//! (SRT's handshake, a RIST ping, a WHIP offer, RTMP's handshake), and says how it went.

use std::time::{Duration, Instant};

use rml_rtmp::handshake::PeerType;
use serde::Serialize;

use crate::direct::{Direct, Kind};
use crate::push::Retry;
use crate::target::Target;

/// How a try went, in words for the page.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reach {
    pub ok: bool,
    /// The round trip (or how long the server took to answer), when it answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
    pub message: String,
    /// SRT: the delay this distance asks for, when longer than the address's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub srt_latency_ms: Option<u32>,
}

impl Reach {
    fn ok(rtt: Duration, message: String) -> Self {
        Self { ok: true, rtt_ms: Some(ms(rtt)), message, srt_latency_ms: None }
    }

    fn failed(message: String) -> Self {
        Self { ok: false, rtt_ms: None, message, srt_latency_ms: None }
    }
}

fn ms(d: Duration) -> u32 {
    d.as_millis().min(u128::from(u32::MAX)) as u32
}

/// Tries `url` with `key` (the stream key, passphrase or token).
pub async fn reach(url: &str, key: &str) -> Reach {
    let Some(kind) = Kind::of(url) else { return rtmp(url).await };
    let d = Direct {
        id: "test".into(),
        kind,
        url: url.trim().into(),
        key: key.into(),
        retry: Retry::default(),
        layer: None,
    };
    if let Err(e) = crate::direct::check(&d) {
        return Reach::failed(e.to_string());
    }
    match kind {
        Kind::Srt => match crate::direct::srt_reach(&d).await {
            Ok((rtt, latency)) => {
                let mut r = Reach::ok(rtt, format!("Your SRT server answered. Ping {} ms.", ms(rtt)));
                if let Some(l) = latency {
                    r.srt_latency_ms = Some(ms(l));
                    r.message.push_str(&format!(
                        " This distance needs a delay of {} ms; Ferrocast uses it when the address doesn't set one.",
                        ms(l)
                    ));
                }
                r
            }
            Err(e) => Reach::failed(e.to_string()),
        },
        Kind::Rist => match crate::rist::reach(&d).await {
            Ok(rtt) => Reach::ok(rtt, format!("Your RIST receiver answered. Ping {} ms.", ms(rtt))),
            Err(e) => Reach::failed(e.to_string()),
        },
        Kind::Whip => match crate::whip::reach(&d).await {
            Ok(took) => Reach::ok(
                took,
                format!(
                    "Your WebRTC server took the offer{}.",
                    if key.trim().is_empty() { "" } else { " and the token" }
                ),
            ),
            Err(e) => Reach::failed(e.to_string()),
        },
    }
}

/// RTMP: connects (TLS too) and shakes hands. The key is only checked by streaming.
async fn rtmp(url: &str) -> Reach {
    let t = match Target::parse(url) {
        Ok(t) => t,
        Err(e) => return Reach::failed(e.to_string()),
    };
    let started = Instant::now();
    let mut s = match crate::push::connect(&t).await {
        Ok(s) => s,
        Err(e) => return Reach::failed(e.to_string()),
    };
    let took = started.elapsed();
    match tokio::time::timeout(Duration::from_secs(10), crate::conn::handshake(&mut s, PeerType::Client)).await {
        Ok(Ok(_)) => Reach::ok(took, format!("{} answered. The stream key is checked when you go live.", t.host)),
        Ok(Err(e)) => Reach::failed(format!("{} answered, but {e}.", t.host)),
        Err(_) => Reach::failed(format!("{} took too long to answer.", t.host)),
    }
}
