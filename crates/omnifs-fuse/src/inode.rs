//! Node allocation and attribute generation for FUSE.
//!
//! Manages the mapping from virtual paths to inode numbers with
//! deduplication and stale entry updates.

use crate::Frontend;
use crate::common::InodeBody;
use fuser::{FileAttr, FileType, INodeNo};
use omnifs_core::path::Path;
use omnifs_engine::render::PathKey;
use omnifs_engine::view::{EntryKind, EntryMeta, FileAttrsCache};
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
    pub(crate) kind: EntryKind,
    pub(crate) attrs: Option<FileAttrsCache>,
    pub(crate) size: u64,
    pub(crate) body: InodeBody,
}

impl NodeEntry {
    pub(crate) fn meta(&self) -> EntryMeta {
        match self.kind {
            EntryKind::Directory => EntryMeta::directory(),
            EntryKind::File => match self.attrs.clone() {
                Some(attrs) => EntryMeta::file(attrs),
                None => EntryMeta::file_without_attrs(),
            },
        }
    }

    fn refresh(
        &mut self,
        incoming_kind: EntryKind,
        incoming_attrs: Option<&FileAttrsCache>,
        fallback_size: u64,
    ) {
        let attrs = self.refreshed_attrs(incoming_kind, incoming_attrs);
        let size = attrs
            .as_ref()
            .map_or(fallback_size, FileAttrsCache::st_size);

        self.kind = incoming_kind;
        self.attrs = attrs;
        self.size = size;
    }

    fn refreshed_attrs(
        &self,
        incoming_kind: EntryKind,
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
            // we already hold for a file. Subtree files re-`stat` at getattr,
            // so a stale value here never reaches them.
            (Some(existing), None)
                if matches!(
                    (self.kind, incoming_kind),
                    (EntryKind::File, EntryKind::File)
                ) =>
            {
                Some(existing.clone())
            },
            (_, incoming) => incoming.cloned(),
        }
    }
}

/// What the caller knows about an inode allocation, which drives how the inode
/// body is updated on an existing node.
///
/// A genuine resolution sets the concrete body. The test-only refresh path
/// leaves the existing body untouched, proving a cache-driven re-touch does not
/// silently demote a still-synthetic node.
pub(crate) enum NodeOrigin {
    /// A cache-driven refresh (cached dirents/control replay) that does not
    /// assert the node's origin. Leaves the body unchanged on an existing inode;
    /// defaults to provider on first insert.
    #[cfg(test)]
    Refresh,
    /// A genuine provider resolution proved a real node at this path.
    Provider,
    /// Passthrough to a real backing-filesystem file.
    Subtree(PathBuf),
    /// Host-synthesized node (a mount-root ignore file).
    Synthetic,
}

/// How an inode refresh updates the body of an existing node.
enum BodyUpdate {
    Set(InodeBody),
    #[cfg(test)]
    Keep,
}

impl NodeOrigin {
    fn body_on_insert(&self) -> InodeBody {
        match self {
            #[cfg(test)]
            NodeOrigin::Refresh => InodeBody::Provider,
            NodeOrigin::Provider => InodeBody::Provider,
            NodeOrigin::Subtree(path) => InodeBody::Subtree(path.clone()),
            NodeOrigin::Synthetic => InodeBody::Synthetic,
        }
    }

    fn body_update(&self) -> BodyUpdate {
        match self {
            #[cfg(test)]
            NodeOrigin::Refresh => BodyUpdate::Keep,
            NodeOrigin::Provider => BodyUpdate::Set(InodeBody::Provider),
            NodeOrigin::Subtree(path) => BodyUpdate::Set(InodeBody::Subtree(path.clone())),
            NodeOrigin::Synthetic => BodyUpdate::Set(InodeBody::Synthetic),
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

    #[cfg(test)]
    pub(crate) fn get_or_alloc_ino(
        &self,
        mount_name: &str,
        path: &Path,
        kind: EntryKind,
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
        let kind = meta.kind();
        let attrs = meta.into_attrs();
        self.get_or_alloc_ino_inner(mount_name, path, kind, attrs, size, &NodeOrigin::Refresh)
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
        let kind = meta.kind();
        let attrs = meta.into_attrs();
        self.get_or_alloc_ino_inner(mount_name, path, kind, attrs, size, &NodeOrigin::Provider)
    }

    pub(crate) fn get_or_alloc_ino_backing(
        &self,
        mount_name: &str,
        path: &Path,
        kind: EntryKind,
        size: u64,
        backing_path: PathBuf,
    ) -> u64 {
        self.get_or_alloc_ino_inner(
            mount_name,
            path,
            kind,
            None,
            size,
            &NodeOrigin::Subtree(backing_path),
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
        let kind = meta.kind();
        let attrs = meta.into_attrs();
        self.get_or_alloc_ino_inner(mount_name, path, kind, attrs, size, &NodeOrigin::Synthetic)
    }

    fn get_or_alloc_ino_inner(
        &self,
        mount_name: &str,
        path: &Path,
        kind: EntryKind,
        attrs: Option<FileAttrsCache>,
        size: u64,
        origin: &NodeOrigin,
    ) -> u64 {
        let key = PathKey::with_mount_str(mount_name, path.clone()).expect("runtime mount name");
        // Use entry API to atomically check-or-insert, avoiding a race where
        // two concurrent lookups for the same (mount, path) allocate different inodes.
        // Use and_modify to update kind/size on existing entries (stale inode fix).
        let incoming_attrs = attrs;
        let body_on_insert = origin.body_on_insert();
        let body_update = origin.body_update();
        *self
            .path_to_inode
            .entry(key)
            .and_modify(|existing_ino| {
                if let Some(mut entry) = self.inodes.get_mut(existing_ino) {
                    entry.refresh(kind, incoming_attrs.as_ref(), size);
                    // A genuine resolution overrides the prior body, so a
                    // `.gitignore` that later appears in the provider stops
                    // being host-synthesized. An origin-agnostic refresh must
                    // not demote a still-synthetic node.
                    match &body_update {
                        BodyUpdate::Set(body) => entry.body = body.clone(),
                        #[cfg(test)]
                        BodyUpdate::Keep => {},
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
                        body: body_on_insert.clone(),
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
    use omnifs_engine::view as view_types;

    fn attrs(size: view_types::FileSize, version_token: Option<&str>) -> FileAttrsCache {
        attrs_with(size, view_types::Stability::Dynamic, version_token)
    }

    fn attrs_with(
        size: view_types::FileSize,
        stability: view_types::Stability,
        version_token: Option<&str>,
    ) -> FileAttrsCache {
        FileAttrsCache::deferred(
            size,
            view_types::ReadMode::Full,
            stability,
            version_token.map(str::to_string),
        )
        .expect("test attrs are valid")
    }

    fn file_entry(attrs: FileAttrsCache) -> NodeEntry {
        let size = attrs.st_size();
        NodeEntry {
            mount_name: "test".to_string(),
            path: Path::parse("/hello/fresh-full").unwrap(),
            kind: EntryKind::File,
            attrs: Some(attrs),
            size,
            body: InodeBody::Provider,
        }
    }

    fn refresh_file(entry: &mut NodeEntry, incoming: &FileAttrsCache) {
        entry.refresh(EntryKind::File, Some(incoming), incoming.st_size());
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
