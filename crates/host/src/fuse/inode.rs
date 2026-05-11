//! Node allocation and attribute generation for FUSE.
//!
//! Manages the mapping from virtual paths to inode numbers with
//! deduplication and stale entry updates.

use crate::cache::{EntryKindCache, EntryMeta, FileAttrsCache};
use crate::fuse::FuseFs;
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
    pub(crate) kind: EntryKindCache,
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
        kind: EntryKindCache,
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
        kind: EntryKindCache,
        size: u64,
        backing_path: PathBuf,
    ) -> u64 {
        self.get_or_alloc_ino_inner(mount_name, path, kind, None, size, Some(backing_path))
    }

    fn get_or_alloc_ino_inner(
        &self,
        mount_name: &str,
        path: &str,
        kind: EntryKindCache,
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
                    entry.kind = kind;
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
    matches!(existing.size, crate::cache::SizeCache::Exact(_))
        && !matches!(incoming.size, crate::cache::SizeCache::Exact(_))
        && existing.bytes == incoming.bytes
        && existing.stability == incoming.stability
        && existing.version_token == incoming.version_token
        && (matches!(existing.stability, crate::cache::StabilityCache::Immutable)
            || (matches!(existing.stability, crate::cache::StabilityCache::Mutable)
                && existing.version_token.is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{BytesCache, ReadModeCache, SizeCache, StabilityCache};

    fn attrs(size: SizeCache, version_token: Option<&str>) -> FileAttrsCache {
        FileAttrsCache {
            size,
            bytes: BytesCache::Deferred(ReadModeCache::Full),
            stability: StabilityCache::Mutable,
            version_token: version_token.map(str::to_string),
        }
    }

    #[test]
    fn learned_exact_survives_same_version_non_exact_refresh() {
        let existing = attrs(SizeCache::Exact(42), Some("v1"));
        let incoming = attrs(SizeCache::Unknown, Some("v1"));

        assert_eq!(
            merge_inode_attrs(Some(&existing), Some(incoming)),
            Some(existing)
        );
    }

    #[test]
    fn incoming_exact_replaces_learned_exact() {
        let existing = attrs(SizeCache::Exact(42), Some("v1"));
        let incoming = attrs(SizeCache::Exact(7), Some("v1"));

        assert_eq!(
            merge_inode_attrs(Some(&existing), Some(incoming.clone())),
            Some(incoming)
        );
    }

    #[test]
    fn version_change_drops_learned_exact() {
        let existing = attrs(SizeCache::Exact(42), Some("v1"));
        let incoming = attrs(SizeCache::Unknown, Some("v2"));

        assert_eq!(
            merge_inode_attrs(Some(&existing), Some(incoming.clone())),
            Some(incoming)
        );
    }

    #[test]
    fn unversioned_mutable_refresh_drops_learned_exact() {
        let existing = attrs(SizeCache::Exact(42), None);
        let incoming = attrs(SizeCache::Unknown, None);

        assert_eq!(
            merge_inode_attrs(Some(&existing), Some(incoming.clone())),
            Some(incoming)
        );
    }
}
