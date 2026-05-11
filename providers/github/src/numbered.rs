use hashbrown::HashSet;
use omnifs_sdk::Cx;
use omnifs_sdk::prelude::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;

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

/// Preload the projected files every numbered resource exposes.
///
/// List responses can include many large issue/PR bodies, so `body` is
/// projected as deferred metadata and fetched from the single resource
/// handler on read.
/// `base` must end in `/` so callers can append the file name.
pub(crate) fn preload_common_fields(
    projection: &mut Projection,
    base: &str,
    title: String,
    _body: Option<String>,
    state: String,
    user: Option<User>,
    version: Option<&str>,
) {
    mutable_preload(projection, format!("{base}title"), title, version);
    projection.preload_entry(
        format!("{base}body"),
        EntryKind::File,
        Some(mutable_deferred_attrs(Size::Unknown, version)),
    );
    mutable_preload(projection, format!("{base}state"), state, version);
    mutable_preload(
        projection,
        format!("{base}user"),
        user.map(|u| u.login).unwrap_or_default(),
        version,
    );
}

pub(crate) fn project_common_fields(
    projection: &mut Projection,
    title: String,
    body: Option<String>,
    state: String,
    user: Option<User>,
    version: Option<&str>,
) {
    projection.file_with_content_attrs("title", title, Stability::Mutable, version_token(version));
    projection.file_with_content_attrs(
        "body",
        body.unwrap_or_default(),
        Stability::Mutable,
        version_token(version),
    );
    projection.file_with_content_attrs("state", state, Stability::Mutable, version_token(version));
    projection.file_with_content_attrs(
        "user",
        user.map(|u| u.login).unwrap_or_default(),
        Stability::Mutable,
        version_token(version),
    );
}

pub(crate) fn mutable_file_content(
    bytes: impl Into<Vec<u8>>,
    version: Option<&str>,
) -> FileContent {
    let bytes = bytes.into();
    let size = Size::Exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    FileContent::bytes_with_attrs(mutable_deferred_attrs(size, version), bytes)
}

pub(crate) fn mutable_deferred_attrs(size: Size, version: Option<&str>) -> FileAttrs {
    let attrs = FileAttrs::deferred(size, ReadMode::Full, Stability::Mutable);
    match version.filter(|version| !version.is_empty()) {
        Some(version) => attrs.with_version(version),
        None => attrs,
    }
}

fn mutable_preload(
    projection: &mut Projection,
    path: impl Into<String>,
    content: impl Into<Vec<u8>>,
    version: Option<&str>,
) {
    let content = content.into();
    let size = Size::Exact(u64::try_from(content.len()).unwrap_or(u64::MAX));
    projection.preload_with_attrs(path, mutable_deferred_attrs(size, version), content);
}

fn version_token(version: Option<&str>) -> Option<VersionToken> {
    version
        .filter(|version| !version.is_empty())
        .map(VersionToken::from)
}

pub(crate) async fn comments_projection(
    cx: &Cx<State>,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
    intent: &DirIntent,
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
            projection
                .file_with_content(name.clone(), format!("{}:\n{body}\n", comment.user.login));
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
                projection.deferred_file(idx.to_string());
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
