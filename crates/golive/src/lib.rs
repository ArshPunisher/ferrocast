//! Go live everywhere (2.0, Track A; docs/plan/LIVE-STUDIO.md). OBS streams once to
//! [`Ingest`] on this PC; the [`Hub`] shares the packets; [`push::run`] sends them on to each
//! platform as they came (no re-encode), each destination on its own.
//!
//! ```text
//! OBS ──▶ Ingest (127.0.0.1) ──▶ Hub ──┬─▶ push: Twitch
//!                                      ├─▶ push: YouTube
//!                                      └─▶ push: Kick, custom servers
//! ```

pub mod bench;
pub mod cut;
pub mod cutter;
pub mod demux;
pub mod direct;
pub mod flv;
pub mod hub;
pub mod ingest;
pub mod links;
pub mod probe;
pub mod protect;
pub mod push;
pub mod quality;
pub mod reach;
mod rist;
mod rist_eap;
mod rist_srp;
pub mod srtla;
pub mod target;
pub mod ts;
pub mod twitch_eb;
mod whip;

mod conn;

pub use flv::VideoCodec;
pub use hub::{Hub, StreamStats};
pub use ingest::Ingest;
pub use probe::{
    BitrateCap, Canvas, CappedInfo, CustomQuality, DestinationQuality, EncodeKey, FittedEncode, MainEncode, Need,
    PlatformLimit, RuleCode, RuleNotice, Service, Verdict, direct_limit, fit, platform_limit,
};
pub use push::{Destination, Health, Link, PushError, PushState, SentStreamInfo};
pub use target::{IpFamily, Target, TargetError};

#[cfg(test)]
mod tests;
