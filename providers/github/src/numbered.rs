use hashbrown::HashSet;
use omnifs_sdk::Cx;
use omnifs_sdk::prelude::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::fmt::Write as _;

use crate::http_ext::GithubHttpExt;
use crate::types::{OwnerName, RepoName, StateFilter, User};
use crate::{Result, State};

pub(crate) const COMMENT_PAGE_SIZE: u64 = 100;

const PAGE_SIZE: u64 = 100;
const SEARCH_RESULT_CAP: u64 = 1000;

/// Listable resources that share GitHub's issue-shaped JSON: real issues
/// and pull requests. The associated constants pick which Search query
/// qualifier and REST resource path apply to `T`, keeping URL choice
/// bound to the type used to deserialize the response.
pub(crate) trait Listable: DeserializeOwned {
    /// Extra qualifier appended to the Search `q=repo:owner/repo`
    /// expression (e.g. `+is:pr`). Empty for resources that include
    /// every issue and PR.
    const SEARCH_QUALIFIER: &'static str;
    /// REST resource segment under `/repos/{owner}/{repo}/` used for
    /// pages 2..N.
    const REST_RESOURCE: &'static str;
    /// Stable identity used to dedupe at the search/REST seam. Items
    /// created or deleted between calls can shift REST's offset by one
    /// and re-emit (or drop) the boundary item.
    fn id(&self) -> u64;
}

#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct SearchResults<T> {
    #[serde(default)]
    total_count: u64,
    #[serde(default)]
    items: Vec<T>,
}

pub(crate) struct ListPage<T> {
    pub(crate) items: Vec<T>,
    pub(crate) exhaustive: bool,
}

impl<T> ListPage<T> {
    pub(crate) fn apply_status(&self, projection: &mut Projection) {
        if self.exhaustive {
            projection.page(PageStatus::Exhaustive);
        } else {
            // List handlers don't honor resume tokens, but the SDK only
            // models partial listings via `More(_)`. Use an opaque
            // sentinel so the cursor can't masquerade as a page number.
            projection.page(PageStatus::More(Cursor::Opaque("capped".into())));
        }
    }
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

/// Run a single Search API page against an arbitrary, agent-supplied
/// qualifier captured as a path segment (the `_issues/q/{query}` and
/// `_prs/q/{query}` routes). The `kind_qualifier` arg pins the result
/// to issues or PRs so the route name keeps its meaning even when the
/// user's query is ambiguous.
///
/// Capped at one Search page (`PAGE_SIZE`); the listing is marked
/// non-exhaustive when `total_count` exceeds that. We deliberately do
/// not wire REST pagination here: queries trade scrolling depth for
/// arbitrary filter expressivity, and a single Search page is what
/// makes the path-segment query encoding worthwhile.
pub(crate) async fn list_query<T: Listable>(
    cx: &Cx<State>,
    owner: &OwnerName,
    repo: &RepoName,
    query: &str,
    kind_qualifier: &str,
) -> Result<ListPage<T>> {
    let mut q = format!("repo:{owner}/{repo}");
    if !kind_qualifier.is_empty() {
        q.push('+');
        q.push_str(kind_qualifier);
    }
    let trimmed = query.trim_matches('+');
    if !trimmed.is_empty() {
        q.push('+');
        q.push_str(trimmed);
    }
    let search_path = format!("/search/issues?q={q}&sort=created&order=desc&per_page={PAGE_SIZE}");
    let response: SearchResults<T> = cx.github_json(&search_path).await?;
    let exhaustive = response.total_count <= PAGE_SIZE;
    Ok(ListPage {
        items: response.items,
        exhaustive,
    })
}

/// Search supplies `total_count` so we can size the rest of the work
/// without parsing Link headers; REST handles pages 2..N in parallel.
/// Capped at the Search API's 1000-item ceiling.
pub(crate) async fn list_hybrid<T: Listable>(
    cx: &Cx<State>,
    owner: &OwnerName,
    repo: &RepoName,
    filter: StateFilter,
) -> Result<ListPage<T>> {
    let search_state_clause = match filter {
        StateFilter::Open => "+state:open",
        StateFilter::All => "",
    };
    let rest_state = match filter {
        StateFilter::Open => "open",
        StateFilter::All => "all",
    };
    let qualifier = T::SEARCH_QUALIFIER;
    let resource = T::REST_RESOURCE;
    let search_path = format!(
        "/search/issues?q=repo:{owner}/{repo}{qualifier}{search_state_clause}\
         &sort=created&order=desc&per_page={PAGE_SIZE}"
    );
    let rest_path = format!(
        "/repos/{owner}/{repo}/{resource}\
         ?state={rest_state}&sort=created&direction=desc&per_page={PAGE_SIZE}"
    );

    let first: SearchResults<T> = cx.github_json(&search_path).await?;
    let capped_total = first.total_count.min(SEARCH_RESULT_CAP);
    let page_count = capped_total.div_ceil(PAGE_SIZE);
    let mut items = first.items;
    items.reserve((capped_total as usize).saturating_sub(items.len()));

    if page_count > 1 {
        let rest_requests = (2..=page_count)
            .map(|page| cx.github_json::<Vec<T>>(format!("{rest_path}&page={page}")));
        for page in join_all(rest_requests).await {
            items.extend(page?);
        }
        let mut seen = HashSet::with_capacity(items.len());
        items.retain(|item| seen.insert(item.id()));
    }

    Ok(ListPage {
        items,
        exhaustive: first.total_count <= SEARCH_RESULT_CAP,
    })
}

/// Maximum body excerpt size, in bytes, included in `summary.md`. Longer
/// bodies are truncated with an ellipsis line so the summary stays
/// roughly token-bounded for an agent's first read.
const SUMMARY_BODY_EXCERPT_BYTES: usize = 1024;

/// Preload the projected files every numbered resource exposes: the
/// four primitive fields plus a bundled `summary.md` for cheap "what
/// is this?" reads. `base` must end in `/` so callers can append the
/// file name.
pub(crate) fn preload_common_fields(
    projection: &mut Projection,
    base: &str,
    title: String,
    body: Option<String>,
    state: String,
    user: Option<User>,
) {
    let kind_label = numbered_kind_label_from_base(base);
    let number = numbered_id_from_base(base);
    let user_login = user.map(|u| u.login).unwrap_or_default();
    let body_str = body.unwrap_or_default();
    let summary =
        build_summary_markdown(kind_label, number, &title, &state, &user_login, &body_str);
    projection.preload(format!("{base}title"), title);
    projection.preload(format!("{base}body"), body_str);
    projection.preload(format!("{base}state"), state);
    projection.preload(format!("{base}user"), user_login);
    projection.preload(format!("{base}summary.md"), summary);
}

/// Render the `summary.md` bundle that each issue/PR projects alongside
/// the primitive sibling files. Format is intentionally short and
/// deterministic so the agent gets a stable answer in ~300 tokens.
pub(crate) fn build_summary_markdown(
    kind_label: &str,
    number: Option<u64>,
    title: &str,
    state: &str,
    user_login: &str,
    body: &str,
) -> String {
    let header = number.map_or_else(
        || format!("# {kind_label}: {title}\n"),
        |n| format!("# {kind_label} #{n}: {title}\n"),
    );
    let mut out = String::with_capacity(header.len() + body.len().min(SUMMARY_BODY_EXCERPT_BYTES));
    out.push_str(&header);
    out.push('\n');
    let user_field = if user_login.is_empty() {
        "(unknown)"
    } else {
        user_login
    };
    let _ = writeln!(out, "state: {state}      author: {user_field}\n");
    if body.is_empty() {
        out.push_str("(no body)\n");
    } else if body.len() <= SUMMARY_BODY_EXCERPT_BYTES {
        out.push_str(body.trim_end());
        out.push('\n');
    } else {
        // Truncate at a UTF-8 boundary: walk back from the byte cap
        // until we land on a character break, then append an ellipsis.
        let mut cut = SUMMARY_BODY_EXCERPT_BYTES;
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push_str(body[..cut].trim_end());
        out.push_str("\n…\n");
    }
    out
}

/// Extract the numeric ID from a numbered-resource base path, e.g.
/// `"raulk/omnifs/_issues/_open/123/"` → `Some(123)`. Used by the
/// summary builder so the rendered header shows `#N:`.
fn numbered_id_from_base(base: &str) -> Option<u64> {
    base.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

/// Choose the human-readable kind label for the rendered summary
/// header from the path family, so issues say "Issue" and PRs say
/// "Pull request" without callers having to thread the label through.
fn numbered_kind_label_from_base(base: &str) -> &'static str {
    if base.contains("/_prs/") {
        "Pull request"
    } else {
        "Issue"
    }
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
