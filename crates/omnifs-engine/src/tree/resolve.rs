//! Internal provider path resolution and child traversal.

use crate::cache::MountResources;
use crate::effect_apply::LookupOutcome;
use crate::ops::namespace::PageRequest;
use crate::tree_refs::TreeRef;
use crate::view::{DirentsPayload, EntryMeta};
use omnifs_api::events::CacheKind;
use omnifs_core::path::Path;

use super::error::{Result, TreeError};
use super::node::{Node, NodeBody};
use super::synthetic;
use crate::{EngineNamespace, RequestCtx};

impl EngineNamespace {
    /// Resolve a full protocol path to a `Node`. Splits the path into
    /// (mount, mount-relative path); the mount root resolves to a directory
    /// without a provider round trip, and any deeper path resolves its leaf via
    /// [`resolve_child`](Self::resolve_child) against the parent directory.
    /// Doubles as path rehydration: a filesystem persisted (mount,
    /// path) in its handle and calls resolve again to rebuild a `Node` after
    /// eviction, without re-walking from root.
    pub(crate) async fn resolve(&self, path: &Path, _ctx: &RequestCtx) -> Result<Node> {
        let (mount, rel) = self.split_mount_path(path)?;

        if rel.is_root() {
            if mount.is_empty() {
                return Ok(Node::new(
                    mount,
                    rel,
                    EntryMeta::directory(),
                    NodeBody::Provider,
                ));
            }
            return self.mount_root_node(mount).await;
        }

        let entry = self.entry_for(&mount)?;
        let Some((parent, name)) = rel.parent_and_name() else {
            return Err(TreeError::invalid_input(format!(
                "resolve: path has no parent: {}",
                rel.as_str()
            )));
        };

        self.resolve_child_in(mount, entry, &parent, None, name, false)
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
    /// mount-root ignore file is always synthesized at the root before cached
    /// dirents or provider lookup (the host-owned file wins). Subtree outcomes
    /// resolve a provider tree reference into the retained host-tree record.
    pub(crate) async fn resolve_child(
        &self,
        parent: &Node,
        name: &str,
        _ctx: &RequestCtx,
    ) -> Result<Node> {
        self.resolve_child_as(parent, name, false).await
    }

    /// Resolve a child of `parent` that is above the fed path of an
    /// operation, on the way to it from the root. A fed child with its
    /// positive lookup in the projection resolves from it, without the
    /// provider: the lookup of the path of the operation takes the claims of
    /// the feeds of each path above it, and has them answer.
    pub(crate) async fn resolve_above_fed(&self, parent: &Node, name: &str) -> Result<Node> {
        self.resolve_child_as(parent, name, true).await
    }

    async fn resolve_child_as(&self, parent: &Node, name: &str, above_fed: bool) -> Result<Node> {
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
            return self.mount_root_node(name.to_string()).await;
        }
        let entry = self.entry_for(parent.mount())?;
        self.resolve_child_in(
            parent.mount().to_string(),
            entry,
            parent.path(),
            Some(parent.fact()),
            name,
            above_fed,
        )
        .await
    }

    /// Shared body for [`resolve`](Self::resolve) and
    /// [`resolve_child`](Self::resolve_child): resolve `name` under `parent`
    /// (a mount-relative directory path) on the runtime serving `mount`. A
    /// fed `parent` gives the path of its facts. A path under an anchor of
    /// another facet of an object resolves as the same path under the anchor
    /// of the claim of the object, without the provider, and the anchor
    /// itself keeps that anchor as the path of its facts.
    async fn resolve_child_in(
        &self,
        mount: String,
        entry: &crate::registry::MountEntry,
        parent: &Path,
        parent_fact: Option<&Path>,
        name: &str,
        above_fed: bool,
    ) -> Result<Node> {
        let resources = entry.resources()?;
        let rel = parent.join(name).map_err(|e| {
            TreeError::invalid_input(format!("resolve_child: invalid name {name:?}: {e}"))
        })?;

        if Self::is_mount_enumeration_root(&mount, parent)
            && self.mount_names().iter().any(|m| m == name)
        {
            return self.mount_root_node(name.to_string()).await;
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

        let fact = match parent_fact {
            Some(parent_fact) => parent_fact.join(name).map_err(|error| {
                TreeError::invalid_input(format!("resolve_child: invalid name {name:?}: {error}"))
            })?,
            None => under_facet_anchor(resources, &rel)?,
        };
        if fact != rel {
            let (fact_parent, _) = fact
                .parent_and_name()
                .ok_or_else(|| TreeError::internal(format!("{fact} has no parent")))?;
            let node =
                Box::pin(self.resolve_child_in(mount, entry, &fact_parent, None, name, above_fed))
                    .await?;
            return Ok(node.at(rel));
        }
        let anchor = resources
            .facet_anchor(&rel)
            .map_err(|error| TreeError::internal(error.to_string()))?;
        let node = self
            .resolve_own_child(mount, entry, parent, name, rel, above_fed)
            .await?;
        Ok(match anchor {
            Some(anchor) => node.with_fact(anchor),
            None => node,
        })
    }

    /// Resolve `name` under `parent` at its own path, `rel`.
    async fn resolve_own_child(
        &self,
        mount: String,
        entry: &crate::registry::MountEntry,
        parent: &Path,
        name: &str,
        rel: Path,
        above_fed: bool,
    ) -> Result<Node> {
        let resources = entry.resources()?;
        let offline = entry.runtime().is_none();
        // A fed fact is served once its feeds have answered, so its lookup
        // asks the provider for their claims.
        let fed = !offline
            && resources
                .is_fed(&rel)
                .map_err(|error| TreeError::internal(error.to_string()))?;
        if fed
            && above_fed
            && let Some(crate::view::LookupPayload::Positive(meta)) = resources
                .lookup_payload(&rel)
                .map_err(|error| TreeError::internal(error.to_string()))?
        {
            return Ok(Node::fed(mount, rel.clone(), rel, meta));
        }
        if !fed {
            if let Some(tree_ref) = self.current_tree(entry, &rel).await? {
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
            if let Some(node) = cached_child_node(entry, &mount, parent, name, &rel)? {
                return Ok(node);
            }
        }

        crate::inspector::cache_event(CacheKind::BrowseMiss);
        let runtime = entry.runtime().expect("online entry has a runtime");
        match runtime.lookup_child(parent, name).await? {
            LookupOutcome::Entry(entry) if fed => Ok(Node::fed(
                mount,
                entry.path().clone(),
                entry.path().clone(),
                entry.meta().clone(),
            )),
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
            LookupOutcome::Feed(claims) => {
                let (at, fact) = super::feed::answer_claims(
                    &runtime,
                    &rel,
                    super::feed::FedFact::Lookup,
                    &claims,
                )
                .await?;
                fed_lookup_node(resources, mount, rel, &at, fact)
            },
        }
    }

    async fn mount_root_node(&self, mount: String) -> Result<Node> {
        let rel = Path::root();
        let entry = self
            .configured_entry(&mount)
            .ok_or_else(|| TreeError::not_found(format!("no such mount: {mount}")))?;
        // A provider may hand the mount root itself to a Git tree.
        if let Some(tree_ref) = self.current_tree(entry, &rel).await? {
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

        // Ordinary mount roots remain provider-shaped while their durable
        // listing and lookup facts are consumed through MountResources.
        Ok(Node::new(
            mount,
            rel,
            EntryMeta::directory(),
            NodeBody::Provider,
        ))
    }

    /// The open tree of the Git fact of the tree path `rel`, at the commit of
    /// the fact. Online, past the deadline of the tree path and outside the
    /// rate-limit window of the mount, `list-children` of the tree path with
    /// the commit as its validator comes first, in the one flight of the
    /// path: `unchanged` renews the deadline, and a subtree with a new commit
    /// updates the Git fact and renews it, and records the tree path as
    /// changed. `None` without a Git fact of `rel` or without its tree open.
    pub(crate) async fn current_tree(
        &self,
        entry: &crate::registry::MountEntry,
        rel: &Path,
    ) -> Result<Option<TreeRef>> {
        let resources = entry.resources()?;
        let captured_epoch = resources.current_epoch();
        let Some(git) = resources
            .git_for_path(rel)
            .map_err(|error| TreeError::internal(error.to_string()))?
        else {
            return Ok(None);
        };
        let Some(tree_ref) = entry.trees().by_identity(&git.id, &git.relative_path) else {
            return Ok(None);
        };
        let Some(runtime) = entry.runtime() else {
            return Ok(Some(tree_ref));
        };
        if runtime.rate_limited_until().is_some()
            || !resources
                .view_expired(rel, crate::clock::now_millis())
                .map_err(|error| TreeError::internal(error.to_string()))?
        {
            return Ok(Some(tree_ref));
        }
        crate::inspector::cache_event(CacheKind::BrowseMiss);
        let request = PageRequest {
            cursor: None,
            validator: Some(git.commit),
        };
        match runtime.list_children(rel, request, captured_epoch).await? {
            crate::ops::namespace::ListOutcome::Unchanged => Ok(Some(tree_ref)),
            crate::ops::namespace::ListOutcome::Subtree(tree) => {
                // A filesystem answers the root of the tree from what it holds
                // until the attributes of the root change.
                runtime.record_dir_changed(rel, false);
                runtime
                    .tree_ref(tree)
                    .map(Some)
                    .ok_or_else(|| TreeError::internal(format!("unresolved tree_ref {tree}")))
            },
            crate::ops::namespace::ListOutcome::Entries(_)
            | crate::ops::namespace::ListOutcome::Feed(_) => {
                Err(TreeError::internal("the tree path answered no tree"))
            },
        }
    }
}

/// A child served from the cache without the provider: its cached lookup, or
/// its entry in the parent's cached listing. Offline, a child the cache does
/// not answer is not found under a complete listing and a miss otherwise.
fn cached_child_node(
    entry: &crate::registry::MountEntry,
    mount: &str,
    parent: &Path,
    name: &str,
    rel: &Path,
) -> Result<Option<Node>> {
    let resources = entry.resources()?;
    let offline = entry.runtime().is_none();
    let expired = !offline
        && resources
            .view_expired(rel, crate::clock::now_millis())
            .map_err(|error| TreeError::internal(error.to_string()))?;
    if let Some(node) = cached_lookup_node(resources, mount, rel, offline, expired)? {
        return Ok(Some(node));
    }

    // Online, a directory the parent's listing names resolves through its own
    // lookup, which alone tells a provider directory from a Git tree handoff.
    let cached_parent = cached_dirents(resources, parent)?;
    if let Some(meta) = cached_parent.as_ref().and_then(|dirents| {
        dirents
            .entries()
            .find(|entry| entry.name == name && (offline || entry.meta.is_file()))
            .map(|entry| entry.meta.clone())
    }) {
        crate::inspector::cache_event(CacheKind::BrowseHit);
        return Ok(Some(Node::new(
            mount.to_string(),
            rel.clone(),
            meta,
            NodeBody::Provider,
        )));
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
    Ok(None)
}

/// `rel` with the leading segments of the nearest anchor of another facet
/// above it replaced by the anchor of its claim; `rel` itself without one.
fn under_facet_anchor(resources: &MountResources, rel: &Path) -> Result<Path> {
    let mut above = rel.parent_and_name().map(|(parent, _)| parent);
    while let Some(path) = above {
        if let Some(anchor) = resources
            .facet_anchor(&path)
            .map_err(|error| TreeError::internal(error.to_string()))?
        {
            return rel
                .segments()
                .skip(path.segments().count())
                .try_fold(anchor, |at, segment| {
                    at.join(segment)
                        .map_err(|error| TreeError::internal(error.to_string()))
                });
        }
        above = path.parent_and_name().map(|(parent, _)| parent);
    }
    Ok(rel.clone())
}

/// A fed child served from its lookup at `at` in the projection once its
/// feeds have answered, with its facts at `fact`, or not found.
fn fed_lookup_node(
    resources: &MountResources,
    mount: String,
    rel: Path,
    at: &Path,
    fact: Path,
) -> Result<Node> {
    match resources
        .lookup_payload(at)
        .map_err(|error| TreeError::internal(error.to_string()))?
    {
        Some(crate::view::LookupPayload::Positive(meta)) => Ok(Node::fed(mount, rel, fact, meta)),
        Some(crate::view::LookupPayload::Negative { .. }) | None => {
            Err(TreeError::not_found(rel.as_str()))
        },
    }
}

fn cached_lookup_node(
    resources: &MountResources,
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
            // An open tree serves before the cache. Git facts persist across
            // a restart while open trees live only in memory: without an open
            // tree, the lookup is a miss, and the provider opens the
            // repository again.
            if resources
                .git_for_path(rel)
                .map_err(|error| TreeError::internal(error.to_string()))?
                .is_some()
            {
                return Ok(None);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::identity::GitId;
    use crate::cache::{
        Caches, FactPayload, GitWrite, ProjectionId, ProjectionTransition, RecordWrite,
    };
    use crate::view::LookupPayload;
    use omnifs_core::MountName;

    #[test]
    fn a_git_fact_without_an_open_tree_is_a_lookup_miss() {
        // A restart keeps the Git fact of a path but no open tree in memory.
        let dir = tempfile::tempdir().unwrap();
        let caches = Caches::open(dir.path()).unwrap();
        let resources = caches
            .mount(&MountName::new("m").unwrap(), ProjectionId::new(b"m"), b"m")
            .unwrap();
        let path = Path::parse("/owner/repo/repo").unwrap();
        resources
            .publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: path.clone(),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                    }],
                    git: vec![GitWrite {
                        path: path.clone(),
                        id: GitId::new("m", "https://example.test/repo.git", None),
                        relative_path: String::new(),
                        commit: "0123456789abcdef0123456789abcdef01234567".into(),
                    }],
                    ..ProjectionTransition::default()
                },
                resources.current_epoch(),
            )
            .unwrap();

        let node = cached_lookup_node(&resources, "m", &path, false, false)
            .expect("a Git fact without an open tree is no internal error");
        assert!(node.is_none());
    }
}
