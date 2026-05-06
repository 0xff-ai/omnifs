use omnifs_sdk::prelude::*;
use serde::Deserialize;

use crate::http_ext::GithubHttpExt;
use crate::numbered;
use crate::types::{OwnerName, RepoId, RepoName, StateFilter, User};
use crate::{Result, State};

#[derive(Clone, Debug, Deserialize)]
struct Pull {
    number: u64,
    title: String,
    body: Option<String>,
    state: Option<String>,
    user: Option<User>,
}

pub struct PullHandlers;

#[handlers]
impl PullHandlers {
    #[dir("/{owner}/{repo}/_prs/{filter}")]
    async fn pull_list_open(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        filter: StateFilter,
    ) -> Result<Projection> {
        pr_list(cx, &owner, &repo, filter).await
    }

    #[dir("/{owner}/{repo}/_prs/{filter}/{number}")]
    async fn pr_open(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        _filter: StateFilter,
        number: u64,
    ) -> Result<Projection> {
        pr_projection(cx, &owner, &repo, number).await
    }

    #[dir("/{owner}/{repo}/_prs/{filter}/{number}/comments")]
    async fn pr_comments_open(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        _filter: StateFilter,
        number: u64,
    ) -> Result<Projection> {
        pr_comments_projection(cx, &owner, &repo, number).await
    }

    #[file("/{owner}/{repo}/_prs/_open/{number}/diff")]
    async fn pr_diff_open(
        cx: &Cx<State>,
        owner: OwnerName,
        repo: RepoName,
        number: u64,
    ) -> Result<FileContent> {
        pr_diff_file(cx, &owner, &repo, number).await
    }

    #[file("/{owner}/{repo}/_prs/_all/{number}/diff")]
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
    let page = numbered::search::<Pull>(cx, owner, repo, "pr", filter).await?;
    let mut projection = Projection::new();
    for pr in page.items {
        let base = format!("{owner}/{repo}/_prs/{}/{}/", filter.as_ref(), pr.number);
        projection.preload(format!("{base}title"), pr.title);
        projection.preload(format!("{base}body"), pr.body.unwrap_or_default());
        projection.preload(format!("{base}state"), pr.state.unwrap_or_default());
        projection.preload(
            format!("{base}user"),
            pr.user.map(|u| u.login).unwrap_or_default(),
        );
        projection.dir(pr.number.to_string());
    }
    if page.exhaustive {
        projection.page(PageStatus::Exhaustive);
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
    projection.file_with_content("title", pr.title);
    projection.file_with_content("body", pr.body.unwrap_or_default());
    projection.file_with_content("state", pr.state.unwrap_or_default());
    projection.file_with_content("user", pr.user.map(|u| u.login).unwrap_or_default());
    projection.page(PageStatus::Exhaustive);
    Ok(projection)
}

async fn pr_comments_projection(
    cx: &DirCx<'_, State>,
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
    let diff = cx
        .github_get(format!("/repos/{repo_id}/pulls/{number}"))
        .header("Accept", "application/vnd.github.diff")
        .send_body()
        .await?;
    Ok(FileContent::bytes(diff))
}
