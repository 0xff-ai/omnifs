//! Internal provider path resolution and child traversal.

use crate::cache::MountResources;
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

        // A provider may hand the mount root itself to a durable Git subtree.
        // Resolve it only through the coherent exact positive lookup fact.
        if rel.is_root() {
            if mount.is_empty() {
                return Ok(Node::new(
                    mount,
                    rel,
                    EntryMeta::directory(),
                    NodeBody::Provider,
                ));
            }
            return self.mount_root_node(mount);
        }

        let entry = self.entry_for(&mount)?;
        let Some((parent, name)) = rel.parent_and_name() else {
            return Err(TreeError::invalid_input(format!(
                "resolve: path has no parent: {}",
                rel.as_str()
            )));
        };

        self.resolve_child_in(mount, entry, &parent, name).await
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
        if !parent.is_dir() {
            return Err(TreeError {
                kind: super::error::TreeErrorKind::NotDirectory,
                message: format!("{} is not a directory", parent.path()),
                retryable: false,
                retry_after: None,
            });
        }
        if Self::is_mount_enumeration_root(parent.mount(), parent.path())
            && self.mount_names().iter().any(|mount| mount == name)
        {
            return self.mount_root_node(name.to_string());
        }
        let entry = self.entry_for(parent.mount())?;
        self.resolve_child_in(parent.mount().to_string(), entry, parent.path(), name)
            .await
    }

    /// Shared body for [`resolve`](Self::resolve) and
    /// [`resolve_child`](Self::resolve_child): resolve `name` under `parent`
    /// (a mount-relative directory path) on the runtime serving `mount`.
    async fn resolve_child_in(
        &self,
        mount: String,
        entry: &crate::registry::MountEntry,
        parent: &Path,
        name: &str,
    ) -> Result<Node> {
        let resources = entry.resources();
        let rel = parent.join(name).map_err(|e| {
            TreeError::invalid_input(format!("resolve_child: invalid name {name:?}: {e}"))
        })?;

        if Self::is_mount_enumeration_root(&mount, parent)
            && self.mount_names().iter().any(|m| m == name)
        {
            return self.mount_root_node(name.to_string());
        }

        // Root ignore files are host-owned. Resolve them before cached dirents
        // or provider lookup so a provider capture cannot change their kind.
        if parent.is_root() && synthetic::is_root_ignore_name(name) {
            let (meta, syn) = synthetic::resolve_synthetic_child(resources, parent, name)?
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
            return match synthetic::resolve_synthetic_child(resources, parent, name)? {
                Some((meta, syn)) => Ok(Node::synthetic(mount, rel, meta, syn)),
                None => Err(TreeError::not_found(rel.as_str())),
            };
        }

        let offline = entry.runtime().is_none();
        let expired = !offline
            && resources
                .view_expired(&rel, crate::clock::now_millis())
                .map_err(|error| TreeError::internal(error.to_string()))?;
        if let Some(node) = cached_lookup_node(
            resources,
            entry.trees(),
            mount.as_str(),
            &rel,
            offline,
            expired,
        )? {
            return Ok(node);
        }

        let cached_parent = cached_dirents(resources, parent)?;
        if let Some(meta) = cached_parent.as_ref().and_then(|dirents| {
            dirents
                .entries
                .iter()
                .find(|entry| entry.name == name)
                .map(|entry| entry.meta.clone())
        }) {
            crate::inspector::cache_event(CacheKind::BrowseHit);
            return Ok(Node::new(mount, rel, meta, NodeBody::Provider));
        }

        if offline {
            if cached_parent
                .as_ref()
                .is_some_and(DirentsPayload::is_complete_offline)
            {
                return Err(TreeError::not_found(rel.as_str()));
            }
            return Err(TreeError::offline_miss(format!(
                "offline lookup has no complete fact for {rel}"
            )));
        }

        crate::inspector::cache_event(CacheKind::BrowseMiss);
        let runtime = entry.runtime().expect("online entry has a runtime");
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
                match synthetic::resolve_synthetic_child(resources, parent, name)? {
                    Some((meta, syn)) => Ok(Node::synthetic(mount, rel, meta, syn)),
                    None => Err(TreeError::not_found(rel.as_str())),
                }
            },
        }
    }

    fn mount_root_node(&self, mount: String) -> Result<Node> {
        let rel = Path::root();
        let entry = self.entry_for(&mount)?;
        let resources = entry.resources();
        let offline = entry.runtime().is_none();
        let expired = !offline
            && resources
                .view_expired(&rel, crate::clock::now_millis())
                .map_err(|error| TreeError::internal(error.to_string()))?;
        if offline || !expired {
            let positive_directory = resources
                .lookup_payload(&rel)
                .map_err(|error| TreeError::internal(error.to_string()))?
                .is_some_and(|lookup| {
                    matches!(
                        lookup,
                        crate::view::LookupPayload::Positive(meta) if meta.is_directory()
                    )
                });
            if positive_directory
                && let Some(git) = resources
                    .git_for_path(&rel)
                    .map_err(|error| TreeError::internal(error.to_string()))?
            {
                let tree_ref = entry
                    .trees()
                    .by_identity(&git.id, &git.relative_path)
                    .ok_or_else(|| TreeError::internal("validated Git tree is not open"))?;
                return Ok(Node::new(
                    mount,
                    rel,
                    EntryMeta::directory(),
                    NodeBody::Host {
                        tree_ref,
                        relative: std::path::PathBuf::new(),
                        kind: super::node::HostKind::Directory,
                    },
                ));
            }
        }

        // Ordinary mount roots remain provider-shaped while their durable
        // listing and lookup facts are consumed through MountResources.
        Ok(Node::new(
            mount,
            rel,
            EntryMeta::directory(),
            NodeBody::Provider,
        ))
    }
}

fn cached_lookup_node(
    resources: &MountResources,
    trees: &crate::tree_refs::TreeRefs,
    mount: &str,
    rel: &Path,
    offline: bool,
    expired: bool,
) -> Result<Option<Node>> {
    let Some(payload) = resources
        .lookup_payload(rel)
        .map_err(|error| TreeError::internal(error.to_string()))?
    else {
        return Ok(None);
    };

    match payload {
        crate::view::LookupPayload::Positive(meta) if offline || !expired => {
            if let Some(git) = resources
                .git_for_path(rel)
                .map_err(|error| TreeError::internal(error.to_string()))?
            {
                let tree_ref = trees
                    .by_identity(&git.id, &git.relative_path)
                    .ok_or_else(|| TreeError::internal("validated Git tree is not open"))?;
                return Ok(Some(Node::new(
                    mount.to_string(),
                    rel.clone(),
                    EntryMeta::directory(),
                    NodeBody::Host {
                        tree_ref,
                        relative: std::path::PathBuf::new(),
                        kind: super::node::HostKind::Directory,
                    },
                )));
            }
            crate::inspector::cache_event(CacheKind::BrowseHit);
            Ok(Some(Node::new(
                mount.to_string(),
                rel.clone(),
                meta,
                NodeBody::Provider,
            )))
        },
        crate::view::LookupPayload::Negative { .. } if offline || !expired => {
            Err(TreeError::not_found(rel.as_str()))
        },
        crate::view::LookupPayload::Positive(_) | crate::view::LookupPayload::Negative { .. } => {
            Ok(None)
        },
    }
}

fn cached_dirents(resources: &MountResources, parent: &Path) -> Result<Option<DirentsPayload>> {
    resources
        .dirents_payload(parent)
        .map_err(|error| TreeError::internal(error.to_string()))
}
