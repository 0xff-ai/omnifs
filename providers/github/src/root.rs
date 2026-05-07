use omnifs_sdk::prelude::*;

use crate::events::events_log_bytes;
use crate::owners::{fetch_owner_repos, resolve_owner_kind};
use crate::types::{OwnerName, RepoName};
use crate::{Result, State};

const AGENT_GUIDE: &str = include_str!("AGENT.md");

pub struct RootHandlers;

#[handlers]
impl RootHandlers {
    #[dir("/")]
    fn root(_cx: &DirCx<'_, State>) -> Result<Projection> {
        // Root is not enumerable: GitHub has no "list all visible owners"
        // call the provider could back this with. Users navigate by path.
        // The mount-rooted `AGENT.md` and `.events` files are auto-derived
        // into the listing from their literal route templates.
        Ok(Projection::new())
    }

    #[file("/AGENT.md")]
    fn agent_guide(_cx: &Cx<State>) -> Result<FileContent> {
        Ok(FileContent::bytes(AGENT_GUIDE.as_bytes()))
    }

    #[file("/.events")]
    fn events_log(cx: &Cx<State>) -> Result<FileContent> {
        Ok(FileContent::bytes(cx.state(events_log_bytes)))
    }

    #[dir("/{owner}")]
    async fn repos(cx: &DirCx<'_, State>, owner: OwnerName) -> Result<Projection> {
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
