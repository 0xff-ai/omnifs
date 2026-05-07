use omnifs_sdk::prelude::*;

use crate::owners::{fetch_owner_repos, resolve_owner_kind};
use crate::types::{OwnerName, RepoName};
use crate::{Result, State};

pub struct RootHandlers;

#[handlers]
impl RootHandlers {
    // No `/` handler: GitHub has no "list all visible owners" API to back
    // it with, so the root is just an implicit prefix dir over the
    // dynamic-capture `/{owner}` route below. The SDK derives that
    // automatically — and crucially leaves the listing non-exhaustive,
    // so `lookup("/", "raulk")` falls through to the capture handler
    // instead of being short-circuited to ENOENT by the host's cache.

    #[dir("/{owner}")]
    async fn repos(cx: &DirCx<State>, owner: OwnerName) -> Result<Projection> {
        let kind = resolve_owner_kind(cx, &owner)
            .await?
            .ok_or_else(|| ProviderError::not_found("owner not found"))?;
        let mut repos = fetch_owner_repos(cx, &owner, kind)
            .await?
            .into_iter()
            .map(|name| {
                name.parse::<RepoName>().map_err(|()| {
                    ProviderError::internal(format!(
                        "GitHub repo name is not a safe path segment: {name}"
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        repos.sort();

        let mut projection = Projection::new();
        for repo in repos {
            projection.dir(repo.to_string());
        }
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }
}
