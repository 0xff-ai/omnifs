//! The `Tree::resolve` body.

use omnifs_core::path::Path;
use omnifs_core::view::EntryMeta;
use omnifs_host::LookupOutcome;

use crate::error::{Result, TreeError};
use crate::node::{Backing, Node};
use crate::{RequestCtx, Tree};

impl Tree {
    /// Resolve a full protocol path to a `Node`. Cache-first (the negative
    /// short-circuit lives inside `Namespace::lookup_child`), then async
    /// `Namespace::lookup_child` (already coalesced + materialized). Subtree
    /// outcomes resolve through `Runtime::resolve_tree_ref` into
    /// `Backing::Subtree`. Doubles as filehandle/inode rehydration: a renderer
    /// persisted (mount, path) in its handle and calls resolve again to rebuild
    /// a `Node` after eviction, without re-walking from root.
    pub async fn resolve(&self, path: &Path, ctx: &RequestCtx) -> Result<Node> {
        let (mount, rel) = self.split_mount_path(path)?;
        let runtime = self.runtime_for(&mount)?;

        // The mount root is always a directory; no provider round trip needed.
        if rel.is_root() {
            return Ok(Node::new(
                mount,
                rel,
                EntryMeta::directory(),
                Backing::Provider,
            ));
        }

        let Some((parent, name)) = rel.parent_and_name() else {
            return Err(TreeError::invalid_input(format!(
                "resolve: path has no parent: {}",
                rel.as_str()
            )));
        };

        match runtime
            .namespace()
            .lookup_child(parent.as_str(), name, ctx.trace)
            .await?
        {
            LookupOutcome::Entry(entry) => Ok(Node::new(
                mount,
                entry.path().clone(),
                entry.meta().clone(),
                Backing::Provider,
            )),
            LookupOutcome::Subtree(tref) => {
                let dir = runtime
                    .resolve_tree_ref(tref)
                    .ok_or_else(|| TreeError::internal(format!("unresolved tree_ref {tref}")))?;
                Ok(Node::new(
                    mount,
                    rel,
                    EntryMeta::directory(),
                    Backing::Subtree(dir),
                ))
            },
            LookupOutcome::NotFound => Err(TreeError::not_found(path.as_str())),
        }
    }
}
