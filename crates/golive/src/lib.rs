//! Go live everywhere (2.0, Track A; docs/plan/LIVE-STUDIO.md). OBS streams once to
//! [`Ingest`] on this PC; the [`Hub`] shares the packets; [`push::run`] sends them on to each
//! platform as they came (no re-encode), each destination on its own.
//!
//! ```text
//! OBS ──▶ Ingest (127.0.0.1) ──▶ Hub ──┬─▶ push: Twitch
//!                                      ├─▶ push: YouTube
//!                                      └─▶ push: Kick, custom servers
//! ```

pub mod flv;
pub mod hub;
pub mod ingest;
pub mod probe;
pub mod protect;
pub mod push;
pub mod target;

mod conn;

pub use hub::{Hub, StreamStats};
pub use ingest::Ingest;
pub use probe::{Need, Service, Verdict};
pub use push::{Destination, Health, PushError, PushState};
pub use target::{Target, TargetError};

#[cfg(test)]
mod tests;
