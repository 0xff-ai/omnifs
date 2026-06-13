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
// A typed `ProviderError` carries its `kind`/`retryable`/`retry-after` through
// to the neutral `TreeErrorKind` so a renderer reproduces the right kernel
// status (a `RateLimited` provider error must surface as EAGAIN, not EIO).
impl From<omnifs_host::Error> for TreeError {
    fn from(err: omnifs_host::Error) -> Self {
        match err {
            omnifs_host::Error::ProviderProtocol(msg) => TreeError::internal(msg),
            omnifs_host::Error::ProviderError(e) => TreeError {
                kind: tree_kind_from_provider(e.kind),
                message: e.message,
                retryable: e.retryable,
                retry_after: e
                    .retry_after
                    .map(|secs| Duration::from_secs(u64::from(secs))),
            },
            other => TreeError::internal(other.to_string()),
        }
    }
}

/// Map a wit provider `error-kind` to the renderer-neutral `TreeErrorKind`.
/// Mirrors the FUSE `provider_error_errno` partition (e.g. `denied` folds onto
/// `PermissionDenied`, `version-mismatch` onto `Internal`) so every renderer's
/// kernel/protocol status matches what the pre-extraction frontends produced.
fn tree_kind_from_provider(kind: omnifs_wit::provider::types::ErrorKind) -> TreeErrorKind {
    use omnifs_wit::provider::types::ErrorKind;
    match kind {
        ErrorKind::NotFound => TreeErrorKind::NotFound,
        ErrorKind::NotADirectory => TreeErrorKind::NotDirectory,
        ErrorKind::NotAFile => TreeErrorKind::IsDirectory,
        ErrorKind::PermissionDenied | ErrorKind::Denied => TreeErrorKind::PermissionDenied,
        ErrorKind::InvalidInput => TreeErrorKind::InvalidInput,
        ErrorKind::TooLarge => TreeErrorKind::TooLarge,
        ErrorKind::RateLimited => TreeErrorKind::RateLimited,
        ErrorKind::Network => TreeErrorKind::Network,
        ErrorKind::Timeout => TreeErrorKind::Timeout,
        ErrorKind::VersionMismatch | ErrorKind::Internal => TreeErrorKind::Internal,
    }
}
