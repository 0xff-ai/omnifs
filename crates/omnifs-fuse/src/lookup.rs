//! FUSE `lookup` op boundary: delegate the name-resolution DECISION to
//! `Tree::resolve_child`, then mint the kernel inode + `FileAttr` on the neutral
//! `Node` it returns.

use super::Frontend;
use fuser::{Errno, FileAttr};
use omnifs_api::events::TraceId;
use omnifs_core::path::Path;
use omnifs_core::view::EntryKind;
use omnifs_tree::{Node, RequestCtx};
use std::time::Duration;

impl Frontend {
    /// Resolve `name` under the provider directory `parent_path` and allocate
    /// (or refresh) the child's inode, returning the kernel `FileAttr` + TTL.
    ///
    /// `Tree` owns the cache-first lookup, the `@next`/`@all` control resolution
    /// (cache-only, `NotFound` once the feed exhausts), the mount-root
    /// ignore-file synthesis after a negative provider result, and the subtree
    /// handoff. The adapter only translates the returned `Node` into inode-table
    /// state: a subtree node binds a backing dir, a synthetic node sets the
    /// `synthetic` marker (so `open` serves it from a per-`fh` buffer), and a
    /// provider node clears any prior synthetic marker (a real `.gitignore` wins).
    pub(super) async fn lookup_op(
        &self,
        mount_name: &str,
        parent_path: &Path,
        name: &str,
        trace: Option<TraceId>,
    ) -> Result<(FileAttr, Duration), Errno> {
        let _permit = self.acquire_op_permit().await;

        // Drive the kernel-side invalidation fan-out (notify + prune) before the
        // resolve. The renderer-neutral mem eviction happens inside
        // `Tree::drain_invalidations`; `Tree::resolve_child`'s own consult then
        // sees the post-eviction state.
        self.drain_and_evict_pending(mount_name);

        let parent = Node::provider_dir(mount_name.to_string(), parent_path.clone());
        let ctx = RequestCtx { trace };
        let node = self
            .tree
            .resolve_child(&parent, name, &ctx)
            .await
            .map_err(|e| super::errno::tree_error_errno(&e))?;
        Ok(self.inode_attr_for_node(mount_name, &node))
    }

    /// Allocate (or refresh) the inode for a resolved child `Node` and return
    /// its kernel `FileAttr` + TTL. A subtree node binds the backing dir; a
    /// synthetic node sets the `synthetic` marker; a provider node clears it.
    pub(super) fn inode_attr_for_node(
        &self,
        mount_name: &str,
        node: &Node,
    ) -> (FileAttr, Duration) {
        let child_path = node.path();
        if let Some(dir) = node.subtree_path() {
            let ino = self.get_or_alloc_ino_backing(
                mount_name,
                child_path,
                EntryKind::Directory,
                0,
                dir.clone(),
            );
            return (self.dir_attr(ino), super::common::TTL);
        }

        let meta = node.projected_meta();
        let kind = meta.kind();
        let size = meta.st_size();
        let ttl = Self::ttl_for_meta(&meta);
        let ino = if node.is_synthetic() {
            self.get_or_alloc_ino_synthetic(mount_name, child_path, meta)
        } else {
            self.get_or_alloc_ino_meta_resolved(mount_name, child_path, meta)
        };
        (self.attr_for_inode_or_meta(ino, kind, size), ttl)
    }
}
