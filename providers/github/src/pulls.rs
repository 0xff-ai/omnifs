use omnifs_sdk::prelude::*;
use serde::Deserialize;

use crate::http_ext::GithubHttpExt;
use crate::numbered::{self, Listable};
use crate::types::{OwnerName, RepoId, RepoName, StateFilter, User};
use crate::{Result, State};

#[derive(Clone, Debug, Deserialize)]
struct Pull {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    user: Option<User>,
    updated_at: Option<String>,
}

impl Listable for Pull {
    const SEARCH_QUALIFIER: &'static str = "+is:pr";
    const REST_RESOURCE: &'static str = "pulls";

    fn id(&self) -> u64 {
        self.number
    }
}

pub struct PullHandlers;

#[handlers]
impl PullHandlers {
    #[dir("/{owner}/{repo}/pulls/{filter}")]
    async fn pull_list_open(
        cx: &DirCx<State>,
        owner: OwnerName,
        repo: RepoName,
        filter: StateFilter,
    ) -> Result<Projection> {
        pr_list(cx, &owner, &repo, filter).await
    }

    #[dir("/{owner}/{repo}/pulls/{filter}/{number}")]
    async fn pr_open(
        cx: &DirCx<State>,
        owner: OwnerName,
        repo: RepoName,
        _filter: StateFilter,
        number: u64,
    ) -> Result<Projection> {
        pr_projection(cx, &owner, &repo, number).await
    }

    #[dir("/{owner}/{repo}/pulls/{filter}/{number}/comments")]
    async fn pr_comments_open(
        cx: &DirCx<State>,
        owner: OwnerName,
        repo: RepoName,
        _filter: StateFilter,
        number: u64,
    ) -> Result<Projection> {
        pr_comments_projection(cx, &owner, &repo, number).await
    }

    #[file("/{owner}/{repo}/pulls/open/{number}/diff")]
    async fn pr_diff_open(
        cx: &Cx<State>,
        owner: OwnerName,
        repo: RepoName,
        number: u64,
    ) -> Result<FileContent> {
        pr_diff_file(cx, &owner, &repo, number).await
    }

    #[file("/{owner}/{repo}/pulls/all/{number}/diff")]
    async fn pr_diff_all(
        cx: &Cx<State>,
        owner: OwnerName,
        repo: RepoName,
        number: u64,
    ) -> Result<FileContent> {
        pr_diff_file(cx, &owner, &repo, number).await
    }
}

async fn pr_list(
    cx: &Cx<State>,
    owner: &OwnerName,
    repo: &RepoName,
    filter: StateFilter,
) -> Result<Projection> {
    let page = numbered::list_hybrid::<Pull>(cx, owner, repo, filter).await?;
    let mut projection = Projection::new();
    page.apply_status(&mut projection);
    for pr in page.items {
        let version = pr.updated_at.clone();
        let number = pr.number;
        let base = format!("{owner}/{repo}/pulls/{}/{number}/", filter.as_ref());
        numbered::project_common_field_effects(
            &mut projection,
            &base,
            pr.title,
            pr.body,
            pr.state,
            pr.user,
            version.as_deref(),
        );
        projection.dir(number.to_string());
    }
    Ok(projection)
}

async fn pr_projection(
    cx: &Cx<State>,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
) -> Result<Projection> {
    let repo_id = RepoId::new(owner, repo);
    let pr: Pull = cx
        .github_json(format!("/repos/{repo_id}/pulls/{number}"))
        .await?;
    let mut projection = Projection::new();
    numbered::project_common_fields(
        &mut projection,
        pr.title,
        pr.body,
        pr.state,
        pr.user,
        pr.updated_at.as_deref(),
    );
    projection.page(PageStatus::Exhaustive);
    Ok(projection)
}

async fn pr_comments_projection(
    cx: &DirCx<State>,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
) -> Result<Projection> {
    numbered::comments_projection(cx, owner, repo, number, cx.intent()).await
}

async fn pr_diff_file(
    cx: &Cx<State>,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
) -> Result<FileContent> {
    let repo_id = RepoId::new(owner, repo);
    let blob = cx
        .github_get(format!("/repos/{repo_id}/pulls/{number}"))
        .header("Accept", "application/vnd.github.diff")
        .into_blob()
        .with_cache_key(format!("github/pulls/{repo_id}/{number}/diff"))
        .send()
        .await?
        .error_for_status()?;
    Ok(FileContent::blob_with_attrs(
        FileAttrs::new(Size::Exact(blob.size), Stability::Mutable),
        blob.id(),
    ))
}
