//! Git operations via the system `git` CLI.
//!
//! Implements provider git callouts. Today the host supports only
//! `open_repo`, which clones a remote if needed and returns a tree-ref
//! handle the subtree handoff resolves to a filesystem path. Tree
//! traversal and blob reads run through FUSE bind-mount reads of the
//! clone directory, not through the WIT.

use crate::callouts::{callout_denied, callout_network, record_outcome};
use crate::capability::CapabilityChecker;
use crate::cloner::{CloneError, GitCloner};
use crate::log_redaction::LogUrl;
use crate::tree_refs::TreeRefs;
use omnifs_caps::Error as CapabilityError;
use omnifs_wit::provider::types as wit_types;
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

    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        url = %LogUrl(&req.clone_url),
        tree_ref = tracing::field::Empty,
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub fn open_repo(
        &self,
        req: &wit_types::GitOpenRequest,
        operation_id: u64,
    ) -> wit_types::CalloutResult {
        let result = match self.open_repo_inner(req, operation_id) {
            Ok(id) => wit_types::CalloutResult::GitRepoOpened(wit_types::GitRepoInfo {
                repo: id,
                tree: id,
            }),
            Err(e) => e.into(),
        };
        record_outcome(&result);
        result
    }

    fn open_repo_inner(
        &self,
        req: &wit_types::GitOpenRequest,
        operation_id: u64,
    ) -> Result<u64, GitError> {
        self.capability.check_git_url(&req.clone_url)?;
        let cache_path = self
            .cloner
            .clone_if_needed(
                &req.cache_key,
                &req.clone_url,
                |cache_key, clone_url| {
                    super::inspector::record_clone_start(operation_id, cache_key, clone_url);
                },
                |cache_key, elapsed, ok| {
                    super::inspector::record_clone_end(operation_id, cache_key, elapsed, ok);
                },
            )
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
