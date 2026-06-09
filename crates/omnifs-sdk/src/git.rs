//! Typed async Git callout builders.

use crate::cx::Cx;
use crate::http::CalloutFuture;
use omnifs_wit::provider::types::{Callout, CalloutResult, GitOpenRequest, GitRepoInfo};

pub struct Builder<'cx, S> {
    cx: &'cx Cx<S>,
}

impl<'cx, S> Builder<'cx, S> {
    pub fn new(cx: &'cx Cx<S>) -> Self {
        Self { cx }
    }

    pub fn open_repo(
        self,
        cache_key: impl Into<String>,
        clone_url: impl Into<String>,
    ) -> CalloutFuture<'cx, S, GitRepoInfo> {
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
