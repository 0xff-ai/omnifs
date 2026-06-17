//! The `Tree::resolve` body.

use omnifs_core::path::Path;
use omnifs_core::view::EntryMeta;
use omnifs_host::LookupOutcome;
use omnifs_host::pagination::is_control_name;

use crate::error::{Result, TreeError};
use crate::node::{Backing, Node};
use crate::synthetic;
use crate::{RequestCtx, Tree};

impl Tree {
    /// Resolve a full protocol path to a `Node`. Cache-first (the negative
    /// short-circuit lives inside `Namespace::lookup_child`), then async
    /// `Namespace::lookup_child` (already coalesced + materialized). Owns the
    /// synthetic-entry resolution FUSE otherwise carries in `lookup_check_caches`
    /// and `synthesize_root_ignore_lookup`: a `@next`/`@all` pagination control
    /// resolves ONLY from the parent's cached dirents (absent => NotFound, never
    /// a provider round trip), and a mount-root ignore file is synthesized ONLY
    /// after a negative provider result (a real provider `.gitignore` wins).
    /// Subtree outcomes resolve through `Runtime::resolve_tree_ref` into
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

        // A pagination control (`@next`/`@all`) resolves ONLY from the parent's
        // cached dirents (a resume cursor remains). A reserved control name is
        // never a real provider entry, so once the control is gone (feed
        // exhausted) the lookup is NotFound; we never consult the provider for it.
        if is_control_name(name) {
            return match synthetic::resolve_synthetic_child(&runtime, &parent, name, false) {
                Some((meta, syn)) => Ok(Node::synthetic(mount, rel, meta, syn)),
                None => Err(TreeError::not_found(path.as_str())),
            };
        }

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
            // The provider has no such child: synthesize a mount-root ignore file
            // only now, never shadowing a real one (the provider was consulted
            // and returned negative). Otherwise surface NotFound.
            LookupOutcome::NotFound => {
                match synthetic::resolve_synthetic_child(&runtime, &parent, name, false) {
                    Some((meta, syn)) => Ok(Node::synthetic(mount, rel, meta, syn)),
                    None => Err(TreeError::not_found(path.as_str())),
                }
            },
        }
    }
}
