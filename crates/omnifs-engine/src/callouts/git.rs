//! Git operations via the system `git` CLI.
//!
//! The `open_repo` callout clones a remote if needed and returns a tree-ref
//! handle that the subtree handoff resolves to a filesystem path. Tree
//! traversal and blob reads run through bind-mount reads of the clone
//! directory, not through the WIT.

use crate::authority::RuntimeAuthority;
use crate::cache::identity::GitId;
use crate::callouts::{callout_denied, callout_invalid, callout_network, record_outcome};
use crate::cloner::{CloneError, GitCloner};
use crate::log_redaction::LogUrl;
use crate::tree_refs::TreeRefs;
use omnifs_wit::provider::types as wit_types;
use std::sync::Arc;
use tracing::warn;
use url::Url;

#[derive(Clone)]
pub struct GitExecutor {
    cloner: Arc<GitCloner>,
    authority: Arc<RuntimeAuthority>,
    trees: Arc<TreeRefs>,
    mount_scope: String,
}

impl GitExecutor {
    pub fn new(
        cloner: Arc<GitCloner>,
        authority: Arc<RuntimeAuthority>,
        trees: Arc<TreeRefs>,
        mount_scope: impl Into<String>,
    ) -> Self {
        Self {
            cloner,
            authority,
            trees,
            mount_scope: mount_scope.into(),
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
        self.authority.check_git_url(&req.clone_url)?;
        let remote = canonical_remote(&req.clone_url)?;
        if let Some(reference) = req.reference.as_deref() {
            GitCloner::validate_reference(reference)
                .map_err(|error| GitError::Invalid(error.to_string()))?;
        }
        let id = GitId::new(&self.mount_scope, &remote, req.reference.as_deref());
        let cache_path = self
            .cloner
            .clone_if_needed(
                &id,
                &req.clone_url,
                &remote,
                req.reference.as_deref(),
                operation_id,
            )
            .map_err(|error| {
                warn!(
                    cache_id = %id,
                    clone_url = %LogUrl(&req.clone_url),
                    error = %error,
                    "clone failed"
                );
                GitError::from(error)
            })?;
        Ok(self.trees.register(cache_path))
    }
}

fn canonical_remote(raw: &str) -> Result<String, GitError> {
    let remote = raw.trim();
    if remote.is_empty()
        || remote
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte == 0)
    {
        return Err(GitError::Invalid("invalid Git remote".to_string()));
    }
    if let Ok(mut url) = Url::parse(remote) {
        if !matches!(url.scheme(), "https" | "ssh" | "git") || url.host_str().is_none() {
            return Err(GitError::Invalid("unsupported Git remote".to_string()));
        }
        if url.scheme() == "https" || url.scheme() == "git" {
            url.set_username("")
                .map_err(|_| GitError::Invalid("invalid Git remote username".to_string()))?;
        }
        url.set_password(None)
            .map_err(|_| GitError::Invalid("invalid Git remote password".to_string()))?;
        return Ok(url.to_string());
    }

    let (user_host, path) = remote
        .split_once(':')
        .ok_or_else(|| GitError::Invalid("invalid Git remote".to_string()))?;
    let (username, host) = user_host
        .rsplit_once('@')
        .map_or((None, user_host), |(username, host)| (Some(username), host));
    if host.is_empty() || path.is_empty() || path.starts_with('/') {
        return Err(GitError::Invalid("invalid Git remote".to_string()));
    }
    Ok(match username {
        Some(username) => format!("{username}@{host}:{path}"),
        None => format!("{host}:{path}"),
    })
}

#[derive(Debug, thiserror::Error)]
enum GitError {
    #[error("{0}")]
    Denied(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Clone(String),
}

impl From<crate::authority::AuthorityError> for GitError {
    fn from(_error: crate::authority::AuthorityError) -> Self {
        Self::Denied("Git remote is not allowed".to_string())
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
            GitError::Invalid(msg) => callout_invalid(msg),
            // Clone failures map to Network +
            // retryable=true. Transient network blips during git clone
            // are common and the runtime trusts the WIT-level retry hint.
            GitError::Clone(msg) => callout_network(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GitCloner, GitExecutor, GitId, canonical_remote};
    use crate::authority::RuntimeAuthority;
    use crate::tree_refs::TreeRefs;
    use omnifs_wit::provider::types as wit_types;
    use std::sync::Arc;

    #[test]
    fn git_identity_excludes_remote_credentials_but_keeps_reference_and_mount() {
        let first = canonical_remote("https://alice:token@example.test/repo.git").unwrap();
        let second = canonical_remote("https://bob:rotated@example.test/repo.git").unwrap();
        assert_eq!(first, "https://example.test/repo.git");
        assert_eq!(first, second);
        assert_eq!(
            GitId::new("mount", &first, Some("main")),
            GitId::new("mount", &second, Some("main"))
        );
        assert_ne!(
            GitId::new("mount", &first, Some("main")),
            GitId::new("mount", &first, Some("release"))
        );
    }

    #[test]
    fn git_identity_preserves_ssh_and_scp_usernames() {
        let ssh_alice = canonical_remote("ssh://alice@example.test/repo.git").unwrap();
        let ssh_bob = canonical_remote("ssh://bob@example.test/repo.git").unwrap();
        let scp_alice = canonical_remote("alice@example.test:repo.git").unwrap();
        let scp_bob = canonical_remote("bob@example.test:repo.git").unwrap();

        assert_ne!(
            GitId::new("mount", &ssh_alice, Some("main")),
            GitId::new("mount", &ssh_bob, Some("main"))
        );
        assert_ne!(
            GitId::new("mount", &scp_alice, Some("main")),
            GitId::new("mount", &scp_bob, Some("main"))
        );
        assert_eq!(ssh_alice, "ssh://alice@example.test/repo.git");
        assert_eq!(scp_alice, "alice@example.test:repo.git");
    }

    #[test]
    fn malformed_authenticated_remote_never_enters_wit_error_text() {
        let temp = tempfile::tempdir().unwrap();
        let executor = GitExecutor::new(
            Arc::new(GitCloner::new(temp.path().to_path_buf()).unwrap()),
            RuntimeAuthority::for_test(&[], &["*"], &[]),
            Arc::new(TreeRefs::new()),
            "mount",
        );

        for remote in [
            "ftp://alice:super-secret@example.test/repo.git",
            "https://alice:super-secret@[",
        ] {
            let result = executor.open_repo(
                &wit_types::GitOpenRequest {
                    clone_url: remote.to_string(),
                    reference: None,
                },
                1,
            );
            let text = format!("{result:?}");
            assert!(!text.contains(remote));
            assert!(!text.contains("super-secret"));
        }
    }
}
