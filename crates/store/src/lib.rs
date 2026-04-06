//! SQLite storage with numbered migrations (R-34).
//!
//! One writer connection and a few readers, all in WAL mode, so a busy chat log never
//! blocks the dashboard (R-35). Calls are synchronous; the engine runs them on tokio's
//! blocking pool. Before any migration the database is copied to `backups/`.

mod db;
mod error;

pub mod backup;
pub mod repo;

pub use db::Store;
pub use error::StoreError;

/// Current time in Unix milliseconds. The store stamps rows itself so callers can't forget.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// A random URL-safe token: 128 bits from the OS, hex-encoded.
pub fn new_token() -> String {
    let mut bytes = [0u8; 16];
    // getrandom only fails if the OS has no entropy source at all; a panic is right there.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut bytes).expect("the OS random number generator is unavailable");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A new sortable id.
pub fn new_id() -> String {
    ulid::Ulid::generate().to_string().to_lowercase()
}
