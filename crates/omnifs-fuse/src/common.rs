//! Shared FUSE constants, path helpers, and inode read targets.

use omnifs_core::path::Path;
use omnifs_core::view as view_types;
use omnifs_core::view::{EntryMeta, FileAttrsCache};
use omnifs_host::pagination;
use omnifs_wit::provider::types as wit_types;
use std::path::PathBuf;
use std::time::Duration;

/// Kernel-side entry/attr TTL. The host never expires entries on time,
/// only on capacity or explicit invalidation via the FUSE notifier and
/// provider cache-invalidate effects. We still must hand the kernel
/// a finite Duration, so pick one large enough that refresh churn is
/// irrelevant in practice (~136 years).
pub(crate) const TTL: Duration = Duration::from_secs(u32::MAX as u64);
pub(crate) const TTL_DYNAMIC: Duration = Duration::from_secs(0);
pub(crate) const ROOT_INO: u64 = 1;

pub(crate) type DirSnapshot = Vec<(u64, String, wit_types::EntryKind)>;

/// Construct a placeholder `wit_types::EntryKind::File(FileOut)` for FUSE
/// snapshot/inode use where only the kind discriminator matters and no
/// real projection data is available (e.g. from a backing-path read or a
/// pre-projection allocation). The embedded `FileOut` is never inspected;
/// only the variant tag is used for `FileType` resolution.
pub(crate) fn file_kind_placeholder() -> wit_types::EntryKind {
    wit_types::EntryKind::File(wit_types::FileOut {
        attrs: wit_types::FileAttrs {
            size: wit_types::FileSize::Unknown,
            stability: wit_types::Stability::Mutable,
            version_token: None,
        },
        bytes: wit_types::ByteSource::Deferred(wit_types::ReadMode::Full),
        content_type: None,
    })
}

/// Volatile-file `EntryMeta` for a synthetic mount-root ignore file. Its size
/// is exact (the ignore content is fixed) so `ls -l`/`cat` report the right
/// length without a learned-size round trip.
pub(crate) fn root_ignore_meta() -> EntryMeta {
    EntryMeta::file(FileAttrsCache {
        size: view_types::FileSize::Exact(pagination::IGNORE_CONTENT.len() as u64),
        bytes: view_types::ByteSource::Deferred(view_types::ReadMode::Full),
        stability: view_types::Stability::Volatile,
        version_token: None,
    })
}

pub(crate) fn is_mount_root(path: &str) -> bool {
    path == Path::ROOT
}

pub(crate) fn join_child_path(parent_path: &str, name: &str) -> String {
    let parent = Path::parse(parent_path).expect("parent path must be absolute");
    let child = parent
        .join(name)
        .expect("child name must be a valid path segment");
    child.as_str().to_string()
}

/// Split a protocol path into `(parent, leaf)`. Returns `None` for the mount
/// root (`/`).
pub(crate) fn split_parent_leaf(path: &str) -> Option<(String, String)> {
    let path = Path::parse(path).ok()?;
    let (parent, leaf) = path.parent_and_name()?;
    Some((parent.as_str().to_string(), leaf.to_string()))
}

#[derive(Clone)]
pub(crate) struct RangedFileHandle {
    pub(crate) mount_name: String,
    pub(crate) path: String,
    pub(crate) provider_handle: u64,
    pub(crate) attrs: FileAttrsCache,
}

pub(crate) struct FullReadTarget {
    pub(crate) ino: u64,
    pub(crate) fh: u64,
    pub(crate) mount_name: String,
    pub(crate) path: String,
    pub(crate) backing_path: Option<PathBuf>,
    pub(crate) attrs: Option<FileAttrsCache>,
    /// True when the inode is a host-synthesized mount-root ignore file, so
    /// `open` serves its fixed content from `fh` rather than the provider.
    pub(crate) synthetic: bool,
}
