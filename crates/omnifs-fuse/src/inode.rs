//! Node allocation and attribute generation for FUSE.
//!
//! Manages the mapping from virtual paths to inode numbers with
//! deduplication and stale entry updates.

use crate::Frontend;
use fuser::{FileAttr, FileType, INodeNo};
use omnifs_core::path::Path;
use omnifs_core::view::{EntryMeta, FileAttrsCache};
use omnifs_host::path_key::PathKey;
use omnifs_host::wit_protocol;
use omnifs_wit::provider::types as wit_types;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

// SAFETY: getuid(2) takes no arguments, reads only kernel-maintained process state,
// and is always safe to call on any POSIX system.
#[allow(unsafe_code)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

// SAFETY: getgid(2) takes no arguments, reads only kernel-maintained process state,
// and is always safe to call on any POSIX system.
#[allow(unsafe_code)]
fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

/// Tracks the per-node state keyed by inode number for a provider mount.
pub(crate) struct NodeEntry {
    pub(crate) mount_name: String,
    pub(crate) path: Path,
    pub(crate) kind: wit_types::EntryKind,
    pub(crate) attrs: Option<FileAttrsCache>,
    pub(crate) size: u64,
    /// When set, FUSE operations for this inode serve directly from the backing
    /// filesystem instead of routing through the Wasm provider.
    pub(crate) backing_path: Option<PathBuf>,
    /// When set, this inode is a host-synthesized mount-root ignore file
    /// (`.gitignore`/`.ignore`/`.rgignore`). `open` serves its fixed content
    /// from a per-`fh` buffer instead of calling the provider. A real provider
    /// file of the same name is never marked synthetic, so it reads normally.
    pub(crate) synthetic: bool,
}

impl NodeEntry {
    fn refresh(
        &mut self,
        incoming_kind: wit_types::EntryKind,
        incoming_attrs: Option<&FileAttrsCache>,
        fallback_size: u64,
    ) {
        let attrs = self.refreshed_attrs(&incoming_kind, incoming_attrs);
        let size = attrs
            .as_ref()
            .map_or(fallback_size, FileAttrsCache::st_size);

        self.kind = incoming_kind;
        self.attrs = attrs;
        self.size = size;
    }

    fn refreshed_attrs(
        &self,
        incoming_kind: &wit_types::EntryKind,
        incoming_attrs: Option<&FileAttrsCache>,
    ) -> Option<FileAttrsCache> {
        match (self.attrs.as_ref(), incoming_attrs) {
            (Some(existing), Some(incoming)) => {
                // Keep the real, read-observed attrs when a silent refresh must
                // not erase a learned size; otherwise take the incoming attrs.
                let refreshed = if existing.keeps_learned_size_over(incoming) {
                    existing.clone()
                } else {
                    incoming.clone()
                };
                Some(refreshed)
            },
            // A refresh that carries no attributes (a listing entry with only a
            // kind) is silent about size, so keep the attrs (and learned size)
            // we already hold for a file. Backing-FS files re-`stat` at getattr,
            // so a stale value here never reaches them.
            (Some(existing), None)
                if matches!(
                    (&self.kind, incoming_kind),
                    (wit_types::EntryKind::File(_), wit_types::EntryKind::File(_))
                ) =>
            {
                Some(existing.clone())
            },
            (_, incoming) => incoming.cloned(),
        }
    }
}

/// What the caller knows about an inode allocation, which drives how the
/// `synthetic` flag is updated on an existing (refreshed) inode.
///
/// The flag is set-once-true by [`NodeOrigin::Synthetic`] and cleared by a
/// genuine resolution of a real node ([`NodeOrigin::Provider`] or
/// [`NodeOrigin::Backing`]); a [`NodeOrigin::Refresh`] (cache-driven re-touch
/// that does not authoritatively know the origin) leaves it unchanged, so a
/// stale dirents/control refresh can never silently demote a still-synthetic
/// node, while a real provider file of the same name still wins.
pub(crate) enum NodeOrigin {
    /// A cache-driven refresh (cached dirents/control replay) that does not
    /// assert the node's origin. Leaves `synthetic` unchanged on an existing
    /// inode; defaults to non-synthetic on first insert.
    Refresh,
    /// A genuine provider resolution proved a real node at this path. Clears
    /// `synthetic` (a real provider file wins over a host-synthesized one).
    Provider,
    /// Passthrough to a real backing-filesystem file. Clears `synthetic`.
    Backing(PathBuf),
    /// Host-synthesized node (a mount-root ignore file). Sets `synthetic`.
    Synthetic,
}

/// How an inode refresh updates the `synthetic` flag of an existing node.
enum SyntheticUpdate {
    /// Set `synthetic = true` (host synthesis).
    Set,
    /// Set `synthetic = false` (a real node resolved here).
    Clear,
    /// Leave the existing `synthetic` flag untouched (origin-agnostic refresh).
    Keep,
}

impl NodeOrigin {
    fn backing_path(&self) -> Option<&PathBuf> {
        match self {
            NodeOrigin::Backing(path) => Some(path),
            NodeOrigin::Refresh | NodeOrigin::Provider | NodeOrigin::Synthetic => None,
        }
    }

    /// How a fresh insert should initialize `synthetic`.
    fn synthetic_on_insert(&self) -> bool {
        matches!(self, NodeOrigin::Synthetic)
    }

    /// How a refresh of an existing inode should update `synthetic`.
    fn synthetic_update(&self) -> SyntheticUpdate {
        match self {
            NodeOrigin::Synthetic => SyntheticUpdate::Set,
            NodeOrigin::Provider | NodeOrigin::Backing(_) => SyntheticUpdate::Clear,
            NodeOrigin::Refresh => SyntheticUpdate::Keep,
        }
    }
}

impl Frontend {
    pub(crate) fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn get_or_alloc_ino(
        &self,
        mount_name: &str,
        path: &Path,
        kind: wit_types::EntryKind,
        size: u64,
    ) -> u64 {
        self.get_or_alloc_ino_inner(mount_name, path, kind, None, size, &NodeOrigin::Refresh)
    }

    /// Allocate (or refresh) an inode from cached metadata WITHOUT asserting the
    /// node's origin. A refresh leaves an existing `synthetic` flag untouched,
    /// so replaying a cached listing/control over a synthesized node never
    /// demotes it. Use [`get_or_alloc_ino_meta_resolved`](Self::get_or_alloc_ino_meta_resolved)
    /// when a genuine provider resolution should win over a synthetic marker.
    ///
    /// The live adapter allocates from a `Tree` `Node`/`Entry` with an asserted
    /// origin (`_resolved`/`_synthetic`/`_backing`); this origin-agnostic refresh
    /// variant is exercised by the in-crate harness to prove a cached-metadata
    /// replay does not demote a still-synthetic node.
    #[cfg(test)]
    pub(crate) fn get_or_alloc_ino_meta(
        &self,
        mount_name: &str,
        path: &Path,
        meta: EntryMeta,
    ) -> u64 {
        let size = meta.st_size();
        let kind = wit_protocol::entry_kind_to_wit(&meta.kind);
        self.get_or_alloc_ino_inner(
            mount_name,
            path,
            kind,
            meta.attrs,
            size,
            &NodeOrigin::Refresh,
        )
    }

    /// Allocate (or refresh) an inode for a node a provider lookup/listing just
    /// resolved as real. Clears any prior `synthetic` marker so a real provider
    /// file (e.g. a `.gitignore` the provider projects) wins over the
    /// host-synthesized ignore content.
    pub(crate) fn get_or_alloc_ino_meta_resolved(
        &self,
        mount_name: &str,
        path: &Path,
        meta: EntryMeta,
    ) -> u64 {
        let size = meta.st_size();
        let kind = wit_protocol::entry_kind_to_wit(&meta.kind);
        self.get_or_alloc_ino_inner(
            mount_name,
            path,
            kind,
            meta.attrs,
            size,
            &NodeOrigin::Provider,
        )
    }

    pub(crate) fn get_or_alloc_ino_backing(
        &self,
        mount_name: &str,
        path: &Path,
        kind: wit_types::EntryKind,
        size: u64,
        backing_path: PathBuf,
    ) -> u64 {
        self.get_or_alloc_ino_inner(
            mount_name,
            path,
            kind,
            None,
            size,
            &NodeOrigin::Backing(backing_path),
        )
    }

    /// Allocate (or refresh) the inode for a host-synthesized mount-root ignore
    /// file, marking it `synthetic` so `open` serves it from a per-`fh` buffer
    /// rather than the provider.
    pub(crate) fn get_or_alloc_ino_synthetic(
        &self,
        mount_name: &str,
        path: &Path,
        meta: EntryMeta,
    ) -> u64 {
        let size = meta.st_size();
        let kind = wit_protocol::entry_kind_to_wit(&meta.kind);
        self.get_or_alloc_ino_inner(
            mount_name,
            path,
            kind,
            meta.attrs,
            size,
            &NodeOrigin::Synthetic,
        )
    }

    fn get_or_alloc_ino_inner(
        &self,
        mount_name: &str,
        path: &Path,
        kind: wit_types::EntryKind,
        attrs: Option<FileAttrsCache>,
        size: u64,
        origin: &NodeOrigin,
    ) -> u64 {
        let key = PathKey::with_mount_str(mount_name, path.clone()).expect("runtime mount name");
        // Use entry API to atomically check-or-insert, avoiding a race where
        // two concurrent lookups for the same (mount, path) allocate different inodes.
        // Use and_modify to update kind/size on existing entries (stale inode fix).
        let incoming_attrs = attrs;
        let synthetic_on_insert = origin.synthetic_on_insert();
        let synthetic_update = origin.synthetic_update();
        let backing_path = origin.backing_path();
        *self
            .path_to_inode
            .entry(key)
            .and_modify(|existing_ino| {
                if let Some(mut entry) = self.inodes.get_mut(existing_ino) {
                    entry.refresh(kind.clone(), incoming_attrs.as_ref(), size);
                    // A genuine provider/backing resolution (Clear) overrides a
                    // prior synthetic marker, so a `.gitignore` that later
                    // appears in the provider stops being host-synthesized. An
                    // origin-agnostic refresh (Keep) must NOT flip the flag: a
                    // cached dirents/control replay can't demote a still-
                    // synthetic node.
                    match synthetic_update {
                        SyntheticUpdate::Set => entry.synthetic = true,
                        SyntheticUpdate::Clear => entry.synthetic = false,
                        SyntheticUpdate::Keep => {},
                    }
                    if let Some(backing_path) = backing_path {
                        entry.backing_path = Some(backing_path.clone());
                    }
                }
            })
            .or_insert_with(|| {
                let ino = self.alloc_ino();
                self.inodes.insert(
                    ino,
                    NodeEntry {
                        mount_name: mount_name.to_string(),
                        path: path.clone(),
                        kind,
                        attrs: incoming_attrs.clone(),
                        size,
                        backing_path: backing_path.cloned(),
                        synthetic: synthetic_on_insert,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::file_kind_placeholder;
    use omnifs_core::view as view_types;

    fn attrs(size: view_types::FileSize, version_token: Option<&str>) -> FileAttrsCache {
        attrs_with(size, view_types::Stability::Dynamic, version_token)
    }

    fn attrs_with(
        size: view_types::FileSize,
        stability: view_types::Stability,
        version_token: Option<&str>,
    ) -> FileAttrsCache {
        FileAttrsCache {
            size,
            bytes: view_types::ByteSource::Deferred(view_types::ReadMode::Full),
            stability,
            version_token: version_token.map(str::to_string),
        }
    }

    fn file_entry(attrs: FileAttrsCache) -> NodeEntry {
        let size = attrs.st_size();
        NodeEntry {
            mount_name: "test".to_string(),
            path: Path::parse("/hello/fresh-full").unwrap(),
            kind: file_kind_placeholder(),
            attrs: Some(attrs),
            size,
            backing_path: None,
            synthetic: false,
        }
    }

    fn refresh_file(entry: &mut NodeEntry, incoming: &FileAttrsCache) {
        entry.refresh(file_kind_placeholder(), Some(incoming), incoming.st_size());
    }

    struct RefreshCase {
        name: &'static str,
        existing: FileAttrsCache,
        incoming: FileAttrsCache,
        expected: FileAttrsCache,
    }

    #[test]
    fn refresh_attrs_follow_learned_size_policy() {
        let cases = [
            {
                let existing = attrs(view_types::FileSize::Exact(42), Some("v1"));
                let incoming = attrs(view_types::FileSize::Unknown, Some("v1"));
                RefreshCase {
                    name: "silent same-version refresh keeps learned exact",
                    existing: existing.clone(),
                    incoming,
                    expected: existing,
                }
            },
            {
                let incoming = attrs(view_types::FileSize::Exact(7), Some("v1"));
                RefreshCase {
                    name: "incoming exact replaces learned exact",
                    existing: attrs(view_types::FileSize::Exact(42), Some("v1")),
                    incoming: incoming.clone(),
                    expected: incoming,
                }
            },
            {
                let incoming = attrs(view_types::FileSize::Unknown, Some("v2"));
                RefreshCase {
                    name: "version change drops learned exact",
                    existing: attrs(view_types::FileSize::Exact(42), Some("v1")),
                    expected: incoming.clone(),
                    incoming,
                }
            },
            {
                let existing = attrs(view_types::FileSize::Exact(42), None);
                let incoming = attrs(view_types::FileSize::Unknown, None);
                RefreshCase {
                    name: "unversioned dynamic refresh keeps learned exact",
                    existing: existing.clone(),
                    incoming,
                    expected: existing,
                }
            },
            {
                let existing = attrs_with(
                    view_types::FileSize::Exact(42),
                    view_types::Stability::Stable,
                    None,
                );
                let incoming = attrs_with(
                    view_types::FileSize::Unknown,
                    view_types::Stability::Stable,
                    None,
                );
                RefreshCase {
                    name: "stable silent refresh keeps learned exact",
                    existing: existing.clone(),
                    incoming,
                    expected: existing,
                }
            },
            {
                let existing = attrs_with(
                    view_types::FileSize::Exact(42),
                    view_types::Stability::Dynamic,
                    None,
                );
                let incoming = attrs_with(
                    view_types::FileSize::Unknown,
                    view_types::Stability::Stable,
                    None,
                );
                RefreshCase {
                    name: "placeholder stability mismatch keeps learned exact",
                    existing: existing.clone(),
                    incoming,
                    expected: existing,
                }
            },
        ];

        for case in cases {
            let mut entry = file_entry(case.existing);

            refresh_file(&mut entry, &case.incoming);

            let refreshed = entry.attrs.as_ref().expect("file attrs survive refresh");
            assert_eq!(
                refreshed, &case.expected,
                "{}: refreshed attrs should match expected",
                case.name
            );
            assert_eq!(
                entry.size,
                case.expected.st_size(),
                "{}: inode size should follow refreshed attrs",
                case.name
            );
        }
    }
}
