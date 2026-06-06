//! Provider error to FUSE errno mapping.

use fuser::Errno;
use omnifs_inspector::InspectorOutcome;
use omnifs_wit::provider::types::{ErrorKind, ProviderError};

/// Map a FUSE errno to a stable inspector outcome.
pub(super) fn inspector_outcome(errno: Errno) -> InspectorOutcome {
    InspectorOutcome::from_errno_code(i32::from(errno))
}

/// Map a provider error to its corresponding FUSE errno.
pub(super) fn provider_error_errno(error: &ProviderError) -> Errno {
    match error.kind {
        ErrorKind::NotFound => Errno::ENOENT,
        ErrorKind::NotADirectory => Errno::ENOTDIR,
        ErrorKind::NotAFile => Errno::EISDIR,
        ErrorKind::PermissionDenied | ErrorKind::Denied => Errno::EACCES,
        ErrorKind::InvalidInput => Errno::EINVAL,
        ErrorKind::TooLarge => Errno::EFBIG,
        ErrorKind::RateLimited => Errno::EAGAIN,
        ErrorKind::Network
        | ErrorKind::Timeout
        | ErrorKind::VersionMismatch
        | ErrorKind::Internal => Errno::EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_wit::provider::types::ErrorKind;

    #[test]
    fn rate_limited_maps_to_eagain() {
        let error = ProviderError {
            kind: ErrorKind::RateLimited,
            message: "rate limited".to_string(),
            retryable: true,
            retry_after: Some(3),
        };
        assert_eq!(
            i32::from(provider_error_errno(&error)),
            i32::from(Errno::EAGAIN)
        );
    }
}
