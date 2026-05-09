//! Shared `tree-ref -> filesystem path` registry.
//!
//! Both git clones and extracted archive trees materialize as plain
//! directories on disk that the host serves through bind mounts. They
//! share a single u64 ID space so a `tree-ref` returned to the provider
//! is unambiguous: there is only one source of truth for resolution.

use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct TreeRegistry {
    paths: DashMap<u64, PathBuf>,
    next_id: AtomicU64,
}

impl TreeRegistry {
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

impl Default for TreeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_assigns_unique_ids() {
        let reg = TreeRegistry::new();
        let a = reg.register(PathBuf::from("/tmp/a"));
        let b = reg.register(PathBuf::from("/tmp/b"));
        assert_ne!(a, b);
        assert_eq!(reg.resolve(a).unwrap(), PathBuf::from("/tmp/a"));
        assert_eq!(reg.resolve(b).unwrap(), PathBuf::from("/tmp/b"));
    }

    #[test]
    fn resolve_unknown_returns_none() {
        let reg = TreeRegistry::new();
        assert!(reg.resolve(9999).is_none());
    }
}
