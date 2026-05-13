//! Git operations via the `gix` crate.
//!
//! Implements provider git callouts. Today the host supports only
//! `open_repo`, which clones a remote if needed and returns a tree-ref
//! handle the subtree handoff resolves to a filesystem path. Tree
//! traversal and blob reads run through FUSE bind-mount reads of the
//! clone directory, not through the WIT.

use crate::omnifs::provider::types as wit_types;
use crate::runtime::capability::{CapabilityChecker, CapabilityError};
use crate::runtime::cloner::{CloneError, GitCloner};
use crate::runtime::tree_refs::TreeRefs;
use crate::runtime::{callout_denied, callout_network};
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

    pub fn open_repo(&self, req: &wit_types::GitOpenRequest) -> wit_types::CalloutResult {
        match self.open_repo_inner(req) {
            Ok(id) => wit_types::CalloutResult::GitRepoOpened(wit_types::GitRepoInfo {
                repo: id,
                tree: id,
            }),
            Err(e) => e.into(),
        }
    }

    fn open_repo_inner(&self, req: &wit_types::GitOpenRequest) -> Result<u64, GitError> {
        self.capability.check_git_url(&req.clone_url)?;
        let cache_path = self
            .cloner
            .clone_if_needed(&req.cache_key, &req.clone_url)
            .map_err(|error| {
                warn!(
                    cache_key = %req.cache_key,
                    clone_url = %req.clone_url,
                    error = %error,
                    "clone failed"
                );
                GitError::from(error)
            })?;
        Ok(self.trees.register(cache_path))
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

#[derive(Debug, thiserror::Error)]
enum GitError {
    #[error("{0}")]
    Denied(String),
    #[error("{0}")]
    Clone(String),
}

impl From<CapabilityError> for GitError {
    fn from(error: CapabilityError) -> Self {
        Self::Denied(error.to_string())
    }
}

impl From<CloneError> for GitError {
    fn from(error: CloneError) -> Self {
        Self::Clone(error.to_string())
    }
}

impl From<GitError> for wit_types::CalloutResult {
    fn from(error: GitError) -> Self {
        match error {
            GitError::Denied(msg) => callout_denied(msg),
            // Preserve today's behavior: clone failures map to Network +
            // retryable=true. Transient network blips during git clone
            // are common and the runtime trusts the WIT-level retry hint.
            GitError::Clone(msg) => callout_network(msg),
        }
    }
}
