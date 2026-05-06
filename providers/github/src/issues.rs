use omnifs_sdk::prelude::*;
use serde::Deserialize;

use crate::http_ext::GithubHttpExt;
use crate::numbered;
use crate::types::{OwnerName, RepoId, RepoName, StateFilter, User};
use crate::{Result, State};

#[derive(Clone, Debug, Deserialize)]
struct Issue {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    user: User,
}

pub struct IssueHandlers;

#[handlers]
impl IssueHandlers {
    #[dir("/{owner}/{repo}/_issues/{filter}")]
    async fn issue_list_open(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        filter: StateFilter,
    ) -> Result<Projection> {
        issue_list(cx, &owner, &repo, filter).await
    }

    #[dir("/{owner}/{repo}/_issues/{filter}/{number}")]
    async fn issue_open(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        _filter: StateFilter,
        number: u64,
    ) -> Result<Projection> {
        issue_projection(cx, &owner, &repo, number).await
    }

    #[dir("/{owner}/{repo}/_issues/{filter}/{number}/comments")]
    async fn issue_comments_open(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        _filter: StateFilter,
        number: u64,
    ) -> Result<Projection> {
        issue_comments_projection(cx, &owner, &repo, number).await
    }
}

async fn issue_list(
    cx: &Cx<State>,
    owner: &OwnerName,
    repo: &RepoName,
    filter: StateFilter,
) -> Result<Projection> {
    let page = numbered::search::<Issue>(cx, owner, repo, "issue", filter).await?;
    let mut projection = Projection::new();
    for issue in page.items {
        let base = format!(
            "{owner}/{repo}/_issues/{}/{}/",
            filter.as_ref(),
            issue.number
        );
        projection.preload(format!("{base}title"), issue.title);
        projection.preload(format!("{base}body"), issue.body.unwrap_or_default());
        projection.preload(format!("{base}state"), issue.state);
        projection.preload(format!("{base}user"), issue.user.login);
        projection.dir(issue.number.to_string());
    }
    if page.exhaustive {
        projection.page(PageStatus::Exhaustive);
    }
    Ok(projection)
}

async fn issue_projection(
    cx: &Cx<State>,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
) -> Result<Projection> {
    let repo_id = RepoId::new(owner, repo);
    let issue: Issue = cx
        .github_json(format!("/repos/{repo_id}/issues/{number}"))
        .await?;

    let mut projection = Projection::new();
    projection.file_with_content("title", issue.title);
    projection.file_with_content("body", issue.body.unwrap_or_default());
    projection.file_with_content("state", issue.state);
    projection.file_with_content("user", issue.user.login);
    projection.page(PageStatus::Exhaustive);
    Ok(projection)
}

async fn issue_comments_projection(
    cx: &DirCx<'_, State>,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
) -> Result<Projection> {
    numbered::comments_projection(cx, owner, repo, number, cx.intent()).await
}
