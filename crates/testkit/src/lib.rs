//! Fakes and fixtures for tests. Never used by shipped code, so panicking on a broken
//! fake is the right failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod obs;
pub mod razorpay;
pub mod rtmp;
pub mod streamelements;
pub mod twitch;
