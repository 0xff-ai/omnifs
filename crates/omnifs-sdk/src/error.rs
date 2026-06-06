use core::time::Duration;
use omnifs_wit::provider::types::{
    CalloutError, ErrorKind, OpResult, ProviderError as WitProviderError, ProviderReturn,
};
use std::fmt;

/// Provider result type alias used throughout the SDK and generated code.
pub type Result<T> = core::result::Result<T, ProviderError>;

/// Provider-side error that can be converted into WIT `OpResult::Error`.
#[derive(Clone, Debug)]
pub struct ProviderError {
    pub(crate) kind: ProviderErrorKind,
    pub(crate) message: String,
    pub(crate) retryable: bool,
    /// Structured backoff hint for `RateLimited` (from HTTP `Retry-After`).
    /// `None` for every other kind, and for a 429 whose header was absent or
    /// non-numeric.
    pub(crate) retry_after: Option<Duration>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProviderErrorKind {
    NotFound,
    NotADirectory,
    NotAFile,
    PermissionDenied,
    Network,
    Timeout,
    Denied,
    InvalidInput,
    TooLarge,
    RateLimited,
    VersionMismatch,
    Unimplemented,
    Internal,
}

impl ProviderErrorKind {
    fn is_retryable(self) -> bool {
        matches!(self, Self::Network | Self::Timeout | Self::RateLimited)
    }

    fn kind_tag(self) -> &'static str {
        match self {
            Self::NotFound => "not-found",
            Self::NotADirectory => "not-a-directory",
            Self::NotAFile => "not-a-file",
            Self::PermissionDenied => "permission-denied",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::Denied => "denied",
            Self::InvalidInput => "invalid-input",
            Self::TooLarge => "too-large",
            Self::RateLimited => "rate-limited",
            Self::VersionMismatch => "version-mismatch",
            Self::Unimplemented => "unimplemented",
            Self::Internal => "internal",
        }
    }

    fn wit_kind(self) -> ErrorKind {
        match self {
            Self::NotFound => ErrorKind::NotFound,
            Self::NotADirectory => ErrorKind::NotADirectory,
            Self::NotAFile => ErrorKind::NotAFile,
            Self::PermissionDenied => ErrorKind::PermissionDenied,
            Self::Network => ErrorKind::Network,
            Self::Timeout => ErrorKind::Timeout,
            Self::Denied => ErrorKind::Denied,
            Self::RateLimited => ErrorKind::RateLimited,
            Self::InvalidInput => ErrorKind::InvalidInput,
            Self::TooLarge => ErrorKind::TooLarge,
            Self::VersionMismatch => ErrorKind::VersionMismatch,
            Self::Unimplemented | Self::Internal => ErrorKind::Internal,
        }
    }
}

impl ProviderError {
    fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: kind.is_retryable(),
            retry_after: None,
        }
    }

    /// Attach a structured backoff hint. Meaningful only on a `RateLimited`
    /// error; the SDK breaker and host rate-limit window both read it.
    #[must_use]
    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }

    /// Structured backoff hint, if the upstream supplied one (`Retry-After`).
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Internal, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::NotFound, message)
    }

    pub fn not_a_directory(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::NotADirectory, message)
    }

    pub fn not_a_file(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::NotAFile, message)
    }

    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::PermissionDenied, message)
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::InvalidInput, message)
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Network, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Timeout, message)
    }

    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Denied, message)
    }

    pub fn too_large(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::TooLarge, message)
    }

    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::RateLimited, message)
    }

    pub fn version_mismatch(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::VersionMismatch, message)
    }

    pub fn unimplemented(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Unimplemented, message)
    }

    pub fn from_http_status(status: u16) -> Self {
        match status {
            401 => Self::permission_denied(format!("HTTP {status}")),
            403 => Self::denied(format!("HTTP {status}")),
            404 => Self::not_found(format!("HTTP {status}")),
            408 => Self::timeout(format!("HTTP {status}")),
            429 => Self::rate_limited(format!("HTTP {status}")),
            400..=499 => Self::invalid_input(format!("HTTP {status}")),
            500..=599 => Self::network(format!("HTTP {status}")),
            _ => Self::internal(format!("HTTP {status}")),
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    pub fn kind(&self) -> ProviderErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = if matches!(
            self.kind,
            ProviderErrorKind::Internal | ProviderErrorKind::Unimplemented
        ) {
            self.message.clone()
        } else {
            format!(
                "[{}; retryable={}] {}",
                self.kind.kind_tag(),
                self.retryable,
                self.message
            )
        };
        f.write_str(&message)
    }
}

impl From<ProviderError> for OpResult {
    fn from(error: ProviderError) -> Self {
        OpResult::Error(WitProviderError {
            kind: error.kind.wit_kind(),
            message: error.message,
            retryable: error.retryable,
            retry_after: error
                .retry_after
                .map(|d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX)),
        })
    }
}

impl From<ProviderError> for ProviderReturn {
    fn from(error: ProviderError) -> Self {
        ProviderReturn::terminal(OpResult::from(error))
    }
}

impl From<CalloutError> for ProviderError {
    fn from(error: CalloutError) -> Self {
        let message = format!("callout error: {}", error.message);
        match error.kind {
            ErrorKind::NotFound => Self::not_found(message),
            ErrorKind::NotADirectory => Self::not_a_directory(message),
            ErrorKind::NotAFile => Self::not_a_file(message),
            ErrorKind::PermissionDenied => Self::permission_denied(message),
            ErrorKind::Network => Self::network(message),
            ErrorKind::Timeout => Self::timeout(message),
            ErrorKind::Denied => Self::denied(message),
            ErrorKind::RateLimited => Self::rate_limited(message),
            ErrorKind::InvalidInput => Self::invalid_input(message),
            ErrorKind::TooLarge => Self::too_large(message),
            ErrorKind::VersionMismatch => Self::version_mismatch(message),
            ErrorKind::Internal => Self::internal(message),
        }
    }
}

impl std::error::Error for ProviderError {}
