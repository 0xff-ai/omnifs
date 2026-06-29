//! Typed async Git callout builders.
//!
//! `cx.git().open_repo(cache_key, clone_url).await` issues a `git-open-repo`
//! callout: the host checks `clone_url` against the provider's capability
//! grants, clones the remote into a host-side cache directory if it is not
//! already there, and returns a `GitRepoInfo` whose `tree` field is the
//! handle a treeref route returns (wrap it in
//! [`crate::handler::TreeRef::new`]). The repository bytes never enter guest
//! memory: traversal and file reads are served through FUSE bind mounts of
//! the clone directory, not through the WIT.
//!
//! `cache_key` names the clone directory and must be a safe relative path:
//! slash-separated, no leading `/`, no `.` or `..` components, no NUL.
//! Repeating a key reuses the existing clone without refetching; a key
//! already bound to a different `clone_url` is an error rather than a silent
//! overwrite. Pick a key that is stable per repository, such as
//! `github.com/<owner>/<repo>`.
//!
//! ```ignore
//! let opened = cx
//!     .git()
//!     .open_repo(
//!         format!("github.com/{owner}/{repo}"),
//!         format!("git@github.com:{owner}/{repo}.git"),
//!     )
//!     .await?;
//! Ok(TreeRef::new(opened.tree))
//! ```

use crate::cx::Cx;
use crate::http::CalloutFuture;
use omnifs_wit::provider::types::{Callout, CalloutResult, GitOpenRequest, GitRepoInfo};

/// Entry point returned by `cx.git()`.
pub struct Builder<'cx, S> {
    cx: &'cx Cx<S>,
}

impl<'cx, S> Builder<'cx, S> {
    pub fn new(cx: &'cx Cx<S>) -> Self {
        Self { cx }
    }

    /// Open (cloning if needed) a repository host-side. Awaiting suspends
    /// the operation while the host clones; a cache hit resumes without
    /// network work. See the module docs for the `cache_key` contract.
    pub fn open_repo(
        self,
        cache_key: impl Into<String>,
        clone_url: impl Into<String>,
    ) -> CalloutFuture<'cx, GitRepoInfo> {
        CalloutFuture::new(
            self.cx,
            Callout::GitOpenRepo(GitOpenRequest {
                cache_key: cache_key.into(),
                clone_url: clone_url.into(),
            }),
            |r| {
                crate::http::expect_callout(
                    "git-open-repo",
                    |r| match r {
                        CalloutResult::GitRepoOpened(info) => Some(Ok(info)),
                        _ => None,
                    },
                    r,
                )
            },
        )
    }
}
