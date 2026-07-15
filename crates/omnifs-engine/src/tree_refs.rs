//! Shared `tree-ref -> filesystem path` registry.
//!
//! Git clones materialize as plain directories on disk that the host serves
//! through bind mounts. A shared u64 ID space keeps a `tree-ref` returned to
//! the provider unambiguous: there is only one source of truth for resolution.

use crate::cache::identity::GitId;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use dashmap::DashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone)]
pub(crate) struct TreeRef {
    pub(crate) id: GitId,
    pub(crate) relative_path: String,
    pub(crate) root: Arc<Dir>,
}

impl fmt::Debug for TreeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TreeRef")
            .field("id", &self.id)
            .field("relative_path", &self.relative_path)
            .finish_non_exhaustive()
    }
}

impl PartialEq for TreeRef {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.relative_path == other.relative_path
    }
}

impl Eq for TreeRef {}

pub struct TreeRefs {
    refs: DashMap<u64, TreeRef>,
    identities: DashMap<(GitId, String), TreeRef>,
    next_handle: AtomicU64,
}

impl TreeRefs {
    pub fn new() -> Self {
        Self {
            refs: DashMap::new(),
            identities: DashMap::new(),
            next_handle: AtomicU64::new(1),
        }
    }

    pub(crate) fn register(&self, id: GitId, path: &Path) -> std::io::Result<u64> {
        let opened = self.open(id, "", path)?;
        let tree_ref = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.refs.insert(tree_ref, opened);
        Ok(tree_ref)
    }

    pub(crate) fn open(
        &self,
        id: GitId,
        relative_path: &str,
        selected_path: &Path,
    ) -> std::io::Result<TreeRef> {
        let key = (id, relative_path.to_string());
        if let Some(existing) = self.identities.get(&key) {
            return Ok(existing.clone());
        }
        let opened = TreeRef {
            id,
            relative_path: relative_path.to_string(),
            root: Arc::new(Dir::open_ambient_dir(selected_path, ambient_authority())?),
        };
        self.identities.insert(key, opened.clone());
        Ok(opened)
    }

    pub(crate) fn by_identity(&self, id: &GitId, relative_path: &str) -> Option<TreeRef> {
        self.identities
            .get(&(*id, relative_path.to_string()))
            .map(|reference| reference.clone())
    }

    pub(crate) fn resolve(&self, tree_ref: u64) -> Option<TreeRef> {
        self.refs.get(&tree_ref).map(|r| r.clone())
    }
}

impl Default for TreeRefs {
    fn default() -> Self {
        Self::new()
    }
}
