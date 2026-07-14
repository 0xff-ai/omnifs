//! Typed async Git callout builders.
//!
//! `cx.git().open_repo(clone_url).send().await` issues a `git-open-repo`
//! callout: the host checks `clone_url` against the provider's capability
//! grants, clones the remote into a host-side cache directory if it is not
//! already there, and returns a `GitRepoInfo` whose `tree` field is the
//! handle a tree-capable directory face returns (wrap it in
//! [`crate::handler::TreeRef::new`]). The repository bytes never enter guest
//! memory: traversal and file reads are served through FUSE bind mounts of
//! the clone directory, not through the WIT.
//!
//! The host validates and derives the clone identity from the remote and
//! optional reference, so providers never name cache entries.
//!
//! ```ignore
//! let opened = cx
//!     .git()
//!     .open_repo(format!("git@github.com:{owner}/{repo}.git"))
//!     .send()
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
    /// network work. The host owns cache identity and layout.
    pub fn open_repo(self, clone_url: impl Into<String>) -> OpenRequest<'cx, S> {
        OpenRequest {
            cx: self.cx,
            clone_url: clone_url.into(),
            reference: None,
        }
    }
}

#[must_use]
pub struct OpenRequest<'cx, S> {
    cx: &'cx Cx<S>,
    clone_url: String,
    reference: Option<String>,
}

impl<'cx, S> OpenRequest<'cx, S> {
    pub fn reference(mut self, reference: impl Into<String>) -> Self {
        self.reference = Some(reference.into());
        self
    }

    pub fn send(self) -> CalloutFuture<'cx, GitRepoInfo> {
        CalloutFuture::new(
            self.cx,
            Callout::GitOpenRepo(GitOpenRequest {
                clone_url: self.clone_url,
                reference: self.reference,
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
