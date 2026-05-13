//! Node allocation and attribute generation for FUSE.
//!
//! Manages the mapping from virtual paths to inode numbers with
//! deduplication and stale entry updates.

use crate::cache::{EntryMeta, FileAttrsCache};
use crate::fuse::FuseFs;
use crate::omnifs::provider::types as wit_types;
use crate::path_key::PathKey;
use fuser::{FileAttr, FileType, INodeNo};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

// SAFETY: libc::getuid() and libc::getgid() are trivially safe.
#[allow(unsafe_code)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[allow(unsafe_code)]
fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

/// Tracks the per-node state keyed by inode number for a provider mount.
pub(crate) struct NodeEntry {
    pub(crate) mount_name: String,
    pub(crate) path: String,
    pub(crate) kind: wit_types::EntryKind,
    pub(crate) attrs: Option<FileAttrsCache>,
    pub(crate) size: u64,
    /// When set, FUSE operations for this inode serve directly from the backing
    /// filesystem instead of routing through the Wasm provider.
    pub(crate) backing_path: Option<PathBuf>,
}

impl FuseFs {
    pub(crate) fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn get_or_alloc_ino(
        &self,
        mount_name: &str,
        path: &str,
        kind: wit_types::EntryKind,
        size: u64,
    ) -> u64 {
        self.get_or_alloc_ino_inner(mount_name, path, kind, None, size, None)
    }

    pub(crate) fn get_or_alloc_ino_meta(
        &self,
        mount_name: &str,
        path: &str,
        meta: EntryMeta,
    ) -> u64 {
        let size = meta.st_size();
        self.get_or_alloc_ino_inner(mount_name, path, meta.kind, meta.attrs, size, None)
    }

    pub(crate) fn get_or_alloc_ino_backing(
        &self,
        mount_name: &str,
        path: &str,
        kind: wit_types::EntryKind,
        size: u64,
        backing_path: PathBuf,
    ) -> u64 {
        self.get_or_alloc_ino_inner(mount_name, path, kind, None, size, Some(backing_path))
    }

    fn get_or_alloc_ino_inner(
        &self,
        mount_name: &str,
        path: &str,
        kind: wit_types::EntryKind,
        attrs: Option<FileAttrsCache>,
        size: u64,
        backing_path: Option<PathBuf>,
    ) -> u64 {
        let key = PathKey::new(mount_name, path);
        // Use entry API to atomically check-or-insert, avoiding a race where
        // two concurrent lookups for the same (mount, path) allocate different inodes.
        // Use and_modify to update kind/size on existing entries (stale inode fix).
        let incoming_attrs = attrs;
        *self
            .path_to_inode
            .entry(key)
            .and_modify(|existing_ino| {
                if let Some(mut entry) = self.inodes.get_mut(existing_ino) {
                    let merged_attrs =
                        merge_inode_attrs(entry.attrs.as_ref(), incoming_attrs.clone());
                    let merged_size = merged_attrs.as_ref().map_or(size, FileAttrsCache::st_size);
                    entry.kind = kind.clone();
                    entry.attrs = merged_attrs;
                    entry.size = merged_size;
                    if backing_path.is_some() {
                        entry.backing_path.clone_from(&backing_path);
                    }
                }
            })
            .or_insert_with(|| {
                let ino = self.alloc_ino();
                self.inodes.insert(
                    ino,
                    NodeEntry {
                        mount_name: mount_name.to_string(),
                        path: path.to_string(),
                        kind,
                        attrs: incoming_attrs.clone(),
                        size,
                        backing_path,
                    },
                );
                ino
            })
    }

    #[allow(clippy::unused_self)]
    pub(crate) fn dir_attr(&self, ino: u64) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino: INodeNo(ino),
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2,
            uid: current_uid(),
            gid: current_gid(),
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    #[allow(clippy::unused_self)]
    pub(crate) fn file_attr(&self, ino: u64, size: u64) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: current_uid(),
            gid: current_gid(),
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    /// Build a `FileAttr` from real filesystem metadata.
    #[allow(clippy::unused_self)]
    pub(crate) fn attr_from_metadata(&self, ino: u64, meta: &std::fs::Metadata) -> FileAttr {
        let kind = if meta.is_dir() {
            FileType::Directory
        } else if meta.is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };
        let perm = if meta.is_dir() { 0o555 } else { 0o444 };
        let nlink = if meta.is_dir() { 2 } else { 1 };
        let now = SystemTime::now();

        FileAttr {
            ino: INodeNo(ino),
            size: meta.len(),
            blocks: meta.len().div_ceil(512),
            atime: meta.accessed().unwrap_or(now),
            mtime: meta.modified().unwrap_or(now),
            ctime: meta.modified().unwrap_or(now),
            crtime: meta.created().unwrap_or(now),
            kind,
            perm,
            nlink,
            uid: current_uid(),
            gid: current_gid(),
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }
}

fn merge_inode_attrs(
    existing: Option<&FileAttrsCache>,
    incoming: Option<FileAttrsCache>,
) -> Option<FileAttrsCache> {
    match (existing, incoming) {
        (Some(existing), Some(incoming))
            if should_preserve_learned_exact_size(existing, &incoming) =>
        {
            Some(existing.clone())
        },
        (_, incoming) => incoming,
    }
}

fn should_preserve_learned_exact_size(
    existing: &FileAttrsCache,
    incoming: &FileAttrsCache,
) -> bool {
    matches!(existing.size, wit_types::FileSize::Exact(_))
        && !matches!(incoming.size, wit_types::FileSize::Exact(_))
        && proj_bytes_equal(&existing.bytes, &incoming.bytes)
        && existing.stability == incoming.stability
        && existing.version_token == incoming.version_token
        && (matches!(existing.stability, wit_types::Stability::Immutable)
            || (matches!(existing.stability, wit_types::Stability::Mutable)
                && existing.version_token.is_some()))
}

// `wit_types::ProjBytes` does not derive `PartialEq` (bindgen `additional_derives`
// would need to opt every variant in). Compare structurally for the cache's
// learned-size promotion check.
fn proj_bytes_equal(a: &wit_types::ProjBytes, b: &wit_types::ProjBytes) -> bool {
    match (a, b) {
        (wit_types::ProjBytes::Inline(x), wit_types::ProjBytes::Inline(y)) => x == y,
        (wit_types::ProjBytes::Deferred(x), wit_types::ProjBytes::Deferred(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(size: wit_types::FileSize, version_token: Option<&str>) -> FileAttrsCache {
        FileAttrsCache {
            size,
            bytes: wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full),
            stability: wit_types::Stability::Mutable,
            version_token: version_token.map(str::to_string),
        }
    }

    // `wit_types::FileSize` doesn't derive PartialEq; compare structurally for tests.
    fn size_eq(a: &wit_types::FileSize, b: &wit_types::FileSize) -> bool {
        matches!(
            (a, b),
            (wit_types::FileSize::Exact(x), wit_types::FileSize::Exact(y)) if x == y
        ) || matches!(
            (a, b),
            (wit_types::FileSize::NonZero, wit_types::FileSize::NonZero)
                | (wit_types::FileSize::Unknown, wit_types::FileSize::Unknown)
        )
    }

    fn attrs_eq(a: &FileAttrsCache, b: &FileAttrsCache) -> bool {
        size_eq(&a.size, &b.size)
            && a.stability == b.stability
            && a.version_token == b.version_token
    }

    #[test]
    fn learned_exact_survives_same_version_non_exact_refresh() {
        let existing = attrs(wit_types::FileSize::Exact(42), Some("v1"));
        let incoming = attrs(wit_types::FileSize::Unknown, Some("v1"));

        let merged = merge_inode_attrs(Some(&existing), Some(incoming)).unwrap();
        assert!(attrs_eq(&merged, &existing));
    }

    #[test]
    fn incoming_exact_replaces_learned_exact() {
        let existing = attrs(wit_types::FileSize::Exact(42), Some("v1"));
        let incoming = attrs(wit_types::FileSize::Exact(7), Some("v1"));

        let merged = merge_inode_attrs(Some(&existing), Some(incoming.clone())).unwrap();
        assert!(attrs_eq(&merged, &incoming));
    }

    #[test]
    fn version_change_drops_learned_exact() {
        let existing = attrs(wit_types::FileSize::Exact(42), Some("v1"));
        let incoming = attrs(wit_types::FileSize::Unknown, Some("v2"));

        let merged = merge_inode_attrs(Some(&existing), Some(incoming.clone())).unwrap();
        assert!(attrs_eq(&merged, &incoming));
    }

    #[test]
    fn unversioned_mutable_refresh_drops_learned_exact() {
        let existing = attrs(wit_types::FileSize::Exact(42), None);
        let incoming = attrs(wit_types::FileSize::Unknown, None);

        let merged = merge_inode_attrs(Some(&existing), Some(incoming.clone())).unwrap();
        assert!(attrs_eq(&merged, &incoming));
    }
}
