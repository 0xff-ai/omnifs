//! FUSE `opendir` op boundary: delegate the listing DECISION to `Tree::list`,
//! then mint the kernel directory snapshot (inode-allocated) on the neutral
//! `Listing` it returns. Also owns the backing-filesystem snapshot for resolved
//! treeref/clone/archive directories, which the renderer reads directly with no
//! provider round trip.

use super::Frontend;
use super::common::{DirSnapshot, InodeBody};
use fuser::Errno;
use omnifs_api::events::TraceId;
use omnifs_core::path::Path;
use omnifs_core::view::EntryKind;
use omnifs_tree::{EntryOrigin, ListOutcome, Listing, Node, RequestCtx};
use std::path::Path as StdPath;

impl Frontend {
    /// Build a directory snapshot by reading the real filesystem (a resolved
    /// treeref/clone/archive backing dir).
    pub(super) fn snapshot_from_fs(
        &self,
        mount_name: &str,
        path: &Path,
        rp: &StdPath,
    ) -> Result<DirSnapshot, Errno> {
        let read_dir = std::fs::read_dir(rp).map_err(|_| Errno::EIO)?;
        let mut snapshot = Vec::new();
        for dir_entry in read_dir.flatten() {
            let fname = dir_entry.file_name();
            let Some(fname_str) = fname.to_str() else {
                continue;
            };
            let child_rp = dir_entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&child_rp) else {
                continue;
            };
            let kind = if meta.is_dir() {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            let child_path = path
                .join(fname_str)
                .expect("backing directory entry must be a valid path segment");
            let child_ino =
                self.get_or_alloc_ino_backing(mount_name, &child_path, kind, meta.len(), child_rp);
            snapshot.push((child_ino, fname_str.to_string(), kind));
        }
        Ok(snapshot)
    }

    /// List the provider directory at `(mount, path)` and build its kernel
    /// directory snapshot.
    ///
    /// `Tree` owns the cache consult+populate, the
    /// serve-cached/`unchanged`/serve-stale paths, the reserved-`@` drop, and the
    /// host-synthesized `@next`/`@all` controls + mount-root ignore files
    /// (returned as synthetic entry origins).
    /// The adapter only allocates inodes: provider entries clear any prior
    /// synthetic marker, synthetic entries set it. A `Subtree` outcome binds the
    /// backing dir on `ino` and snapshots it from the real filesystem.
    pub(super) async fn opendir_op(
        &self,
        mount_name: &str,
        ino: u64,
        path: &Path,
        trace: Option<TraceId>,
    ) -> Result<DirSnapshot, Errno> {
        let _permit = self.acquire_op_permit().await;

        // Drive the kernel-side invalidation fan-out (notify + prune) before the
        // list. The mem eviction happens inside `Tree::drain_invalidations`;
        // `Tree::list`'s own cache consult then sees the post-eviction state.
        self.drain_and_evict_pending(mount_name);

        let node = Node::provider_dir(mount_name.to_string(), path.clone());
        let ctx = RequestCtx { trace };
        match self
            .tree
            .list(&node, None, &ctx)
            .await
            .map_err(|e| super::errno::tree_error_errno(&e))?
        {
            ListOutcome::Listing(listing) => {
                Ok(self.snapshot_from_listing(mount_name, path, &listing))
            },
            ListOutcome::Subtree(dir) => {
                if let Some(mut entry) = self.inodes.get_mut(&ino)
                    && !entry.body.is_backing()
                {
                    entry.body = InodeBody::Backing(dir.clone());
                }
                self.snapshot_from_fs(mount_name, path, &dir)
            },
        }
    }

    /// Materialize a kernel directory snapshot from a neutral `Listing`.
    pub(crate) fn snapshot_from_listing(
        &self,
        mount_name: &str,
        path: &Path,
        listing: &Listing,
    ) -> DirSnapshot {
        let mut snapshot = Vec::with_capacity(listing.entries.len());
        for entry in &listing.entries {
            let (entry_mount, child_path) = if mount_name.is_empty() && path.is_root() {
                (entry.name.as_str(), Path::root())
            } else {
                (
                    mount_name,
                    path.join(&entry.name)
                        .expect("listing entry must be a valid path segment"),
                )
            };
            let kind = entry.meta.kind();
            let ino = match &entry.origin {
                EntryOrigin::Provider => self.get_or_alloc_ino_meta_resolved(
                    entry_mount,
                    &child_path,
                    entry.meta.clone(),
                ),
                EntryOrigin::Synthetic(_) => {
                    self.get_or_alloc_ino_synthetic(mount_name, &child_path, entry.meta.clone())
                },
            };
            snapshot.push((ino, entry.name.clone(), kind));
        }
        snapshot
    }
}
