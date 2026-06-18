//! FUSE `lookup` op boundary: enter the async runtime once, delegate the
//! name-resolution DECISION to `Tree::resolve_child`, then mint the kernel
//! inode + `FileAttr` on the neutral `Node` it returns.

use super::Frontend;
use fuser::{Errno, FileAttr};
use omnifs_core::path::Path;
use omnifs_core::view::EntryMeta;
use omnifs_host::wit_protocol;
use omnifs_inspector::TraceId;
use omnifs_tree::{Backing, Node, RequestCtx};
use std::time::Duration;

impl Frontend {
    /// Resolve `name` under the provider directory `parent_path` and allocate
    /// (or refresh) the child's inode, returning the kernel `FileAttr` + TTL.
    ///
    /// Enters the async runtime exactly once (`block_on(Tree::resolve_child)`).
    /// `Tree` owns the cache-first lookup, the `@next`/`@all` control resolution
    /// (cache-only, `NotFound` once the feed exhausts), the mount-root
    /// ignore-file synthesis after a negative provider result, and the subtree
    /// handoff. The adapter only translates the returned `Node` into inode-table
    /// state: a subtree node binds a backing dir, a synthetic node sets the
    /// `synthetic` marker (so `open` serves it from a per-`fh` buffer), and a
    /// provider node clears any prior synthetic marker (a real `.gitignore` wins).
    pub(super) fn lookup_op(
        &self,
        mount_name: &str,
        parent_path: &Path,
        name: &str,
        trace: Option<TraceId>,
    ) -> Result<(FileAttr, Duration), Errno> {
        // Drive the kernel-side invalidation fan-out (notify + prune) before the
        // resolve. The renderer-neutral mem eviction happens inside
        // `Tree::drain_invalidations`; `Tree::resolve_child`'s own consult then
        // sees the post-eviction state.
        self.drain_and_evict_pending(mount_name);

        let parent = provider_dir_node(mount_name, parent_path);
        let ctx = RequestCtx { trace };
        let node = self
            .rt
            .block_on(self.tree.resolve_child(&parent, name, &ctx))
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
        match node.backing() {
            Backing::Subtree(dir) => {
                let ino = self.get_or_alloc_ino_backing(
                    mount_name,
                    child_path,
                    omnifs_wit::provider::types::EntryKind::Directory,
                    0,
                    dir.clone(),
                );
                (self.dir_attr(ino), super::common::TTL)
            },
            Backing::Provider => {
                let meta = node_meta(node);
                let kind = wit_protocol::entry_kind_to_wit(&meta.kind);
                let size = meta.st_size();
                let ttl = Self::ttl_for_meta(&meta);
                let ino = if node.is_synthetic() {
                    self.get_or_alloc_ino_synthetic(mount_name, child_path, meta)
                } else {
                    self.get_or_alloc_ino_meta_resolved(mount_name, child_path, meta)
                };
                (self.attr_for_inode_or_meta(ino, &kind, size), ttl)
            },
        }
    }
}

/// The `EntryMeta` a resolved `Node` projects (kind + optional attrs), the shape
/// the inode allocator and TTL/attr builders consume.
pub(super) fn node_meta(node: &Node) -> EntryMeta {
    EntryMeta {
        kind: node.kind(),
        attrs: node.attrs().cloned(),
    }
}

/// Build the minimal directory `Node` `Tree` needs to resolve a child or list a
/// directory: a provider-backed directory at (mount, path). The inode table has
/// already proved this path is a directory.
pub(super) fn provider_dir_node(mount_name: &str, path: &Path) -> Node {
    Node::new(
        mount_name.to_string(),
        path.clone(),
        EntryMeta::directory(),
        Backing::Provider,
    )
}
