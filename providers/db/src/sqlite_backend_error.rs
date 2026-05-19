//! Backend errors. Kept thin: providers surface domain errors
//! through `ProviderError`, so the backend layer just classifies
//! between "open failed" and everything else.

use std::fmt;

#[derive(Debug)]
pub(crate) enum BackendError {
    /// The connection could not be opened (file missing, sandbox
    /// denied, journal-mode mismatch, etc.).
    Open(String),
    /// A query against an open connection failed.
    Query(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(msg) => write!(f, "open database: {msg}"),
            Self::Query(msg) => write!(f, "query database: {msg}"),
        }
    }
}

impl std::error::Error for BackendError {}

impl From<rusqlite::Error> for BackendError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Query(err.to_string())
    }
}
