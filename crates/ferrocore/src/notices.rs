//! One-line things the engine wants said on the dashboard that happen deep in a capture (which
//! has no way to speak): queued here, taken by the engine crate every couple of seconds.

use std::sync::Mutex;

/// A calm line for the dashboard: the key (same key replaces), the line, what's behind its (i).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub key: &'static str,
    pub line: String,
    pub detail: String,
}

static QUEUE: Mutex<Vec<Notice>> = Mutex::new(Vec::new());

/// Queues one (a short queue: older ones go when it's long).
pub fn push(key: &'static str, line: String, detail: String) {
    let mut q = QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    q.retain(|n| !(n.key == key && n.line == line));
    q.push(Notice { key, line, detail });
    let extra = q.len().saturating_sub(8);
    q.drain(..extra);
}

/// Everything queued since the last call.
pub fn take() -> Vec<Notice> {
    std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|p| p.into_inner()))
}
