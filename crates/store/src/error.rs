//! Store errors, each with a plain message the API can pass on.

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the database couldn't be opened: {0}")]
    Open(String),
    #[error(
        "the database is newer (version {found}) than this app (version {supported}); update Ferrocast"
    )]
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
}

pub type Result<T> = std::result::Result<T, StoreError>;
