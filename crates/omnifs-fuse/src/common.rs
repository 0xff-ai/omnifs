//! Shared FUSE constants and the protocol-local inode table shape.

use fuser::FileType;
use omnifs_core::path::Path;
use std::time::Duration;

/// Kernel-side entry/attr TTL for a stable exact-size node. The host never
/// expires entries on time, only on capacity or explicit invalidation via the
/// FUSE notifier and provider cache-invalidate effects. We still must hand the
/// kernel a finite `Duration`, so pick one large enough that refresh churn is
/// irrelevant in practice (~136 years). The namespace bakes this into
/// [`omnifs_engine::Attrs::ttl`]; FUSE copies it through.
pub(crate) const TTL: Duration = Duration::from_secs(u32::MAX as u64);
pub(crate) const ROOT_INO: u64 = 1;

/// One row of the FUSE inode table: the protocol identity a kernel inode number
/// rehydrates from. It caches no provider bytes; the namespace owns all
/// projection state. This is the FUSE analogue of the NFS `Inode`.
#[derive(Clone)]
pub(crate) struct Inode {
    /// The parent inode, for `inval_entry` dentry notifications.
    pub(crate) parent: u64,
    /// The leaf name under `parent`, for `inval_entry`.
    pub(crate) name: String,
    pub(crate) kind: NodeKind,
    /// Structural namespace `Path` is the durable FUSE identity across daemon
    /// replacement; the parent/name fields are protocol-local kernel state.
    pub(crate) body: Path,
}

/// Node kind at the FUSE boundary, mapped to a `fuser::FileType` for replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeKind {
    Directory,
    File,
    Symlink,
}

impl NodeKind {
    pub(crate) fn file_type(self) -> FileType {
        match self {
            Self::Directory => FileType::Directory,
            Self::File => FileType::RegularFile,
            Self::Symlink => FileType::Symlink,
        }
    }
}

impl From<&omnifs_engine::EntryKind> for NodeKind {
    fn from(kind: &omnifs_engine::EntryKind) -> Self {
        match kind {
            omnifs_engine::EntryKind::Directory => Self::Directory,
            omnifs_engine::EntryKind::File => Self::File,
            omnifs_engine::EntryKind::Symlink => Self::Symlink,
        }
    }
}

/// A kernel directory snapshot: the ordered children captured at `opendir`,
/// replayed across `readdir` offsets.
pub(crate) type DirSnapshot = Vec<(u64, String, NodeKind)>;
