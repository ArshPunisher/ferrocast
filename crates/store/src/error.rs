//! Store errors, each with a plain message the API can pass on.

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the database couldn't be opened: {0}")]
    Open(String),
    #[error("the database is newer (version {found}) than this app (version {supported}); update Ferrocast")]
    TooNew { found: i64, supported: i64 },
    #[error("couldn't back up the database before updating it: {0}")]
    Backup(String),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("{0}")]
    Conflict(String),
    #[error("stored data is damaged ({what}): {detail}")]
    Corrupt { what: &'static str, detail: String },
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("couldn't save: {0}")]
    Json(#[from] serde_json::Error),
    #[error("file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Pack(PackError),
}

/// Why a `.ferropack` couldn't be imported; each maps to its own HTTP status.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PackError {
    /// Not a zip, or no manifest (415).
    #[error("That file isn't a .ferropack.")]
    NotAPack,
    /// Truncated or damaged contents (415).
    #[error("The pack is damaged: {0}")]
    Corrupt(String),
    /// Made by a newer app (422).
    #[error("This pack was made by a newer Ferrocast (pack version {0}). Update Ferrocast, then import it.")]
    TooNew(u32),
    /// Over a size limit (413).
    #[error("{0}")]
    TooLarge(String),
    /// A file name that tries to leave the media folder (415).
    #[error("The pack has an unsafe file name: {0}")]
    Unsafe(String),
}

impl From<PackError> for StoreError {
    fn from(e: PackError) -> Self {
        StoreError::Pack(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;
