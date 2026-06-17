//! Renderer-neutral error surface for the projection core.

use std::time::Duration;

/// Renderer-neutral error kind. Promoted from the omnifs-nfs `ProviderFsError`
/// shape: the FUSE adapter maps it to errno, the NFS adapter to nfsstat4. The
/// wit_types `ProviderError` never appears in a public `Tree` type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeErrorKind {
    NotFound,
    NotDirectory,
    IsDirectory,
    PermissionDenied,
    InvalidInput,
    TooLarge,
    RateLimited,
    Timeout,
    Network,
    Internal,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct TreeError {
    pub kind: TreeErrorKind,
    pub message: String,
    pub retryable: bool,
    pub retry_after: Option<Duration>,
}

impl TreeError {
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            kind: TreeErrorKind::NotFound,
            message: message.into(),
            retryable: false,
            retry_after: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: TreeErrorKind::Internal,
            message: message.into(),
            retryable: false,
            retry_after: None,
        }
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            kind: TreeErrorKind::InvalidInput,
            message: message.into(),
            retryable: false,
            retry_after: None,
        }
    }
}

pub type Result<T> = std::result::Result<T, TreeError>;

// Host `Error` variants: Wasmtime, ProviderProtocol(String),
// ProviderError(wit_types::ProviderError), UnexpectedOpResult { op, result }.
// Slice 1 maps everything that is not a clean ENOENT to Internal; a richer
// wit ProviderError.kind -> TreeErrorKind mapping lands with read/open
// (slice 3+).
impl From<omnifs_host::Error> for TreeError {
    fn from(err: omnifs_host::Error) -> Self {
        match err {
            omnifs_host::Error::ProviderProtocol(msg) => TreeError::internal(msg),
            omnifs_host::Error::ProviderError(e) => TreeError {
                kind: TreeErrorKind::Internal,
                message: format!("{e:?}"),
                retryable: false,
                retry_after: None,
            },
            other => TreeError::internal(other.to_string()),
        }
    }
}
