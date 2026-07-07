//! The FUSE op boundary over the [`Namespace`](omnifs_engine::Namespace)
//! surface.
//!
//! Each method is the decision half of a fuser callback: it resolves through the
//! namespace, mints kernel inode identity, and returns the plain reply payload
//! (a `FileAttr` + TTL, a directory snapshot, or bytes). The thin fuser
//! callbacks in `filesystem.rs` marshal these into `Reply*` sinks; the in-crate
//! tests drive them directly. Whole-mode files buffer once per open; ranged
//! files read through per request; backing (resolved treeref) paths are served
//! from the local filesystem with `std::fs`.

use super::Frontend;
use super::common::{Body, DirSnapshot, NodeKind, ROOT_INO, TTL};
use super::errno::ns_error_errno;
use super::read_helpers::data_slice;
use fuser::{Errno, FileAttr, FopenFlags};
use omnifs_engine::{DirCursor, NodeId, NsEntryKind, NsError};
use std::path::{Path, PathBuf};
use std::time::Duration;

impl Frontend {
    /// Rebind an inode a listing bound as a plain provider directory to the
    /// resolved treeref subtree it turned out to be, so its children serve
    /// locally from `root`.
    fn rebind_subtree(&self, ino: u64, node: NodeId, root: PathBuf) {
        if let Some(mut entry) = self.inodes.get_mut(&ino)
            && !matches!(entry.body, Body::Subtree { .. })
        {
            entry.body = Body::Subtree { node, root };
            entry.kind = NodeKind::Directory;
        }
    }

    /// Resolve `name` under `parent_ino`, allocating (or reusing) the child
    /// inode, and return its kernel `FileAttr` + TTL.
    pub(crate) async fn do_lookup(
        &self,
        parent_ino: u64,
        name: &str,
    ) -> Result<(u64, FileAttr, Duration), Errno> {
        self.apply_pending_events();
        let inode = self.inodes.get(&parent_ino).ok_or(Errno::ENOENT)?;
        if inode.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        let body = inode.body.clone();
        drop(inode);

        if let Some(backing) = body.backing() {
            let child = backing.join(name);
            let meta = std::fs::symlink_metadata(&child).map_err(|_| Errno::ENOENT)?;
            let kind = self.backing_kind(&meta);
            let ino = self.intern_backing(parent_ino, name, child, kind);
            return Ok((ino, self.attr_from_metadata(ino, &meta), TTL));
        }

        let parent_node = body.node().ok_or(Errno::ENOENT)?;
        let _permit = self.acquire_op_permit().await;
        match self.namespace.lookup(parent_node, name).await {
            Ok(answer) => {
                let ino = self.bind_answer(parent_ino, name, answer.node, &answer.kind);
                self.apply_pending_events();
                let (attr, ttl) = self.ns_file_attr(ino, answer.node, &answer.attrs);
                Ok((ino, attr, ttl))
            },
            Err(NsError::NotFound) => Err(Errno::ENOENT),
            Err(error) => {
                self.apply_pending_events();
                Err(ns_error_errno(&error))
            },
        }
    }

    /// The current attributes of `ino`.
    pub(crate) async fn do_getattr(&self, ino: u64) -> Result<(FileAttr, Duration), Errno> {
        self.apply_pending_events();
        if ino == ROOT_INO {
            return Ok((self.dir_attr(ROOT_INO), TTL));
        }
        let inode = self.inodes.get(&ino).ok_or(Errno::ENOENT)?;
        let body = inode.body.clone();
        drop(inode);

        if let Some(backing) = body.backing() {
            let meta = std::fs::symlink_metadata(backing).map_err(|_| Errno::ENOENT)?;
            return Ok((self.attr_from_metadata(ino, &meta), TTL));
        }

        let node = body.node().ok_or(Errno::ENOENT)?;
        let _permit = self.acquire_op_permit().await;
        let attrs = match self.namespace.getattr(node).await {
            Ok(attrs) => attrs,
            Err(NsError::NotFound) => {
                self.apply_pending_events();
                return Err(Errno::ENOENT);
            },
            Err(error) => {
                self.apply_pending_events();
                return Err(ns_error_errno(&error));
            },
        };
        self.apply_pending_events();

        // A node that resolves to a treeref backing dir now serves locally.
        if let NsEntryKind::Subtree { root } = &attrs.kind {
            self.rebind_subtree(ino, node, root.clone());
            let meta = std::fs::symlink_metadata(root).map_err(|_| Errno::ENOENT)?;
            return Ok((self.attr_from_metadata(ino, &meta), TTL));
        }
        Ok(self.ns_file_attr(ino, node, &attrs))
    }

    /// Build the kernel directory snapshot for `ino` by draining every namespace
    /// page, allocating a child inode per entry.
    pub(crate) async fn do_opendir(&self, ino: u64) -> Result<DirSnapshot, Errno> {
        self.apply_pending_events();

        let node = if ino == ROOT_INO {
            NodeId::ROOT
        } else {
            let inode = self.inodes.get(&ino).ok_or(Errno::ENOENT)?;
            if inode.kind != NodeKind::Directory {
                return Err(Errno::ENOTDIR);
            }
            let body = inode.body.clone();
            drop(inode);
            if let Some(backing) = body.backing() {
                let backing = backing.clone();
                return self.snapshot_from_fs(ino, &backing);
            }
            body.node().ok_or(Errno::ENOENT)?
        };

        let _permit = self.acquire_op_permit().await;
        let mut snapshot = DirSnapshot::new();
        let mut cursor = DirCursor::start();
        loop {
            let page = self
                .namespace
                .readdir(node, cursor, 0)
                .await
                .map_err(|error| ns_error_errno(&error))?;
            for entry in page.entries {
                let kind = NodeKind::from(&entry.kind);
                // A listing never marks a treeref child; it binds as a plain
                // provider node and is promoted on the lookup that descends.
                let child_ino = self.intern_node(ino, &entry.name, entry.node, kind, None);
                snapshot.push((child_ino, entry.name, kind));
            }
            match page.next {
                Some(next) => cursor = next,
                None => break,
            }
        }
        self.apply_pending_events();
        Ok(snapshot)
    }

    /// Build a directory snapshot by reading a resolved treeref backing dir from
    /// the local filesystem.
    fn snapshot_from_fs(&self, parent_ino: u64, root: &Path) -> Result<DirSnapshot, Errno> {
        let mut snapshot = DirSnapshot::new();
        for entry in std::fs::read_dir(root).map_err(|_| Errno::EIO)? {
            let entry = entry.map_err(|_| Errno::EIO)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let child = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&child) else {
                continue;
            };
            let kind = self.backing_kind(&meta);
            let ino = self.intern_backing(parent_ino, name, child, kind);
            snapshot.push((ino, name.to_string(), kind));
        }
        Ok(snapshot)
    }

    /// Open `ino` on `fh`. A whole-mode file materializes once into the per-`fh`
    /// buffer; a ranged file binds the node for per-read read-through; a backing
    /// file opens lazily (`do_read` streams it from the filesystem). Returns the
    /// kernel open flags.
    pub(crate) async fn do_open(&self, ino: u64, fh: u64) -> Result<FopenFlags, Errno> {
        self.apply_pending_events();
        let inode = self.inodes.get(&ino).ok_or(Errno::ENOENT)?;
        let body = inode.body.clone();
        drop(inode);

        let node = match body {
            // A backing file is served from the real filesystem on demand.
            Body::Backing(_) => return Ok(FopenFlags::empty()),
            // A subtree root is a directory.
            Body::Subtree { .. } => return Err(Errno::EISDIR),
            Body::Node(node) => node,
        };

        let _permit = self.acquire_op_permit().await;
        let attrs = match self.namespace.getattr(node).await {
            Ok(attrs) => attrs,
            Err(NsError::NotFound) => return Err(Errno::ENOENT),
            Err(error) => return Err(ns_error_errno(&error)),
        };
        if matches!(
            attrs.kind,
            NsEntryKind::Directory | NsEntryKind::Subtree { .. }
        ) {
            return Err(Errno::EISDIR);
        }

        let flags = if attrs.direct_io {
            FopenFlags::FOPEN_DIRECT_IO
        } else {
            FopenFlags::empty()
        };
        match attrs.read_style {
            omnifs_engine::ReadStyle::Ranged => {
                self.ranged_fhs.insert(fh, node);
            },
            omnifs_engine::ReadStyle::Whole => {
                // The engine serves the whole payload; buffer it once so a
                // mutating control (`@next`) or an unversioned dynamic render
                // runs exactly once per open.
                let answer = self
                    .namespace
                    .read(node, 0, u32::MAX)
                    .await
                    .map_err(|error| ns_error_errno(&error))?;
                self.file_cache.insert(fh, answer.bytes);
            },
        }
        self.apply_pending_events();
        Ok(flags)
    }

    /// Serve `size` bytes at `offset` for `(ino, fh)`.
    pub(crate) async fn do_read(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, Errno> {
        // A ranged file reads through per request; the namespace dedups the
        // provider open behind its handle cache.
        if let Some(node) = self.ranged_fhs.get(&fh).map(|r| *r) {
            let _permit = self.acquire_op_permit().await;
            let answer = self
                .namespace
                .read(node, offset, size)
                .await
                .map_err(|error| ns_error_errno(&error))?;
            if matches!(answer.attrs.stability, omnifs_engine::StabilityClass::Live) {
                let mut grown = self.grown_sizes.entry(node).or_insert(0);
                *grown = (*grown).max(answer.attrs.size);
            }
            self.apply_pending_events();
            return Ok(answer.bytes);
        }

        // A whole file was buffered at open; slice from the buffer.
        if let Some(cached) = self.file_cache.get(&fh) {
            return Ok(data_slice(&cached, offset, size).to_vec());
        }

        // Lazily-opened bodies: a backing file streams from the filesystem, a
        // whole namespace file buffers on first read (defensive fallback).
        let inode = self.inodes.get(&ino).ok_or(Errno::ENOENT)?;
        let body = inode.body.clone();
        drop(inode);
        match body {
            Body::Backing(path) => {
                let data = std::fs::read(&path).map_err(|_| Errno::EIO)?;
                let slice = data_slice(&data, offset, size).to_vec();
                self.file_cache.insert(fh, data);
                Ok(slice)
            },
            Body::Subtree { .. } => Err(Errno::EISDIR),
            Body::Node(node) => {
                let _permit = self.acquire_op_permit().await;
                let answer = self
                    .namespace
                    .read(node, 0, u32::MAX)
                    .await
                    .map_err(|error| ns_error_errno(&error))?;
                let slice = data_slice(&answer.bytes, offset, size).to_vec();
                self.file_cache.insert(fh, answer.bytes);
                Ok(slice)
            },
        }
    }

    /// Read the link target of a backing symlink. The projection produces no
    /// symlinks of its own, so a namespace node is never a symlink.
    pub(crate) fn do_readlink(&self, ino: u64) -> Result<Vec<u8>, Errno> {
        let inode = self.inodes.get(&ino).ok_or(Errno::ENOENT)?;
        let Some(path) = inode.body.backing().cloned() else {
            return Err(Errno::EINVAL);
        };
        drop(inode);
        std::fs::read_link(path)
            .map(|target| target.as_os_str().as_encoded_bytes().to_vec())
            .map_err(|_| Errno::EIO)
    }

    /// Release a file handle: drop its per-`fh` protocol state. The namespace
    /// owns the ranged handle's lifecycle (its idle sweep closes it), so FUSE
    /// only clears its own buffers.
    pub(crate) fn do_release(&self, fh: u64) {
        self.file_cache.remove(&fh);
        self.ranged_fhs.remove(&fh);
    }

    /// Drop a directory snapshot bound to `fh`.
    pub(crate) fn do_releasedir(&self, fh: u64) {
        self.dir_snapshots.remove(&fh);
    }
}
