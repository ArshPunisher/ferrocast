//! Engine errors. The server maps each to an HTTP status with a plain message.

use ferrocast_connector::ConnectorError;
use ferrocast_store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The request is fine but can't run right now (422).
    #[error("{0}")]
    Unavailable(String),
    /// Bad input (400).
    #[error("{0}")]
    Invalid(String),
    /// Clashes with something that exists (409).
    #[error("{0}")]
    Conflict(String),
    /// Too often (429).
    #[error("{0}")]
    TooMany(String),
    #[error("{0} not found")]
    NotFound(&'static str),
    /// Gone or never there, with its own error code (404).
    #[error("{message}")]
    Missing { code: &'static str, message: &'static str, hint: &'static str },
    #[error(transparent)]
    Connector(#[from] ConnectorError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("internal error: {0}")]
    Internal(String),
}
