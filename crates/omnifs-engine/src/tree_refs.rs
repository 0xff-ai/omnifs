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
    pub(crate) root: Arc<Dir>,
}

impl fmt::Debug for TreeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TreeRef").field("id", &self.id).finish()
    }
}

impl PartialEq for TreeRef {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for TreeRef {}

pub struct TreeRefs {
    refs: DashMap<u64, TreeRef>,
    next_handle: AtomicU64,
}

impl TreeRefs {
    pub fn new() -> Self {
        Self {
            refs: DashMap::new(),
            next_handle: AtomicU64::new(1),
        }
    }

    pub fn register(&self, id: GitId, path: &Path) -> std::io::Result<u64> {
        let root = Arc::new(Dir::open_ambient_dir(path, ambient_authority())?);
        let tree_ref = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.refs.insert(tree_ref, TreeRef { id, root });
        Ok(tree_ref)
    }

    pub(crate) fn resolve(&self, tree_ref: u64) -> Option<TreeRef> {
        self.refs.get(&tree_ref).map(|r| r.clone())
    }

    pub(crate) fn identity(&self, tree_ref: u64) -> Option<GitId> {
        self.refs
            .get(&tree_ref)
            .map(|reference| reference.id.clone())
    }
}

impl Default for TreeRefs {
    fn default() -> Self {
        Self::new()
    }
}
