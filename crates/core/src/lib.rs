//! Ferrocast's pure core: events, dedupe, templates, alert rules and queue, commands, filters.
//!
//! Nothing here does I/O, reads a clock or spawns tasks. Callers pass time in as
//! [`event::Millis`], which keeps every rule a plain function that is cheap to test with
//! hostile input and could run in the browser editor later.

mod macros;

pub mod alert;
pub mod dedupe;
pub mod event;
pub mod money;
pub mod queue;
pub mod sample;
pub mod template;
pub mod goal;
pub mod filter;
pub mod command;
pub mod moderation;
pub mod tts;
