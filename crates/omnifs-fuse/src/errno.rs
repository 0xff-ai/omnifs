//! Namespace error to FUSE errno mapping.

use fuser::Errno;
use omnifs_engine::{NsError, NsRetryClass};

/// Map a plain-data [`NsError`] to its FUSE errno. The namespace owns the error
/// partition (provider `error-kind` already folded into `NsError`); the FUSE
/// adapter only chooses the kernel status. A `RateLimited` listing/read surfaces
/// as `EAGAIN` so `ls`/`cat` retry rather than fail; `Timeout`/`Network` stay
/// `EIO` because FUSE has no deferral channel.
pub(super) fn ns_error_errno(error: &NsError) -> Errno {
    match error.retry_class() {
        NsRetryClass::Retry => match error {
            NsError::RateLimited { .. } => Errno::EAGAIN,
            _ => Errno::EIO,
        },
        NsRetryClass::TooLarge => Errno::EFBIG,
        NsRetryClass::Gone => match error {
            NsError::NotFound => Errno::ENOENT,
            NsError::NotDirectory => Errno::ENOTDIR,
            NsError::IsDirectory => Errno::EISDIR,
            _ => Errno::EIO,
        },
        NsRetryClass::Terminal => match error {
            NsError::Permission => Errno::EACCES,
            NsError::Invalid => Errno::EINVAL,
            _ => Errno::EIO,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_maps_to_eagain() {
        let error = NsError::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(3)),
        };
        assert_eq!(i32::from(ns_error_errno(&error)), i32::from(Errno::EAGAIN));
    }

    #[test]
    fn gone_kinds_map_to_their_errnos() {
        assert_eq!(
            i32::from(ns_error_errno(&NsError::NotFound)),
            i32::from(Errno::ENOENT)
        );
        assert_eq!(
            i32::from(ns_error_errno(&NsError::IsDirectory)),
            i32::from(Errno::EISDIR)
        );
        assert_eq!(
            i32::from(ns_error_errno(&NsError::OfflineMiss)),
            i32::from(Errno::EIO)
        );
    }
}
