//! Public error type of kioku-core (`thiserror`); internal code uses `anyhow`.

use thiserror::Error;

/// Errors returned by the `Store` facade and other public core APIs.
#[derive(Debug, Error)]
pub enum Error {
    /// The requested session, project, page or handoff does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The caller passed something the core refuses (bad path, empty title, …).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// A conditional write or delivery ID conflicts with stored data.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Anything else: I/O, SQLite, tantivy, serialization.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl Error {
    /// Builds a `NotFound` error from anything printable.
    pub fn not_found(what: impl Into<String>) -> Error {
        Error::NotFound(what.into())
    }

    /// Builds an `InvalidInput` error from anything printable.
    pub fn invalid(what: impl Into<String>) -> Error {
        Error::InvalidInput(what.into())
    }
}

/// Result alias used by the public core API.
pub type Result<T> = std::result::Result<T, Error>;
