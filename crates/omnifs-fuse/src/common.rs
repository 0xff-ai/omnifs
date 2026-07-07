//! Shared FUSE constants and the protocol-local inode table shape.

use fuser::FileType;
use omnifs_engine::NodeId;
use std::path::PathBuf;
use std::time::Duration;

/// Kernel-side entry/attr TTL for a stable exact-size node. The host never
/// expires entries on time, only on capacity or explicit invalidation via the
/// FUSE notifier and provider cache-invalidate effects. We still must hand the
/// kernel a finite `Duration`, so pick one large enough that refresh churn is
/// irrelevant in practice (~136 years). The namespace bakes this into
/// [`omnifs_engine::Attrs::ttl`]; FUSE copies it through and only names the
/// constant for the synthetic root and backing paths it stats itself.
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
    pub(crate) body: Body,
}

/// What a FUSE inode projects.
#[derive(Clone)]
pub(crate) enum Body {
    /// A namespace node: resolution, attrs, listing, and reads go through the
    /// [`omnifs_engine::Namespace`] via this handle.
    Node(NodeId),
    /// A resolved treeref subtree root: it is a namespace node, but its
    /// directory is served locally from `root` (its children have no namespace
    /// identity).
    Subtree { node: NodeId, root: PathBuf },
    /// A pure filesystem child under a subtree, served entirely from `path`.
    Backing(PathBuf),
}

impl Body {
    /// The backing directory/file this inode serves from the local filesystem,
    /// for a subtree root or a backing child.
    pub(crate) fn backing(&self) -> Option<&PathBuf> {
        match self {
            Self::Subtree { root, .. } => Some(root),
            Self::Backing(path) => Some(path),
            Self::Node(_) => None,
        }
    }

    /// The namespace handle this inode resolves through, absent for a pure
    /// backing child.
    pub(crate) fn node(&self) -> Option<NodeId> {
        match self {
            Self::Node(node) | Self::Subtree { node, .. } => Some(*node),
            Self::Backing(_) => None,
        }
    }
}

/// Node kind at the FUSE boundary, mapped to a `fuser::FileType` for replies.
/// A namespace `Subtree` (resolved treeref) presents as a directory.
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

impl From<&omnifs_engine::NsEntryKind> for NodeKind {
    fn from(kind: &omnifs_engine::NsEntryKind) -> Self {
        match kind {
            omnifs_engine::NsEntryKind::Directory | omnifs_engine::NsEntryKind::Subtree { .. } => {
                Self::Directory
            },
            omnifs_engine::NsEntryKind::File => Self::File,
            omnifs_engine::NsEntryKind::Symlink => Self::Symlink,
        }
    }
}

/// A kernel directory snapshot: the ordered children captured at `opendir`,
/// replayed across `readdir` offsets.
pub(crate) type DirSnapshot = Vec<(u64, String, NodeKind)>;
