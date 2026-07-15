//! Renderer-neutral error surface for the projection core.

use std::time::Duration;

use crate::ProviderErrorClass;

/// Renderer-neutral error kind. Promoted from the omnifs-nfs `ProviderFsError`
/// shape: the FUSE adapter maps it to errno, the NFS adapter to nfsstat4. The
/// wit_types `ProviderError` never crosses the namespace boundary.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    Retry,
    Gone,
    Terminal,
    TooLarge,
}

impl TreeErrorKind {
    pub fn retry_class(self) -> RetryClass {
        match self {
            Self::RateLimited | Self::Timeout | Self::Network => RetryClass::Retry,
            Self::NotFound | Self::NotDirectory | Self::IsDirectory => RetryClass::Gone,
            Self::TooLarge => RetryClass::TooLarge,
            Self::PermissionDenied | Self::InvalidInput | Self::Internal => RetryClass::Terminal,
        }
    }
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

    pub fn too_large(message: impl Into<String>) -> Self {
        Self {
            kind: TreeErrorKind::TooLarge,
            message: message.into(),
            retryable: false,
            retry_after: None,
        }
    }

    pub fn is_directory(message: impl Into<String>) -> Self {
        Self {
            kind: TreeErrorKind::IsDirectory,
            message: message.into(),
            retryable: false,
            retry_after: None,
        }
    }
}

pub type Result<T> = std::result::Result<T, TreeError>;

// Host `EngineError` variants: Wasmtime, ProviderProtocol(String),
// ProviderError(wit_types::ProviderError).
// A typed `ProviderError` carries its `kind`/`retryable`/`retry-after` through
// to the neutral `TreeErrorKind` so a renderer reproduces the right kernel
// status (a `RateLimited` provider error must surface as EAGAIN, not EIO).
impl From<crate::EngineError> for TreeError {
    fn from(err: crate::EngineError) -> Self {
        match err {
            crate::EngineError::Wasmtime(error) => TreeError::internal(error.to_string()),
            crate::EngineError::ProviderProtocol(msg) => TreeError::internal(msg),
            crate::EngineError::ProviderError(e) => TreeError {
                kind: TreeErrorKind::from(
                    crate::EngineError::ProviderError(e.clone())
                        .provider_class()
                        .unwrap_or(ProviderErrorClass::Internal),
                ),
                message: e.message,
                retryable: e.retryable,
                retry_after: e
                    .retry_after
                    .map(|secs| Duration::from_secs(u64::from(secs))),
            },
        }
    }
}

/// Preserve the shared semantic partition across the engine and renderer-neutral
/// tree layers. Each frontend maps this class to its own protocol status.
impl From<ProviderErrorClass> for TreeErrorKind {
    fn from(kind: ProviderErrorClass) -> Self {
        match kind {
            ProviderErrorClass::NotFound => Self::NotFound,
            ProviderErrorClass::NotDirectory => Self::NotDirectory,
            ProviderErrorClass::IsDirectory => Self::IsDirectory,
            ProviderErrorClass::PermissionDenied => Self::PermissionDenied,
            ProviderErrorClass::InvalidInput => Self::InvalidInput,
            ProviderErrorClass::TooLarge => Self::TooLarge,
            ProviderErrorClass::RateLimited => Self::RateLimited,
            ProviderErrorClass::Network => Self::Network,
            ProviderErrorClass::Timeout => Self::Timeout,
            ProviderErrorClass::Internal => Self::Internal,
        }
    }
}
