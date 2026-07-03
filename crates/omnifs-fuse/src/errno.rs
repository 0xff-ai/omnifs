//! Tree error to FUSE errno mapping.

use fuser::Errno;
use omnifs_api::events::InspectorOutcome;
use omnifs_tree::{TreeError, TreeErrorKind};

/// Map a FUSE errno to a stable inspector outcome.
pub(super) fn inspector_outcome(errno: Errno) -> InspectorOutcome {
    InspectorOutcome::from_errno_code(i32::from(errno))
}

/// Map a renderer-neutral [`TreeError`] to its FUSE errno. The projection core
/// owns the error partition (provider `error-kind` already folded into
/// `TreeErrorKind`); the FUSE adapter only chooses the kernel status. A
/// `RateLimited` listing/read surfaces as EAGAIN so `ls`/`cat` retry rather than
/// fail, preserving the pre-extraction behavior.
pub(super) fn tree_error_errno(error: &TreeError) -> Errno {
    match error.kind {
        TreeErrorKind::NotFound => Errno::ENOENT,
        TreeErrorKind::NotDirectory => Errno::ENOTDIR,
        TreeErrorKind::IsDirectory => Errno::EISDIR,
        TreeErrorKind::PermissionDenied => Errno::EACCES,
        TreeErrorKind::InvalidInput => Errno::EINVAL,
        TreeErrorKind::TooLarge => Errno::EFBIG,
        TreeErrorKind::RateLimited => Errno::EAGAIN,
        TreeErrorKind::Timeout | TreeErrorKind::Network | TreeErrorKind::Internal => Errno::EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_maps_to_eagain() {
        let error = TreeError {
            kind: TreeErrorKind::RateLimited,
            message: "rate limited".to_string(),
            retryable: true,
            retry_after: Some(std::time::Duration::from_secs(3)),
        };
        assert_eq!(
            i32::from(tree_error_errno(&error)),
            i32::from(Errno::EAGAIN)
        );
    }
}
