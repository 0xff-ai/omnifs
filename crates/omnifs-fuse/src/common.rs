//! Shared FUSE constants, path helpers, and inode read targets.

use fuser::FopenFlags;
use omnifs_core::path::Path;
use omnifs_engine::view::{EntryKind, FileAttrsCache};
use omnifs_engine::{Node, RangedHandle};
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

pub(crate) type DirSnapshot = Vec<(u64, String, EntryKind)>;

#[derive(Debug, Clone)]
pub(crate) enum InodeBody {
    Provider,
    Subtree(PathBuf),
    Synthetic,
}

impl InodeBody {
    pub(crate) fn backing_path(&self) -> Option<&PathBuf> {
        match self {
            Self::Subtree(path) => Some(path),
            Self::Provider | Self::Synthetic => None,
        }
    }

    pub(crate) fn is_backing(&self) -> bool {
        matches!(self, Self::Subtree(_))
    }

    pub(crate) fn is_synthetic(&self) -> bool {
        matches!(self, Self::Synthetic)
    }
}

/// File `EntryMeta` for a synthetic mount-root ignore file, mirroring
/// the `Tree` synthetic projection. The live adapter materializes ignore files
/// from `Tree` entries with synthetic origin; the in-crate harness uses this to seed a
/// host-synthesized inode directly.
#[cfg(test)]
pub(crate) fn root_ignore_meta() -> omnifs_engine::view::EntryMeta {
    omnifs_engine::view::EntryMeta::file(
        FileAttrsCache::deferred(
            omnifs_engine::view::FileSize::Exact(b"@*\n".len() as u64),
            omnifs_engine::view::ReadMode::Full,
            omnifs_engine::view::Stability::Stable,
            None,
        )
        .expect("root ignore attrs are valid"),
    )
}

/// Split a protocol path into `(parent, leaf)`. Returns `None` for the mount
/// root (`/`).
pub(crate) fn split_parent_leaf(path: &Path) -> Option<(Path, String)> {
    let (parent, leaf) = path.parent_and_name()?;
    Some((parent, leaf.to_string()))
}

/// A `Tree`-owned ranged handle paired with the kernel inode it serves. The
/// inode is FUSE identity (not `Tree`'s concern); the adapter keeps it beside
/// the handle so `release` can scope the live-follow `follow_sizes` cleanup to
/// the ino without `Tree` ever knowing about inodes.
pub(crate) struct RangedSlot {
    pub(crate) ino: u64,
    pub(crate) handle: RangedHandle,
}

pub(crate) struct FullReadTarget {
    pub(crate) ino: u64,
    pub(crate) fh: u64,
    pub(crate) mount_name: String,
    pub(crate) path: Path,
    pub(crate) body: InodeBody,
    pub(crate) attrs: Option<FileAttrsCache>,
}

impl FullReadTarget {
    pub(crate) fn provider_node(&self) -> Node {
        Node::provider_file(
            self.mount_name.clone(),
            self.path.clone(),
            self.attrs.clone(),
        )
    }

    pub(crate) fn parent_and_leaf(&self) -> Option<(Path, String)> {
        split_parent_leaf(&self.path)
    }

    pub(crate) fn is_synthetic_candidate(&self) -> bool {
        self.body.is_synthetic()
            || self
                .parent_and_leaf()
                .is_some_and(|(_, leaf)| leaf == "@next" || leaf == "@all")
    }

    pub(crate) fn is_ranged(&self) -> bool {
        self.attrs
            .as_ref()
            .is_some_and(FileAttrsCache::is_deferred_ranged)
    }

    pub(crate) fn should_prefetch_full(&self) -> bool {
        self.attrs
            .as_ref()
            .is_some_and(|attrs| attrs.is_deferred_full() && !attrs.has_exact_size())
    }

    pub(crate) fn lazy_open_flags(&self) -> FopenFlags {
        self.attrs
            .as_ref()
            .filter(|attrs| attrs.should_direct_io())
            .map_or_else(FopenFlags::empty, |_| FopenFlags::FOPEN_DIRECT_IO)
    }
}
