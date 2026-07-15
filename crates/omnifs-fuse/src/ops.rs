//! The FUSE op boundary over the [`Namespace`](omnifs_engine::Namespace)
//! surface.
//!
//! Each method is the decision half of a fuser callback: it resolves through the
//! namespace, mints kernel inode identity, and returns the plain reply payload
//! (a `FileAttr` + TTL, a directory snapshot, or bytes). The thin fuser
//! callbacks in `filesystem.rs` marshal these into `Reply*` sinks; the in-crate
//! tests drive them directly. Whole-mode files buffer once per open; ranged
//! files read through per request.

use super::Frontend;
use super::common::{DirSnapshot, NodeKind, ROOT_INO, TTL};
use super::errno::ns_error_errno;
use super::read_helpers::data_slice;
use fuser::{Errno, FileAttr, FopenFlags};
use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, EntryKind};
use std::time::Duration;

impl Frontend {
    async fn settle<T>(&self, result: Result<T, Errno>) -> Result<T, Errno> {
        self.flush_events().await?;
        result
    }

    /// Resolve a FUSE inode's current namespace identity by walking its stable
    /// parent/name chain to the nearest hot ancestor and descending iteratively.
    /// The walk never reconstructs a path string, and all returned ids are from
    /// the currently attached daemon instance.
    pub(crate) async fn live_node(&self, ino: u64) -> Result<Path, Errno> {
        self.inodes
            .get(&ino)
            .map(|inode| inode.body.clone())
            .ok_or(Errno::ENOENT)
    }

    /// Resolve `name` under `parent_ino`, allocating (or reusing) the child
    /// inode, and return its kernel `FileAttr` + TTL.
    pub(crate) async fn do_lookup(
        &self,
        parent_ino: u64,
        name: &str,
    ) -> Result<(u64, FileAttr, Duration), Errno> {
        let inode = self.inodes.get(&parent_ino).ok_or(Errno::ENOENT)?;
        if inode.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        drop(inode);

        let parent_node = self.live_node(parent_ino).await?;
        let answer = self
            .settle(
                self.namespace
                    .lookup(parent_node, name)
                    .await
                    .map_err(|error| ns_error_errno(&error)),
            )
            .await?;
        let kind = NodeKind::from(&answer.attrs.kind);
        let ino = self.intern_node(parent_ino, name, answer.path.clone(), kind);
        let (attr, ttl) = self.ns_file_attr(ino, answer.path, &answer.attrs);
        Ok((ino, attr, ttl))
    }

    /// The current attributes of `ino`.
    pub(crate) async fn do_getattr(&self, ino: u64) -> Result<(FileAttr, Duration), Errno> {
        if ino == ROOT_INO {
            let attrs = omnifs_engine::Attrs {
                kind: EntryKind::Directory,
                dev: 0,
                ino: 0,
                size: 0,
                blocks: 0,
                mode: 0o555,
                nlink: 2,
                accessed: None,
                modified: None,
                created: None,
                ttl: TTL,
                change: 0,
                direct_io: false,
                stability: omnifs_engine::StabilityClass::Stable,
                read_style: omnifs_engine::ReadStyle::Whole,
            };
            return Ok(self.ns_file_attr(ROOT_INO, Path::root(), &attrs));
        }
        let node = self.live_node(ino).await?;
        let attrs = self
            .settle(
                self.namespace
                    .getattr(node.clone())
                    .await
                    .map_err(|error| ns_error_errno(&error)),
            )
            .await?;

        Ok(self.ns_file_attr(ino, node, &attrs))
    }

    /// Build the kernel directory snapshot for `ino` by draining every namespace
    /// page, allocating a child inode per entry.
    pub(crate) async fn do_opendir(&self, ino: u64) -> Result<DirSnapshot, Errno> {
        if ino != ROOT_INO {
            let inode = self.inodes.get(&ino).ok_or(Errno::ENOENT)?;
            if inode.kind != NodeKind::Directory {
                return Err(Errno::ENOTDIR);
            }
        }
        let node = self.live_node(ino).await?;
        let mut snapshot = DirSnapshot::new();
        let mut cursor = DirCursor::start();
        loop {
            let page = self
                .settle(
                    self.namespace
                        .readdir(node.clone(), cursor, 0)
                        .await
                        .map_err(|error| ns_error_errno(&error)),
                )
                .await?;
            for entry in page.entries {
                let kind = NodeKind::from(&entry.attrs.kind);
                let child_ino = self.intern_node(ino, &entry.name, entry.path, kind);
                snapshot.push((child_ino, entry.name, kind));
            }
            match page.next {
                Some(next) => cursor = next,
                None => break,
            }
        }
        Ok(snapshot)
    }

    /// Open `ino` on `fh`. A whole-mode file materializes once into the per-`fh`
    /// buffer; a ranged file binds the node for per-read read-through. Returns the
    /// kernel open flags.
    pub(crate) async fn do_open(&self, ino: u64, fh: u64) -> Result<FopenFlags, Errno> {
        let node = self.live_node(ino).await?;
        let attrs = self
            .settle(
                self.namespace
                    .getattr(node.clone())
                    .await
                    .map_err(|error| ns_error_errno(&error)),
            )
            .await?;
        if matches!(attrs.kind, EntryKind::Directory) {
            return Err(Errno::EISDIR);
        }

        let flags = if attrs.direct_io {
            FopenFlags::FOPEN_DIRECT_IO
        } else {
            FopenFlags::empty()
        };
        match attrs.read_style {
            omnifs_engine::ReadStyle::Ranged => {
                self.ranged_fhs.insert(fh, node.clone());
            },
            omnifs_engine::ReadStyle::Whole => {
                // The engine serves the whole payload; buffer it once so a
                // mutating control (`@next`) or an unversioned dynamic render
                // runs exactly once per open.
                let answer = self
                    .settle(
                        self.namespace
                            .read(node, 0, u32::MAX)
                            .await
                            .map_err(|error| ns_error_errno(&error)),
                    )
                    .await?;
                self.file_cache.insert(fh, answer.bytes);
            },
        }
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
        if let Some(node) = self.ranged_fhs.get(&fh).map(|r| r.clone()) {
            let answer = self
                .settle(
                    self.namespace
                        .read(node.clone(), offset, size)
                        .await
                        .map_err(|error| ns_error_errno(&error)),
                )
                .await?;
            if matches!(answer.attrs.stability, omnifs_engine::StabilityClass::Live) {
                let mut grown = self.grown_sizes.entry(node).or_insert(0);
                *grown = (*grown).max(answer.attrs.size);
            }
            return Ok(answer.bytes);
        }

        // A whole file was buffered at open; slice from the buffer.
        if let Some(cached) = self.file_cache.get(&fh) {
            return Ok(data_slice(&cached, offset, size).to_vec());
        }

        // Defensive fallback for a whole namespace file opened without cached
        // data.
        let body = self.live_node(ino).await?;
        let answer = self
            .settle(
                self.namespace
                    .read(body, 0, u32::MAX)
                    .await
                    .map_err(|error| ns_error_errno(&error)),
            )
            .await?;
        let slice = data_slice(&answer.bytes, offset, size).to_vec();
        self.file_cache.insert(fh, answer.bytes);
        Ok(slice)
    }

    /// Read the link target through the namespace facade.
    pub(crate) async fn do_readlink(&self, ino: u64) -> Result<Vec<u8>, Errno> {
        let node = self.live_node(ino).await?;
        let target = self
            .settle(
                self.namespace
                    .readlink(node)
                    .await
                    .map_err(|error| ns_error_errno(&error)),
            )
            .await?;
        Ok(target.as_os_str().as_encoded_bytes().to_vec())
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
