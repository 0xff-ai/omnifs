use omnifs_sdk::prelude::*;
use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::http_ext::GithubHttpExt;
use crate::numbered::{self, Listable};
use crate::types::{OwnerName, RepoId, RepoName, StateFilter, User};
use crate::{Result, State};

#[derive(Clone, Debug, Deserialize)]
struct Issue {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    user: Option<User>,
    pull_request: Option<IgnoredAny>,
}

impl Listable for Issue {
    const SEARCH_QUALIFIER: &'static str = "";
    const REST_RESOURCE: &'static str = "issues";

    fn id(&self) -> u64 {
        self.number
    }
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

    /// Empty parent for `_q/issues`. Queries are not enumerable; the
    /// handler exists so listing and lookup of the `issues` child
    /// resolve cleanly under the shared `_q` namespace.
    #[dir("/{owner}/{repo}/_q/issues")]
    fn issue_q_root(
        _cx: &DirCx<'_, State>,
        _owner: OwnerName,
        _repo: RepoName,
    ) -> Result<Projection> {
        let mut projection = Projection::new();
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[dir("/{owner}/{repo}/_q/issues/{query}")]
    async fn issue_q_list(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        query: String,
    ) -> Result<Projection> {
        let page = numbered::list_query::<Issue>(cx, &owner, &repo, &query, "is:issue").await?;
        let mut projection = Projection::new();
        page.apply_status(&mut projection);
        for item in page.items {
            // Search may still bleed PRs into a permissive query; this
            // route is the issues view so we drop them here.
            if item.pull_request.is_some() {
                continue;
            }
            let number = item.number;
            let base = format!("{owner}/{repo}/_q/issues/{query}/{number}/");
            numbered::preload_common_fields(
                &mut projection,
                &base,
                item.title,
                item.body,
                item.state,
                item.user,
            );
            projection.dir(number.to_string());
        }
        Ok(projection)
    }

    #[dir("/{owner}/{repo}/_q/issues/{query}/{number}")]
    async fn issue_q_detail(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        _query: String,
        number: u64,
    ) -> Result<Projection> {
        issue_projection(cx, &owner, &repo, number).await
    }

    #[dir("/{owner}/{repo}/_q/issues/{query}/{number}/comments")]
    async fn issue_q_comments(
        cx: &DirCx<'_, State>,
        owner: OwnerName,
        repo: RepoName,
        _query: String,
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
    let page = numbered::list_hybrid::<Issue>(cx, owner, repo, filter).await?;
    let mut projection = Projection::new();
    page.apply_status(&mut projection);
    ingest_issue_items(&mut projection, owner, repo, filter, page.items);
    Ok(projection)
}

fn ingest_issue_items(
    projection: &mut Projection,
    owner: &OwnerName,
    repo: &RepoName,
    filter: StateFilter,
    items: impl IntoIterator<Item = Issue>,
) {
    for item in items {
        if item.pull_request.is_some() {
            preload_pr_from_issue(projection, owner, repo, filter, item);
            continue;
        }
        let number = item.number;
        let base = format!("{owner}/{repo}/_issues/{}/{number}/", filter.as_ref());
        numbered::preload_common_fields(
            projection, &base, item.title, item.body, item.state, item.user,
        );
        projection.dir(number.to_string());
    }
}

fn preload_pr_from_issue(
    projection: &mut Projection,
    owner: &OwnerName,
    repo: &RepoName,
    filter: StateFilter,
    item: Issue,
) {
    let base = format!("{owner}/{repo}/_prs/{}/{}/", filter.as_ref(), item.number);
    projection.preload_dir(base.trim_end_matches('/'));
    numbered::preload_common_fields(
        projection, &base, item.title, item.body, item.state, item.user,
    );
    projection.preload_dir(format!("{base}comments").trim_end_matches('/'));
    projection.preload_entry(format!("{base}diff"), EntryKind::File, None);
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

    let user_login = issue.user.map(|u| u.login).unwrap_or_default();
    let body = issue.body.unwrap_or_default();
    let summary = numbered::build_summary_markdown(
        "Issue",
        Some(issue.number),
        &issue.title,
        &issue.state,
        &user_login,
        &body,
    );
    let mut projection = Projection::new();
    projection.file_with_content("title", issue.title);
    projection.file_with_content("body", body);
    projection.file_with_content("state", issue.state);
    projection.file_with_content("user", user_login);
    projection.file_with_content("summary.md", summary);
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
