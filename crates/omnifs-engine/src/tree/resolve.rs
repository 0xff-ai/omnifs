//! The `Tree::resolve` / `Tree::resolve_child` bodies.

use std::sync::Arc;

use crate::Runtime;
use crate::cache::{Record, RecordKind};
use crate::effect_apply::LookupOutcome;
use crate::view::{DirentsPayload, EntryMeta};
use omnifs_core::path::Path;

use super::error::{Result, TreeError};
use super::node::{Node, NodeBody};
use super::synthetic;
use crate::{RequestCtx, Tree};

impl Tree {
    /// Resolve a full protocol path to a `Node`. Splits the path into
    /// (mount, mount-relative path); the mount root resolves to a directory
    /// without a provider round trip, and any deeper path resolves its leaf via
    /// [`resolve_child`](Self::resolve_child) against the parent directory.
    /// Doubles as filehandle/inode rehydration: a renderer persisted (mount,
    /// path) in its handle and calls resolve again to rebuild a `Node` after
    /// eviction, without re-walking from root.
    pub async fn resolve(&self, path: &Path, ctx: &RequestCtx) -> Result<Node> {
        let (mount, rel) = self.ctx.split_mount_path(path)?;

        // The mount root is always a directory; no provider round trip needed.
        if rel.is_root() {
            return Ok(Node::new(
                mount,
                rel,
                EntryMeta::directory(),
                NodeBody::Provider,
            ));
        }

        let runtime = self.ctx.runtime_for(&mount)?;
        let Some((parent, name)) = rel.parent_and_name() else {
            return Err(TreeError::invalid_input(format!(
                "resolve: path has no parent: {}",
                rel.as_str()
            )));
        };

        self.resolve_child_in(mount, &runtime, &parent, name, ctx)
            .await
    }

    /// Resolve a child of an already-resolved parent directory `Node` to a
    /// `Node`. This is the renderer-neutral name oracle FUSE drives from
    /// `lookup` and NFS from `LOOKUP`: both hold a parent handle plus a leaf
    /// name, so neither has to reconstruct (and re-split) a full protocol path.
    /// `parent` must be a provider-backed directory (a treeref subtree child is
    /// resolved by the renderer through `NodeBody::Subtree`, not here).
    ///
    /// Owns the synthetic-entry resolution FUSE otherwise carries in
    /// `lookup_check_caches` and `synthesize_root_ignore_lookup`: a
    /// `@next`/`@all` pagination control resolves ONLY from the parent's cached
    /// dirents (absent => NotFound, never a provider round trip), and a
    /// mount-root ignore file is synthesized ONLY after a negative provider
    /// result (a real provider `.gitignore` wins). Subtree outcomes resolve
    /// through `Runtime::resolve_tree_ref` into `NodeBody::Subtree`.
    pub async fn resolve_child(&self, parent: &Node, name: &str, ctx: &RequestCtx) -> Result<Node> {
        let runtime = self.ctx.runtime_for(parent.mount())?;
        self.resolve_child_in(
            parent.mount().to_string(),
            &runtime,
            parent.path(),
            name,
            ctx,
        )
        .await
    }

    /// Shared body for [`resolve`](Self::resolve) and
    /// [`resolve_child`](Self::resolve_child): resolve `name` under `parent`
    /// (a mount-relative directory path) on the runtime serving `mount`.
    async fn resolve_child_in(
        &self,
        mount: String,
        runtime: &Arc<Runtime>,
        parent: &Path,
        name: &str,
        ctx: &RequestCtx,
    ) -> Result<Node> {
        let rel = parent.join(name).map_err(|e| {
            TreeError::invalid_input(format!("resolve_child: invalid name {name:?}: {e}"))
        })?;

        if self.ctx.is_mount_enumeration_root(&mount, parent)
            && self
                .ctx
                .mount_names()
                .is_some_and(|mounts| mounts.iter().any(|m| m == name))
        {
            return Ok(Node::new(
                name.to_string(),
                Path::root(),
                EntryMeta::directory(),
                NodeBody::Provider,
            ));
        }

        // A pagination control (`@next`/`@all`) resolves ONLY from the parent's
        // cached dirents (a resume cursor remains). A reserved control name is
        // never a real provider entry, so once the control is gone (feed
        // exhausted) the lookup is NotFound; we never consult the provider for it.
        if synthetic::is_control_name(name) {
            return match synthetic::resolve_synthetic_child(runtime, parent, name, false) {
                Some((meta, syn)) => Ok(Node::synthetic(mount, rel, meta, syn)),
                None => Err(TreeError::not_found(rel.as_str())),
            };
        }

        if let Some(meta) = cached_dirent_child(runtime, parent, name) {
            return Ok(Node::new(mount, rel, meta, NodeBody::Provider));
        }

        match runtime
            .namespace()
            .lookup_child(parent, name, ctx.trace)
            .await?
        {
            LookupOutcome::Entry(entry) => Ok(Node::new(
                mount,
                entry.path().clone(),
                entry.meta().clone(),
                NodeBody::Provider,
            )),
            LookupOutcome::Subtree(tref) => {
                let dir = runtime
                    .resolve_tree_ref(tref)
                    .ok_or_else(|| TreeError::internal(format!("unresolved tree_ref {tref}")))?;
                Ok(Node::new(
                    mount,
                    rel,
                    EntryMeta::directory(),
                    NodeBody::Subtree(dir),
                ))
            },
            // The provider has no such child: synthesize a mount-root ignore file
            // only now, never shadowing a real one (the provider was consulted
            // and returned negative). Otherwise surface NotFound.
            LookupOutcome::NotFound => {
                match synthetic::resolve_synthetic_child(runtime, parent, name, false) {
                    Some((meta, syn)) => Ok(Node::synthetic(mount, rel, meta, syn)),
                    None => Err(TreeError::not_found(rel.as_str())),
                }
            },
        }
    }
}

fn cached_dirent_child(runtime: &Runtime, parent: &Path, name: &str) -> Option<EntryMeta> {
    runtime
        .cache()
        .mem_get(parent, RecordKind::Dirents, None)
        .as_ref()
        .and_then(|record| dirent_child(record, name))
        .or_else(|| {
            runtime
                .cache()
                .cache_get(parent, RecordKind::Dirents, None)
                .as_ref()
                .and_then(|record| dirent_child(record, name))
        })
}

fn dirent_child(record: &Record, name: &str) -> Option<EntryMeta> {
    let dirents = DirentsPayload::deserialize(&record.payload)?;
    dirents
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.meta.clone())
}
