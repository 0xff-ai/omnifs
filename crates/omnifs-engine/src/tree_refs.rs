//! Shared `tree-ref -> filesystem path` registry.
//!
//! Git clones materialize as plain directories on disk that the host serves
//! through bind mounts. A shared u64 ID space keeps a `tree-ref` returned to
//! the provider unambiguous: there is only one source of truth for resolution.

use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct TreeRefs {
    paths: DashMap<u64, PathBuf>,
    next_id: AtomicU64,
}

impl TreeRefs {
    pub fn new() -> Self {
        Self {
            paths: DashMap::new(),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn register(&self, path: PathBuf) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.paths.insert(id, path);
        id
    }

    pub fn resolve(&self, tree_ref: u64) -> Option<PathBuf> {
        self.paths.get(&tree_ref).map(|r| r.clone())
    }
}

impl Default for TreeRefs {
    fn default() -> Self {
        Self::new()
    }
}
