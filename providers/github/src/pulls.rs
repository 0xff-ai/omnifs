use omnifs_sdk::prelude::*;
use serde::Deserialize;

use crate::http_ext::{GithubHttpExt, github_check_status};
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

    /// Empty parent for `_q/prs`. Mirrors the issues query view: not
    /// enumerable, exists so lookup/listing of `prs` resolves cleanly
    /// under the shared `_q` namespace.
    #[dir("/{owner}/{repo}/_q/prs")]
    fn pr_q_root(_cx: &DirCx<'_, State>, _owner: OwnerName, _repo: RepoName) -> Result<Projection> {
        let mut projection = Projection::new();
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[dir("/{owner}/{repo}/_q/prs/{query}")]
    async fn pr_q_list(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        query: String,
    ) -> Result<Projection> {
        let page = numbered::list_query::<Pull>(cx, &owner, &repo, &query, "is:pr").await?;
        let mut projection = Projection::new();
        page.apply_status(&mut projection);
        for pr in page.items {
            let number = pr.number;
            let base = format!("{owner}/{repo}/_q/prs/{query}/{number}/");
            numbered::preload_common_fields(
                &mut projection,
                &base,
                pr.title,
                pr.body,
                pr.state,
                pr.user,
            );
            projection.dir(number.to_string());
        }
        Ok(projection)
    }

    #[dir("/{owner}/{repo}/_q/prs/{query}/{number}")]
    async fn pr_q_detail(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        _query: String,
        number: u64,
    ) -> Result<Projection> {
        pr_projection(cx, &owner, &repo, number).await
    }

    #[dir("/{owner}/{repo}/_q/prs/{query}/{number}/comments")]
    async fn pr_q_comments(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        _query: String,
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
    let page = numbered::list_hybrid::<Pull>(cx, owner, repo, filter).await?;
    let mut projection = Projection::new();
    page.apply_status(&mut projection);
    for pr in page.items {
        let number = pr.number;
        let base = format!("{owner}/{repo}/_prs/{}/{number}/", filter.as_ref());
        numbered::preload_common_fields(
            &mut projection,
            &base,
            pr.title,
            pr.body,
            pr.state,
            pr.user,
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
    let user_login = pr.user.map(|u| u.login).unwrap_or_default();
    let body = pr.body.unwrap_or_default();
    let summary = numbered::build_summary_markdown(
        "Pull request",
        Some(pr.number),
        &pr.title,
        &pr.state,
        &user_login,
        &body,
    );
    let mut projection = Projection::new();
    projection.file_with_content("title", pr.title);
    projection.file_with_content("body", body);
    projection.file_with_content("state", pr.state);
    projection.file_with_content("user", user_login);
    projection.file_with_content("summary.md", summary);
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
    let resp = cx
        .github_get(format!("/repos/{repo_id}/pulls/{number}"))
        .header("Accept", "application/vnd.github.diff")
        .send()
        .await?;
    Ok(FileContent::bytes(github_check_status(resp)?.into_body()))
}
