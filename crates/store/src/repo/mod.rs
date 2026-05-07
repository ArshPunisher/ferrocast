//! Repositories: one module per kind of data. Each takes `&Store` so callers never touch
//! SQL, and every write stamps its own time.

pub mod bot;
pub mod events;
pub mod goals;
pub mod media;
pub mod overlays;
pub mod rules;
pub mod settings;
pub mod viewers;

use rusqlite::types::Type;

/// Reads a JSON column into a typed value, turning bad JSON into a clear "damaged" error.
pub(crate) fn from_json<T: serde::de::DeserializeOwned>(col: usize, raw: String) -> rusqlite::Result<T> {
    serde_json::from_str(&raw).map_err(|e| rusqlite::Error::FromSqlConversionFailure(col, Type::Text, Box::new(e)))
}
