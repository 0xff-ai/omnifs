//! Inode allocation, identity interning, and `FileAttr` construction.
//!
//! FUSE owns kernel identity: it maps a stable namespace path to a kernel
//! inode number and builds `fuser::FileAttr` replies from namespace attributes.

use crate::Frontend;
use crate::common::{Inode, NodeKind, ROOT_INO};
use fuser::{FileAttr, INodeNo};
use omnifs_core::path::Path;
use omnifs_engine::Attrs;
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

    /// Allocate (or reuse) the kernel inode for a resolved namespace node.
    pub(crate) fn intern_node(&self, parent: u64, name: &str, node: Path, kind: NodeKind) -> u64 {
        let ino = *self
            .by_node
            .entry(node.clone())
            .or_insert_with(|| self.alloc_ino());
        if ino == ROOT_INO {
            return ino;
        }
        self.inodes.insert(
            ino,
            Inode {
                parent,
                name: name.to_string(),
                kind,
                body: node,
            },
        );
        ino
    }

    /// Build a kernel `FileAttr` from the policied namespace [`Attrs`], folding
    /// in a live-follow grown size so a polling `tail -f` re-stats to the latest
    /// end. The TTL is engine-decided; FUSE copies it through.
    pub(crate) fn ns_file_attr(&self, ino: u64, node: Path, attrs: &Attrs) -> (FileAttr, Duration) {
        let grown = self.grown_sizes.get(&node).map_or(0, |g| *g);
        let size = attrs.size.max(grown);
        (
            NodeKind::from(&attrs.kind).attr(ino, attrs, size),
            attrs.ttl,
        )
    }
}

impl NodeKind {
    pub(crate) fn attr(self, ino: u64, attrs: &Attrs, size: u64) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: attrs.blocks,
            atime: attrs.accessed.map(millis_to_system_time).unwrap_or(now),
            mtime: attrs.modified.map(millis_to_system_time).unwrap_or(now),
            ctime: attrs.modified.map(millis_to_system_time).unwrap_or(now),
            crtime: attrs.created.map(millis_to_system_time).unwrap_or(now),
            kind: self.file_type(),
            perm: attrs.mode,
            nlink: attrs.nlink,
            uid: current_uid(),
            gid: current_gid(),
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }
}

fn millis_to_system_time(millis: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(millis)
}
