//! Internal provider path resolution and child traversal.

use std::sync::Arc;

use crate::Runtime;
use crate::cache::RecordKind;
use crate::effect_apply::LookupOutcome;
use crate::view::{DirentsPayload, EntryMeta};
use omnifs_api::events::CacheKind;
use omnifs_core::path::Path;

use super::error::{Result, TreeError};
use super::node::{Node, NodeBody};
use super::synthetic;
use crate::{RequestCtx, TreeNamespace};

impl TreeNamespace {
    /// Resolve a full protocol path to a `Node`. Splits the path into
    /// (mount, mount-relative path); the mount root resolves to a directory
    /// without a provider round trip, and any deeper path resolves its leaf via
    /// [`resolve_child`](Self::resolve_child) against the parent directory.
    /// Doubles as path rehydration: a frontend persisted (mount,
    /// path) in its handle and calls resolve again to rebuild a `Node` after
    /// eviction, without re-walking from root.
    pub(crate) async fn resolve(&self, path: &Path, _ctx: &RequestCtx) -> Result<Node> {
        let (mount, rel) = self.split_mount_path(path)?;

        // The mount root is always a directory; no provider round trip needed.
        if rel.is_root() {
            return Ok(Node::new(
                mount,
                rel,
                EntryMeta::directory(),
                NodeBody::Provider,
            ));
        }

        let runtime = self.runtime_for(&mount)?;
        let Some((parent, name)) = rel.parent_and_name() else {
            return Err(TreeError::invalid_input(format!(
                "resolve: path has no parent: {}",
                rel.as_str()
            )));
        };

        self.resolve_child_in(mount, &runtime, &parent, name).await
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
    /// mount-root ignore file is always synthesized at the root before cached
    /// dirents or provider lookup (the host-owned file wins). Subtree outcomes
    /// resolve a provider tree reference into the retained host-tree record.
    pub(crate) async fn resolve_child(
        &self,
        parent: &Node,
        name: &str,
        _ctx: &RequestCtx,
    ) -> Result<Node> {
        if parent.host().is_some() {
            return self
                .resolve_host_child(parent, name)
                .await
                .map_err(Into::into);
        }
        if self.is_mount_enumeration_root(parent.mount(), parent.path())
            && self.mount_names().iter().any(|mount| mount == name)
        {
            return Ok(Node::new(
                name.to_string(),
                Path::root(),
                EntryMeta::directory(),
                NodeBody::Provider,
            ));
        }
        let runtime = self.runtime_for(parent.mount())?;
        self.resolve_child_in(parent.mount().to_string(), &runtime, parent.path(), name)
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
    ) -> Result<Node> {
        let rel = parent.join(name).map_err(|e| {
            TreeError::invalid_input(format!("resolve_child: invalid name {name:?}: {e}"))
        })?;

        if self.is_mount_enumeration_root(&mount, parent)
            && self.mount_names().iter().any(|m| m == name)
        {
            return Ok(Node::new(
                name.to_string(),
                Path::root(),
                EntryMeta::directory(),
                NodeBody::Provider,
            ));
        }

        // Root ignore files are host-owned. Resolve them before cached dirents
        // or provider lookup so a provider capture cannot change their kind.
        if parent.is_root() && synthetic::is_root_ignore_name(name) {
            let (meta, syn) = synthetic::resolve_synthetic_child(runtime, parent, name)?
                .expect("root ignore name must resolve synthetically");
            return Ok(Node::synthetic(mount, rel, meta, syn));
        }

        // A pagination control (`@next`/`@all`) resolves ONLY from the parent's
        // cached dirents, and does so for the directory's whole paginated
        // lifetime: the record persists past exhaustion so a name a consumer
        // already resolved from an earlier listing snapshot keeps resolving.
        // A reserved control name is never a real provider entry, so a
        // directory that never paged (no cached record) is NotFound; we never
        // consult the provider for it.
        if synthetic::is_control_name(name) {
            return match synthetic::resolve_synthetic_child(runtime, parent, name)? {
                Some((meta, syn)) => Ok(Node::synthetic(mount, rel, meta, syn)),
                None => Err(TreeError::not_found(rel.as_str())),
            };
        }

        if let Some(meta) = cached_dirent_child(runtime, parent, name)? {
            return Ok(Node::new(mount, rel, meta, NodeBody::Provider));
        }

        crate::inspector::cache_event(CacheKind::BrowseMiss);
        match runtime.lookup_child(parent, name).await? {
            LookupOutcome::Entry(entry) => Ok(Node::new(
                mount,
                entry.path().clone(),
                entry.meta().clone(),
                NodeBody::Provider,
            )),
            LookupOutcome::Subtree(tref) => {
                let dir = runtime
                    .tree_ref(tref)
                    .ok_or_else(|| TreeError::internal(format!("unresolved tree_ref {tref}")))?;
                Ok(Node::new(
                    mount,
                    rel,
                    EntryMeta::directory(),
                    NodeBody::Host {
                        tree_ref: dir,
                        relative: std::path::PathBuf::new(),
                        kind: super::node::HostKind::Directory,
                    },
                ))
            },
            // Controls may have been cached by a prior paged listing. Root
            // ignore names were handled before provider lookup above.
            LookupOutcome::NotFound => {
                match synthetic::resolve_synthetic_child(runtime, parent, name)? {
                    Some((meta, syn)) => Ok(Node::synthetic(mount, rel, meta, syn)),
                    None => Err(TreeError::not_found(rel.as_str())),
                }
            },
        }
    }
}

fn cached_dirent_child(runtime: &Runtime, parent: &Path, name: &str) -> Result<Option<EntryMeta>> {
    let record = runtime
        .resources
        .cache_get(parent, RecordKind::Dirents, None)
        .map_err(|error| TreeError::internal(error.to_string()))?;
    let Some(record) = record else {
        return Ok(None);
    };
    let dirents: DirentsPayload = postcard::from_bytes(&record.payload)
        .map_err(|error| TreeError::internal(error.to_string()))?;
    let entry = dirents
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.meta.clone());
    if entry.is_some() {
        crate::inspector::cache_event(CacheKind::BrowseHit);
    }
    Ok(entry)
}
