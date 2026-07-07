//! Inode allocation, identity interning, and `FileAttr` construction.
//!
//! FUSE owns kernel identity: it maps a stable namespace [`NodeId`] (or a
//! backing filesystem path for a resolved treeref subtree) to a kernel inode
//! number and builds `fuser::FileAttr` replies. It never inspects provider
//! bytes or the projection's caches; attributes come from the policied
//! [`omnifs_engine::Attrs`] or from a direct `std::fs` stat of a backing path.

use crate::Frontend;
use crate::common::{Body, Inode, NodeKind, ROOT_INO};
use fuser::{FileAttr, FileType, INodeNo};
use omnifs_engine::{Attrs, NodeId, NsEntryKind};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

// SAFETY: getuid(2) takes no arguments, reads only kernel-maintained process
// state, and is always safe to call on any POSIX system.
#[allow(unsafe_code)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

// SAFETY: getgid(2) takes no arguments, reads only kernel-maintained process
// state, and is always safe to call on any POSIX system.
#[allow(unsafe_code)]
fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

impl Frontend {
    pub(crate) fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    /// Allocate (or reuse) the kernel inode for a resolved namespace node,
    /// preserving a resolved subtree backing over a later plain provider
    /// resolution of the same node. Mirrors the NFS `intern_node`, minus the
    /// export-root scope (FUSE has a single root).
    pub(crate) fn intern_node(
        &self,
        parent: u64,
        name: &str,
        node: NodeId,
        kind: NodeKind,
        subtree_root: Option<PathBuf>,
    ) -> u64 {
        let ino = *self.by_node.entry(node).or_insert_with(|| self.alloc_ino());
        if ino == ROOT_INO {
            return ino;
        }
        let existing = self.inodes.get(&ino).map(|entry| entry.body.clone());
        let body = match (subtree_root, existing) {
            (Some(root), _) => Body::Subtree { node, root },
            // A listing re-binds a treeref child as a plain provider directory;
            // keep the subtree backing a prior lookup resolved.
            (None, Some(Body::Subtree { node: kept, root })) => Body::Subtree { node: kept, root },
            (None, _) => Body::Node(node),
        };
        self.inodes.insert(
            ino,
            Inode {
                parent,
                name: name.to_string(),
                kind,
                body,
            },
        );
        ino
    }

    /// Allocate (or reuse) the kernel inode for a subtree-local backing path.
    pub(crate) fn intern_backing(
        &self,
        parent: u64,
        name: &str,
        path: PathBuf,
        kind: NodeKind,
    ) -> u64 {
        let ino = *self
            .by_backing
            .entry(path.clone())
            .or_insert_with(|| self.alloc_ino());
        self.inodes.insert(
            ino,
            Inode {
                parent,
                name: name.to_string(),
                kind,
                body: Body::Backing(path),
            },
        );
        ino
    }

    /// Bind a resolved namespace answer to an inode, recording a subtree backing
    /// when the namespace reports the node is a resolved treeref.
    pub(crate) fn bind_answer(
        &self,
        parent: u64,
        name: &str,
        node: NodeId,
        kind: &NsEntryKind,
    ) -> u64 {
        let subtree_root = match kind {
            NsEntryKind::Subtree { root } => Some(root.clone()),
            _ => None,
        };
        self.intern_node(parent, name, node, NodeKind::from(kind), subtree_root)
    }

    /// Build a kernel `FileAttr` from the policied namespace [`Attrs`], folding
    /// in a live-follow grown size so a polling `tail -f` re-stats to the latest
    /// end. The TTL is engine-decided; FUSE copies it through.
    pub(crate) fn ns_file_attr(
        &self,
        ino: u64,
        node: NodeId,
        attrs: &Attrs,
    ) -> (FileAttr, Duration) {
        let attr = match attrs.kind {
            NsEntryKind::Directory | NsEntryKind::Subtree { .. } => self.dir_attr(ino),
            NsEntryKind::File => {
                let grown = self.grown_sizes.get(&node).map_or(0, |g| *g);
                self.file_attr(ino, attrs.size.max(grown))
            },
            NsEntryKind::Symlink => self.symlink_attr(ino, attrs.size),
        };
        (attr, attrs.ttl)
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

    #[allow(clippy::unused_self)]
    pub(crate) fn symlink_attr(&self, ino: u64, size: u64) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::Symlink,
            perm: 0o777,
            nlink: 1,
            uid: current_uid(),
            gid: current_gid(),
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    /// Build a `FileAttr` from a backing path's `std::fs` metadata. The subtree
    /// children of a resolved treeref are pure local files; the byte boundary
    /// keeps their real metadata off the provider surface.
    #[allow(clippy::unused_self)]
    pub(crate) fn attr_from_metadata(&self, ino: u64, meta: &std::fs::Metadata) -> FileAttr {
        use std::os::unix::fs::MetadataExt;
        let file_type = meta.file_type();
        let kind = if file_type.is_dir() {
            FileType::Directory
        } else if file_type.is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };
        let now = SystemTime::now();
        FileAttr {
            ino: INodeNo(ino),
            size: meta.len(),
            blocks: meta.blocks(),
            atime: meta.accessed().unwrap_or(now),
            mtime: meta.modified().unwrap_or(now),
            ctime: meta.modified().unwrap_or(now),
            crtime: meta.created().unwrap_or(now),
            kind,
            perm: u16::try_from(meta.mode() & 0o7777).unwrap_or(0o444),
            nlink: u32::try_from(meta.nlink()).unwrap_or(1),
            uid: current_uid(),
            gid: current_gid(),
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    /// The kernel `NodeKind` of a backing path, from a `std::fs` stat.
    #[allow(clippy::unused_self)]
    pub(crate) fn backing_kind(&self, meta: &std::fs::Metadata) -> NodeKind {
        let file_type = meta.file_type();
        if file_type.is_dir() {
            NodeKind::Directory
        } else if file_type.is_symlink() {
            NodeKind::Symlink
        } else {
            NodeKind::File
        }
    }
}
