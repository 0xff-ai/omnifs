//! Git operations via the `gix` crate.
//!
//! Implements provider git callouts. Today the host supports only
//! `open_repo`, which clones a remote if needed and returns a tree-ref
//! handle the subtree handoff resolves to a filesystem path. Tree
//! traversal and blob reads run through FUSE bind-mount reads of the
//! clone directory, not through the WIT.

use crate::runtime::capability::CapabilityChecker;
use crate::runtime::cloner::GitCloner;
use crate::runtime::executor::{CalloutResponse, ErrorKind};
use crate::runtime::tree_refs::TreeRefs;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;

pub struct GitExecutor {
    cloner: Arc<GitCloner>,
    capability: Arc<CapabilityChecker>,
    trees: Arc<TreeRefs>,
}

impl GitExecutor {
    pub fn new(
        cloner: Arc<GitCloner>,
        capability: Arc<CapabilityChecker>,
        trees: Arc<TreeRefs>,
    ) -> Self {
        Self {
            cloner,
            capability,
            trees,
        }
    }

    pub fn open_repo(&self, cache_key: &str, clone_url: &str) -> CalloutResponse {
        if let Err(e) = self.capability.check_git_url(clone_url) {
            return CalloutResponse::Error {
                kind: ErrorKind::Denied,
                message: e.to_string(),
                retryable: false,
            };
        }

        let cache_path = match self.cloner.clone_if_needed(cache_key, clone_url) {
            Ok(p) => p,
            Err(e) => {
                warn!(cache_key, clone_url, error = %e, "clone failed");
                return CalloutResponse::Error {
                    kind: ErrorKind::Network,
                    message: e.to_string(),
                    retryable: true,
                };
            },
        };

        let id = self.trees.register(cache_path);
        CalloutResponse::GitRepoOpened(id)
    }

    /// Look up the local filesystem path for a `repo-id`.
    /// Used by the runtime to resolve `subtree` operation results.
    pub fn repo_path(&self, repo_id: u64) -> Option<PathBuf> {
        self.trees.resolve(repo_id)
    }

    /// Register a local repo path directly, returning its repo ID.
    pub fn register_local(&self, path: PathBuf) -> u64 {
        self.trees.register(path)
    }
}
