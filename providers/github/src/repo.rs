use omnifs_sdk::prelude::*;

use crate::types::{OwnerName, RepoId, RepoName};
use crate::{Result, State};

pub struct RepoHandlers;

#[handlers]
impl RepoHandlers {
    #[dir("/{owner}/{repo}")]
    fn repo(_owner: OwnerName, _repo: RepoName) -> Result<Projection> {
        let mut projection = Projection::new();
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[treeref("/{owner}/{repo}/repo")]
    async fn repo_tree(cx: &Cx<State>, owner: OwnerName, repo: RepoName) -> Result<TreeRef> {
        let repo_id = RepoId::new(&owner, &repo);
        let repo = cx
            .git()
            .open_repo(
                format!("github.com/{repo_id}"),
                format!("git@github.com:{repo_id}.git"),
            )
            .await?;
        Ok(TreeRef::new(repo.tree))
    }

    #[dir("/{owner}/{repo}/issues")]
    fn issues(_owner: OwnerName, _repo: RepoName) -> Result<Projection> {
        let mut projection = Projection::new();
        projection.dir("all");
        projection.dir("open");
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[dir("/{owner}/{repo}/pulls")]
    fn prs(_owner: OwnerName, _repo: RepoName) -> Result<Projection> {
        let mut projection = Projection::new();
        projection.dir("all");
        projection.dir("open");
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[dir("/{owner}/{repo}/actions")]
    fn actions(_owner: OwnerName, _repo: RepoName) -> Result<Projection> {
        let mut projection = Projection::new();
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }
}
