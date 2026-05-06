use omnifs_sdk::Cx;
use omnifs_sdk::prelude::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::http_ext::GithubHttpExt;
use crate::types::{OwnerName, RepoName, StateFilter};
use crate::{Result, State};

pub(crate) const COMMENT_PAGE_SIZE: u64 = 100;

#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct SearchResults<T> {
    #[serde(default)]
    total_count: u64,
    #[serde(default)]
    items: Vec<T>,
}

pub(crate) struct SearchPage<T> {
    pub(crate) items: Vec<T>,
    pub(crate) exhaustive: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CommentRecord {
    pub(crate) user: CommentUser,
    pub(crate) body: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CommentUser {
    pub(crate) login: String,
}

pub(crate) async fn search<T>(
    cx: &Cx<State>,
    owner: &OwnerName,
    repo: &RepoName,
    kind: &str,
    filter: StateFilter,
) -> Result<SearchPage<T>>
where
    T: DeserializeOwned,
{
    let state_clause = match filter {
        StateFilter::Open => "+state:open",
        StateFilter::All => "",
    };
    let query = format!("repo:{owner}/{repo}+is:{kind}{state_clause}");
    let base_path = format!("/search/issues?q={query}&sort=created&order=desc&per_page=100");

    let first_page: SearchResults<T> = cx.github_json(&base_path).await?;

    let capped_total = first_page.total_count.min(1000);
    let page_count = capped_total.div_ceil(100);
    let mut items = first_page.items;

    for page in 2..=page_count {
        let page_results: SearchResults<T> =
            cx.github_json(format!("{base_path}&page={page}")).await?;
        items.extend(page_results.items);
    }

    Ok(SearchPage {
        items,
        exhaustive: first_page.total_count <= 1000,
    })
}

pub(crate) async fn comments_projection(
    cx: &Cx<State>,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
    intent: &DirIntent<'_>,
) -> Result<Projection> {
    match intent {
        DirIntent::ReadProjectedFile { name } => {
            let idx = name
                .parse::<u64>()
                .map_err(|_| ProviderError::not_found("comment not found"))?;
            if idx == 0 {
                return Err(ProviderError::not_found("comments are 1-indexed"));
            }
            let page = ((idx - 1) / COMMENT_PAGE_SIZE) + 1;
            let offset = ((idx - 1) % COMMENT_PAGE_SIZE) as usize;
            let comments: Vec<CommentRecord> = cx
                .github_json(format!(
                    "/repos/{owner}/{repo}/issues/{number}/comments?per_page={COMMENT_PAGE_SIZE}&page={page}"
                ))
                .await?;
            let comment = comments
                .get(offset)
                .ok_or_else(|| ProviderError::not_found("comment not found"))?;
            let body = comment.body.as_deref().unwrap_or("");
            let mut projection = Projection::new();
            projection.file_with_content(
                (*name).to_string(),
                format!("{}:\n{body}\n", comment.user.login),
            );
            Ok(projection)
        },
        DirIntent::Lookup { .. } | DirIntent::List { .. } => {
            let comments: Vec<CommentRecord> = cx
                .github_json(format!(
                    "/repos/{owner}/{repo}/issues/{number}/comments?per_page={COMMENT_PAGE_SIZE}&page=1"
                ))
                .await?;
            let mut projection = Projection::new();
            for idx in 1..=comments.len() {
                projection.file(idx.to_string());
            }
            let exhaustive = u64::try_from(comments.len()).unwrap_or(u64::MAX) < COMMENT_PAGE_SIZE;
            if exhaustive {
                projection.page(PageStatus::Exhaustive);
            } else {
                projection.page(PageStatus::More(Cursor::Page(2)));
            }
            Ok(projection)
        },
    }
}
