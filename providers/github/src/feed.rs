//! The feeds of the issues, pull requests, and comments of a repository.
//!
//! The feed of a repository takes its first full listing a part for each of
//! its requests: the rows of its issues and pull requests by `updated_at`,
//! then its comments, then the GitHub GraphQL counts of the comments of each
//! item, with the IDs of an item with another count. After it, each answer is
//! a check of the changes since the time of its last row, a part for each of
//! its requests. Before the first full listing ends, the feed of an item
//! answers for that item from `GET /repos/{owner}/{repo}/issues/{number}`, a
//! part for each of its requests. Each comment that the IDs of an item bring
//! comes by its ID in a part of its own.
//!
//! The facts of an issue or a pull request live once, under its folder in
//! `all/`, which its entry in `open/` shares while it is open.

use std::collections::{BTreeMap, BTreeSet};

use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

use crate::api::GitHubApi;
use crate::item::{IssueKey, ItemKind, PullKey, RepoKey};
use crate::objects::{Comment, Issue, ItemData, PullRequest, Repo};
use crate::{OwnerName, RepoId, RepoName, StateFilter};

/// The rows, comments, and GraphQL nodes of a page.
const PAGE: usize = 100;

// ===========================================================================
// Tokens
// ===========================================================================

/// The token of the feed of a repository: the `updated_at` of its last row,
/// the `ETag` of the last request for its changes since that time, and, while
/// its first full listing or a check is unfinished, its stage.
#[derive(Serialize, Deserialize)]
struct RepoToken {
    #[serde(with = "time::serde::rfc3339::option")]
    time: Option<OffsetDateTime>,
    etag: Option<String>,
    stage: Option<Stage>,
}

/// The stage of an unfinished first full listing or check of a repository.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    /// The rows of the first full listing, at their link `next`.
    Rows { next: String },
    /// The comments of the first full listing, at a page.
    Comments { page: usize },
    /// The counts of the first full listing, at the cursor of a connection.
    Counts {
        connection: ItemKind,
        cursor: Option<String>,
    },
    /// The IDs of the items of a page of the counts with another count than
    /// their `comments/`, an item at a time from the first: the page of its
    /// ids after `after`, none for its first page, with the ids before it;
    /// then the stage `then`, none at the end of the first full listing.
    Ids {
        kind: ItemKind,
        items: BTreeSet<u64>,
        after: Option<String>,
        seen: BTreeSet<u64>,
        then: Option<Box<Stage>>,
    },
    /// The comments of an item that its ids brought, by their IDs, then the
    /// stage `then`, none at the end of the first full listing.
    Fetch {
        item: ItemRef,
        ids: BTreeSet<u64>,
        then: Option<Box<Stage>>,
    },
    /// A check, at its next request.
    Check(Box<Check>),
}

/// An unfinished check: its next request, the time of its last row and the
/// `ETag` of its first page, each changed item by its number, the counts of
/// GitHub GraphQL, and the items that GitHub GraphQL removed.
#[derive(Serialize, Deserialize)]
struct Check {
    next: CheckRequest,
    #[serde(with = "time::serde::rfc3339::option")]
    time: Option<OffsetDateTime>,
    etag: Option<String>,
    changed: BTreeMap<u64, Changed>,
    totals: Option<Totals>,
    removed: BTreeSet<ItemRef>,
}

/// A changed item of a check: its kind, the count `comments` of its row, the
/// IDs of the comments that the list of the check brought, whether its IDs
/// are taken, and, while they page, their next page.
#[derive(Serialize, Deserialize)]
struct Changed {
    kind: ItemKind,
    comments: usize,
    brought: BTreeSet<u64>,
    taken: bool,
    ids: Option<IdsPage>,
}

/// The cursor of the next page of the ids of an item, with the ids before it.
#[derive(Serialize, Deserialize)]
struct IdsPage {
    after: String,
    seen: BTreeSet<u64>,
}

/// The counts `totalCount` of the issues and of the pull requests.
#[derive(Clone, Copy, Serialize, Deserialize)]
struct Totals {
    issues: usize,
    pulls: usize,
}

/// The next request of a check.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckRequest {
    /// The page of rows at a link `next`.
    Rows { next: String },
    /// A page of the comments since the time of the check.
    Comments { page: usize },
    /// The `POST` of GitHub GraphQL with the counts, while the check lacks
    /// them, and the first page of the ids of the next changed items.
    Counts,
    /// The next page of the ids of a changed item.
    Ids { number: u64 },
    /// The comments that the ids of the changed items brought, by their IDs
    /// under the numbers of their items.
    Fetch {
        comments: BTreeMap<u64, BTreeSet<u64>>,
    },
    /// A page of the numbers of a kind, with the numbers before it.
    Numbers {
        kind: ItemKind,
        after: Option<String>,
        numbers: BTreeSet<u64>,
    },
}

/// The token of the feed of an item: the `ETag` of its last answer, none for
/// an item from a page of rows, its `updated_at`, its count `comments`, and
/// the next request of an unfinished answer. The token of a removed item is
/// `null`.
#[derive(Serialize, Deserialize)]
struct ItemToken {
    etag: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    comments: usize,
    next: Option<ItemRequest>,
}

/// The next request of the comments of an item, with the IDs of the comments
/// that its lists brought.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ItemRequest {
    /// A page of its comments updated since a time.
    Since {
        #[serde(with = "time::serde::rfc3339")]
        since: OffsetDateTime,
        page: usize,
        brought: BTreeSet<u64>,
    },
    /// A page of all of its comments.
    Comments { page: usize, brought: BTreeSet<u64> },
    /// A page of its ids, with the time a comment is taken again from, and
    /// the ids before it.
    Ids {
        #[serde(with = "time::serde::rfc3339")]
        time: OffsetDateTime,
        after: Option<String>,
        brought: BTreeSet<u64>,
        seen: BTreeSet<u64>,
    },
    /// The comments that its ids brought, by their IDs, then the next page
    /// of its ids; none after their last page.
    Fetch {
        ids: BTreeSet<u64>,
        then: Option<Box<ItemRequest>>,
    },
}

/// An issue or a pull request of the repository of a feed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct ItemRef {
    kind: ItemKind,
    number: u64,
}

fn encode<T: Serialize>(token: &T) -> Result<String> {
    serde_json::to_string(token)
        .map_err(|error| ProviderError::internal(format!("feed token: {error}")))
}

fn decode<T: DeserializeOwned>(token: &str) -> Result<T> {
    serde_json::from_str(token)
        .map_err(|error| ProviderError::invalid_input(format!("feed token: {error}")))
}

// ===========================================================================
// Rows
// ===========================================================================

/// A row of `GET /repos/{owner}/{repo}/issues`, or the answer of
/// `GET /repos/{owner}/{repo}/issues/{number}`.
#[derive(Deserialize)]
struct ItemRow {
    #[serde(flatten)]
    data: ItemData,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    comments: usize,
}

impl ItemRow {
    fn item(&self) -> ItemRef {
        ItemRef {
            kind: if self.data.is_pull_request() {
                ItemKind::Pulls
            } else {
                ItemKind::Issues
            },
            number: self.data.number,
        }
    }

    fn token(&self, etag: Option<String>) -> ItemToken {
        ItemToken {
            etag,
            updated_at: self.updated_at,
            comments: self.comments,
            next: None,
        }
    }
}

/// A row of `GET /repos/{owner}/{repo}/issues/comments`.
#[derive(Deserialize)]
struct CommentRow {
    #[serde(flatten)]
    comment: Comment,
    html_url: String,
}

impl CommentRow {
    /// The item of the comment, from the kind and the number of its
    /// `html_url`, `https://github.com/{owner}/{repo}/{issues|pull}/{number}#issuecomment-{id}`.
    fn item(&self) -> Result<ItemRef> {
        let invalid = || ProviderError::invalid_input(format!("comment at {}", self.html_url));
        let (path, _) = self.html_url.split_once('#').ok_or_else(invalid)?;
        let (rest, number) = path.rsplit_once('/').ok_or_else(invalid)?;
        let kind = match rest.rsplit_once('/').ok_or_else(invalid)?.1 {
            "issues" => ItemKind::Issues,
            "pull" => ItemKind::Pulls,
            _ => return Err(invalid()),
        };
        Ok(ItemRef {
            kind,
            number: number.parse().map_err(|_| invalid())?,
        })
    }
}

/// A page of rows: the rows, the `ETag` of their answer, and their link
/// `next` as a path of the API.
struct RowPage {
    rows: Vec<ItemRow>,
    etag: Option<String>,
    next: Option<String>,
}

impl RowPage {
    /// The page of an answer, `None` for `304`.
    fn of(answer: PageAnswer<Vec<ItemRow>>) -> Result<Option<Self>> {
        let next = next_link(&answer)?;
        match answer {
            PageAnswer::Unchanged { .. } => Ok(None),
            PageAnswer::Fresh {
                value, validator, ..
            } => Ok(Some(Self {
                rows: value,
                etag: validator.map(|etag| etag.0),
                next,
            })),
        }
    }
}

/// The link `next` of the header `Link` of an answer, as a path of the API.
fn next_link<T>(answer: &PageAnswer<T>) -> Result<Option<String>> {
    let Some(url) = answer.header("link").and_then(|links| {
        links.split(',').find_map(|link| {
            let (url, params) = link.split_once(';')?;
            params
                .split(';')
                .any(|param| param.trim() == r#"rel="next""#)
                .then(|| url.trim().trim_start_matches('<').trim_end_matches('>'))
        })
    }) else {
        return Ok(None);
    };
    url.strip_prefix(GitHubApi.base())
        .map(|path| Some(path.to_owned()))
        .ok_or_else(|| ProviderError::invalid_input(format!("link next {url} leaves the API")))
}

/// The answer of `GET /repos/{owner}/{repo}/issues/{number}`.
enum ItemAnswer {
    /// `304`: the item of the token stands.
    Unchanged,
    /// `301`, `404`, or `410`: the item left the repository.
    Removed,
    /// `200`: the item, with the `ETag` of the answer.
    Found(ItemRow, Option<String>),
}

/// Ask for an item, with `If-None-Match` and the `ETag` of its token, or
/// with `If-Modified-Since` and the HTTP date of the `updated_at` of a token
/// without an `ETag`.
async fn item_answer<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    token: Option<&ItemToken>,
) -> Result<ItemAnswer> {
    let request = cx
        .endpoint(GitHubApi)
        .get(format!("/repos/{}/issues/{}", repo.id, item.number));
    let request = match token {
        None => request,
        Some(ItemToken {
            etag: Some(etag), ..
        }) => request.header("If-None-Match", etag.as_str()),
        Some(ItemToken {
            etag: None,
            updated_at,
            ..
        }) => request.header("If-Modified-Since", http_date(*updated_at)?),
    };
    let response = request.send().await?;
    match response.status() {
        200 => Ok(ItemAnswer::Found(
            serde_json::from_slice(response.body())
                .map_err(|error| ProviderError::invalid_input(format!("json decode: {error}")))?,
            response.header("etag").map(ToOwned::to_owned),
        )),
        304 => Ok(ItemAnswer::Unchanged),
        301 | 404 | 410 => Ok(ItemAnswer::Removed),
        status => Err(ProviderError::from_http_status(status)),
    }
}

/// A page of the comments of an item updated since a time.
async fn item_comments<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    since: Option<OffsetDateTime>,
    page: usize,
) -> Result<Vec<Comment>> {
    cx.endpoint(GitHubApi)
        .get(format!(
            "/repos/{}/issues/{}/comments?per_page={PAGE}{}&page={page}",
            repo.id,
            item.number,
            since_pair(since)?
        ))
        .json()
        .await
}

/// The query pair `since` of a time, none without a time.
fn since_pair(time: Option<OffsetDateTime>) -> Result<String> {
    match time {
        Some(time) => Ok(format!("&since={}", rfc3339(time)?)),
        None => Ok(String::new()),
    }
}

fn rfc3339(time: OffsetDateTime) -> Result<String> {
    time.format(&Rfc3339)
        .map_err(|error| ProviderError::internal(format!("time {time}: {error}")))
}

fn http_date(time: OffsetDateTime) -> Result<String> {
    time.to_offset(UtcOffset::UTC)
        .format(format_description!(
            "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
        ))
        .map_err(|error| ProviderError::internal(format!("time {time}: {error}")))
}

// ===========================================================================
// GitHub GraphQL
// ===========================================================================

/// An answer of GitHub GraphQL: its data, with its errors.
#[derive(Deserialize)]
struct GraphQl<T> {
    data: Option<GraphQlData<T>>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Deserialize)]
struct GraphQlError {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: String,
}

/// The data of an answer of GitHub GraphQL about a repository.
#[derive(Deserialize)]
struct GraphQlData<T> {
    repository: T,
}

/// A page of a connection under the alias `page`.
#[derive(Deserialize)]
struct Paged<T> {
    page: Connection<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    nodes: Vec<T>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

impl PageInfo {
    /// The cursor of the next page, `None` after the last page.
    fn next(self) -> Result<Option<String>> {
        if !self.has_next_page {
            return Ok(None);
        }
        self.end_cursor.map(Some).ok_or_else(|| {
            ProviderError::invalid_input("a GraphQL page before another has no cursor")
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Total {
    total_count: usize,
}

/// An item with the count of its comments.
#[derive(Deserialize)]
struct Counted {
    number: u64,
    comments: Total,
}

#[derive(Deserialize)]
struct Numbered {
    number: u64,
}

/// The id of a comment, with the time of its last update.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentId {
    database_id: u64,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

/// An item with a page of the ids of its comments.
#[derive(Deserialize)]
struct Commented {
    comments: Connection<CommentId>,
}

/// The answer of the query of the ids of an item, `null` for an item that
/// left the repository.
#[derive(Deserialize)]
struct Item {
    item: Option<Commented>,
}

/// The answer of the query of a check: the counts of the issues and of the
/// pull requests, and each changed item under its alias, `null` for an item
/// that left the repository.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Counts {
    issues: Total,
    pull_requests: Total,
    #[serde(flatten)]
    items: BTreeMap<String, Option<Commented>>,
}

impl ItemKind {
    /// The GraphQL connection of the items of the kind.
    const fn connection(self) -> &'static str {
        match self {
            Self::Issues => "issues",
            Self::Pulls => "pullRequests",
        }
    }
}

/// Ask GitHub GraphQL `query` about the repository, with `variables` and the
/// repository as `$owner` and `$name`. An item that GitHub GraphQL cannot
/// resolve, an error of the type `NOT_FOUND`, is `null` in the data; an
/// answer with another error fails, as rate limited, with the window of its
/// headers, for the type `RATE_LIMITED`.
async fn graphql<T: DeserializeOwned>(
    cx: &Cx,
    repo: &Repository,
    query: &str,
    mut variables: serde_json::Value,
) -> Result<T> {
    variables["owner"] = repo.id.owner.to_string().into();
    variables["name"] = repo.id.repo.to_string().into();
    let response = cx
        .endpoint(GitHubApi)
        .post("/graphql")
        .body_json(&json!({ "query": query, "variables": variables }))
        .send()
        .await?;
    if response.status() != 200 {
        return Err(ProviderError::from_http_status(response.status()));
    }
    let answer: GraphQl<T> = serde_json::from_slice(response.body())
        .map_err(|error| ProviderError::invalid_input(format!("GitHub GraphQL: {error}")))?;
    let errors = answer.errors.iter().flatten().collect::<Vec<_>>();
    let message = || {
        format!(
            "GitHub GraphQL: {}",
            errors
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )
    };
    if errors
        .iter()
        .any(|error| error.kind.as_deref() == Some("RATE_LIMITED"))
    {
        return Err(ProviderError::rate_limited(message())
            .with_retry_after(crate::api::retry_window(response.headers())?));
    }
    if errors
        .iter()
        .any(|error| error.kind.as_deref() != Some("NOT_FOUND"))
    {
        return Err(ProviderError::internal(message()));
    }
    answer
        .data
        .map(|data| data.repository)
        .ok_or_else(|| ProviderError::internal(message()))
}

/// The selection of the ids of the comments of an item, with `arguments`.
fn comment_ids(arguments: &str) -> String {
    let comments = format!(
        "comments({arguments}) {{ nodes {{ databaseId updatedAt }} pageInfo {{ hasNextPage endCursor }} }}"
    );
    format!("... on Issue {{ {comments} }} ... on PullRequest {{ {comments} }}")
}

/// The selection of a node of a page of a connection with the counts of its
/// comments.
const COUNTED: &str = "number comments { totalCount }";

/// The selection of a node of a page of a connection with its number.
const NUMBERED: &str = "number";

/// The selection of the counts of the issues and of the pull requests.
const TOTALS: &str = "issues { totalCount } pullRequests { totalCount }";

/// A page of the items of a connection, each with the selection `node`.
fn page_query(connection: ItemKind, node: &str) -> String {
    format!(
        "query($owner: String!, $name: String!, $after: String) {{ \
         repository(owner: $owner, name: $name) {{ page: {}(first: {PAGE}, after: $after) {{ \
         nodes {{ {node} }} pageInfo {{ hasNextPage endCursor }} }} }} }}",
        connection.connection()
    )
}

/// A page of the ids of the comments of the item `$number`.
fn ids_query() -> String {
    format!(
        "query($owner: String!, $name: String!, $number: Int!, $after: String) {{ \
         repository(owner: $owner, name: $name) {{ item: issueOrPullRequest(number: $number) {{ {} }} }} }}",
        comment_ids(&format!("first: {PAGE}, after: $after"))
    )
}

/// A query of a check: `selection` of the repository, with the first page of
/// the ids of the comments of each item of `items` under its alias.
fn check_query(selection: &str, items: &[ItemRef]) -> String {
    let items = items
        .iter()
        .map(|item| {
            format!(
                "{}: issueOrPullRequest(number: {}) {{ {} }}",
                alias(*item),
                item.number,
                comment_ids(&format!("first: {PAGE}"))
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ \
         {selection} {items} }} }}"
    )
}

/// The alias of an item in the query of a check.
fn alias(item: ItemRef) -> String {
    format!("i{}", item.number)
}

/// Take a comment of an item by its ID with
/// `GET /repos/{owner}/{repo}/issues/comments/{id}`: the comment, or the
/// removal of a comment that left its item, `404`, with its `comments/`
/// joined as `write` says.
async fn fetch_comment<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    id: u64,
    write: ListingWrite,
) -> Result<()> {
    let response = cx
        .endpoint(GitHubApi)
        .get(format!("/repos/{}/issues/comments/{id}", repo.id))
        .send()
        .await?;
    match response.status() {
        200 => {
            let comment = serde_json::from_slice(response.body())
                .map_err(|error| ProviderError::invalid_input(format!("json decode: {error}")))?;
            write_comment(cx, repo, item, write, &comment)
        },
        404 => {
            let comments = comments_path(repo, item);
            cx.remove(format!("{comments}/{id}"))?;
            cx.dir(&comments, write)
        },
        status => Err(ProviderError::from_http_status(status)),
    }
}

/// A page of the ids of the comments of an item, none for an item that left
/// the repository.
async fn ids_page<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    after: Option<String>,
) -> Result<Option<Connection<CommentId>>> {
    let answer: Item = graphql(
        cx,
        repo,
        &ids_query(),
        json!({ "number": item.number, "after": after }),
    )
    .await?;
    Ok(answer.item.map(|item| item.comments))
}

// ===========================================================================
// Facts
// ===========================================================================

/// The repository of a feed: its id at the GitHub API, and the path of its
/// folder in the projection, which begins the anchor path the host asked the
/// feed for. The id normalizes the owner; the path keeps it as the host
/// serves it.
struct Repository {
    id: RepoId,
    path: String,
}

impl Repository {
    fn of<K>(cx: &FeedCx<K>, owner: &OwnerName, repo: &RepoName) -> Result<Self> {
        let mut segments = cx.path().segments();
        let (Some(owner_segment), Some(repo_segment)) = (segments.next(), segments.next()) else {
            return Err(ProviderError::internal(format!(
                "the feed at {} names no repository",
                cx.path()
            )));
        };
        Ok(Self {
            id: RepoId::new(owner, repo),
            path: format!("/{owner_segment}/{repo_segment}"),
        })
    }
}

fn folder(repo: &Repository, kind: ItemKind, filter: StateFilter) -> String {
    format!("{}/{kind}/{filter}", repo.path)
}

/// The folder of an item in `filter`: in `all/`, with its facts; in `open/`,
/// the entry that shares them.
fn item_path(repo: &Repository, item: ItemRef, filter: StateFilter) -> String {
    format!("{}/{}", folder(repo, item.kind, filter), item.number)
}

/// The folder `comments/` of an item, under its folder in `all/`.
fn comments_path(repo: &Repository, item: ItemRef) -> String {
    format!("{}/comments", item_path(repo, item, StateFilter::All))
}

/// Write the facts of an item from its row: `title`, `body`, `state`, and
/// `user` under `{kind}/all/{number}/`, with its entry in `{kind}/open/` for
/// the state `open`, else the removal of that entry; its folder `comments/`,
/// emptied and complete for a count `comments` of `0`, as its list would be
/// empty; and its entries in the listings of its folders, which they join
/// as `folders` says.
fn write_item<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    row: &ItemRow,
    folders: ListingWrite,
) -> Result<()> {
    let item = row.item();
    cx.dir(folder(repo, item.kind, StateFilter::All), folders)?;
    let at = item_path(repo, item, StateFilter::All);
    cx.dir(&at, ListingWrite::Partial)?;
    cx.file(format!("{at}/title"), row.data.title.as_bytes())?;
    cx.file(format!("{at}/body"), row.data.body_text().as_bytes())?;
    cx.file(format!("{at}/state"), row.data.state.as_bytes())?;
    cx.file(format!("{at}/user"), row.data.login().as_bytes())?;
    let comments = comments_path(repo, item);
    if row.comments == 0 {
        for id in listed(cx, &comments)? {
            cx.remove(format!("{comments}/{id}"))?;
        }
        cx.dir(&comments, ListingWrite::Complete)?;
    } else {
        cx.dir(&comments, ListingWrite::Partial)?;
    }
    if row.data.state == "open" {
        cx.dir(folder(repo, item.kind, StateFilter::Open), folders)?;
        cx.dir(
            item_path(repo, item, StateFilter::Open),
            ListingWrite::Partial,
        )?;
    } else {
        cx.remove(item_path(repo, item, StateFilter::Open))?;
    }
    Ok(())
}

/// Write the facts of a comment: `author` and `body.md` under
/// `comments/{id}/` of its item, its `comments/` joined as `write` says.
fn write_comment<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    write: ListingWrite,
    comment: &Comment,
) -> Result<()> {
    let comments = comments_path(repo, item);
    cx.dir(&comments, write)?;
    let at = format!("{comments}/{}", comment.id);
    cx.dir(&at, ListingWrite::Partial)?;
    cx.file(format!("{at}/author"), comment.user.login.as_bytes())?;
    cx.file(format!("{at}/body.md"), comment.body_text().as_bytes())?;
    Ok(())
}

/// Remove an item that left the repository from its folders.
fn remove_item<K>(cx: &FeedCx<K>, repo: &Repository, item: ItemRef) -> Result<()> {
    for filter in [StateFilter::All, StateFilter::Open] {
        cx.remove(item_path(repo, item, filter))?;
    }
    Ok(())
}

/// Whether the projection has a complete listing of `path`.
fn complete<K>(cx: &FeedCx<K>, path: &str) -> Result<bool> {
    Ok(cx
        .listing(path, ListingForm::Count)?
        .is_some_and(|facts| facts.complete))
}

/// The count of the entries the projection lists at `path`, none without a
/// listing.
fn count<K>(cx: &FeedCx<K>, path: &str) -> Result<Option<usize>> {
    let Some(facts) = cx.listing(path, ListingForm::Count)? else {
        return Ok(None);
    };
    let ListingEntries::Count(count) = facts.entries else {
        return Err(ProviderError::internal(format!(
            "the listing of {path} has no count"
        )));
    };
    usize::try_from(count)
        .map(Some)
        .map_err(|error| ProviderError::internal(format!("the listing of {path}: {error}")))
}

/// The numbers the projection lists at `path`, none without a listing.
fn listed<K>(cx: &FeedCx<K>, path: &str) -> Result<BTreeSet<u64>> {
    let Some(facts) = cx.listing(path, ListingForm::Names)? else {
        return Ok(BTreeSet::new());
    };
    let ListingEntries::Names(names) = facts.entries else {
        return Err(ProviderError::internal(format!(
            "the listing of {path} has no names"
        )));
    };
    names
        .iter()
        .map(|name| {
            name.parse()
                .map_err(|_| ProviderError::internal(format!("{path} lists {name}")))
        })
        .collect()
}

/// The IDs of a page of the ids of the comments of an item that come by their
/// IDs: each that `brought`, the comments the lists of the answer brought,
/// lacks, and that `known`, the comments of the item in the projection, lacks
/// or that was updated at or after `since`; none of `known` without `since`.
/// `seen` takes each id of the page.
fn ids_to_fetch(
    page: &[CommentId],
    known: &BTreeSet<u64>,
    brought: &BTreeSet<u64>,
    since: Option<OffsetDateTime>,
    seen: &mut BTreeSet<u64>,
) -> BTreeSet<u64> {
    let mut fetch = BTreeSet::new();
    for id in page {
        seen.insert(id.database_id);
        if !brought.contains(&id.database_id)
            && (!known.contains(&id.database_id)
                || since.is_some_and(|since| id.updated_at >= since))
        {
            fetch.insert(id.database_id);
        }
    }
    fetch
}

/// The end of the IDs of an item after their last page: the removal of each
/// comment of `known` and `brought` that `seen`, the ids of its pages, lacks,
/// and its `comments/` complete when no comment of `fetch` comes by its ID,
/// else at the last of those.
fn end_ids<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    known: &BTreeSet<u64>,
    brought: &BTreeSet<u64>,
    seen: &BTreeSet<u64>,
    fetch: &BTreeSet<u64>,
) -> Result<()> {
    let comments = comments_path(repo, item);
    for id in known.iter().chain(brought).filter(|id| !seen.contains(id)) {
        cx.remove(format!("{comments}/{id}"))?;
    }
    if fetch.is_empty() {
        cx.dir(&comments, ListingWrite::Complete)?;
    }
    Ok(())
}

/// How a comment by its ID joins the `comments/` of its item: complete after
/// the last comment of the last page of its IDs, which `last` says, else as
/// it is.
fn fetch_write(last: bool) -> ListingWrite {
    if last {
        ListingWrite::Complete
    } else {
        ListingWrite::Partial
    }
}

// ===========================================================================
// The feed of a repository
// ===========================================================================

/// One call of the feed of a repository: its context, the repository, and
/// the time and the `ETag` of its token.
struct RepoFeed<'a> {
    cx: &'a FeedCx<RepoKey>,
    repo: Repository,
    time: Option<OffsetDateTime>,
    etag: Option<String>,
}

impl Repo {
    /// The feed of the issues, pull requests, and comments of a repository.
    pub(crate) async fn feed(cx: FeedCx<RepoKey>) -> Result<Feed> {
        let repo = Repository::of(&cx, &cx.key().owner, &cx.key().repo)?;
        let Some(RepoToken { time, etag, stage }) = cx.token().map(decode).transpose()? else {
            let feed = RepoFeed {
                cx: &cx,
                repo,
                time: None,
                etag: None,
            };
            return feed.listing_rows(None).await;
        };
        let feed = RepoFeed {
            cx: &cx,
            repo,
            time,
            etag,
        };
        match stage {
            Some(Stage::Rows { next }) => feed.listing_rows(Some(next)).await,
            Some(Stage::Comments { page }) => feed.listing_comments(page).await,
            Some(Stage::Counts { connection, cursor }) => {
                feed.listing_counts(connection, cursor).await
            },
            Some(Stage::Ids {
                kind,
                items,
                after,
                seen,
                then,
            }) => feed.listing_ids(kind, items, after, seen, then).await,
            Some(Stage::Fetch { item, ids, then }) => feed.listing_fetch(item, ids, then).await,
            Some(Stage::Check(check)) => feed.check_step(*check).await,
            None => feed.check().await,
        }
    }
}

impl RepoFeed<'_> {
    /// The first page of the rows updated since the time, with the `ETag` of
    /// that time; `None` when its rows stand.
    async fn first_rows(&self) -> Result<Option<RowPage>> {
        let answer = self
            .cx
            .endpoint(GitHubApi)
            .get(format!(
                "/repos/{}/issues?state=all&sort=updated&direction=asc&per_page={PAGE}{}",
                self.repo.id,
                since_pair(self.time)?
            ))
            .maybe_if_none_match(self.etag.clone().map(VersionToken).as_ref())
            .page()
            .await?;
        RowPage::of(answer)
    }

    /// The page of rows at a link `next`.
    async fn next_rows(&self, next: &str) -> Result<RowPage> {
        let answer = self.cx.endpoint(GitHubApi).get(next).page().await?;
        RowPage::of(answer)?.ok_or_else(|| unconditional_304("the rows"))
    }

    /// A page of the rows of the first full listing, with the inner token of
    /// each item and the time of the last `updated_at`.
    async fn listing_rows(&self, next: Option<String>) -> Result<Feed> {
        let (page, etag) = match &next {
            None => {
                let page = self
                    .first_rows()
                    .await?
                    .ok_or_else(|| unconditional_304("the rows"))?;
                let etag = page.etag.clone();
                (page, etag)
            },
            Some(next) => (self.next_rows(next).await?, self.etag.clone()),
        };
        for row in &page.rows {
            write_item(self.cx, &self.repo, row, ListingWrite::Partial)?;
        }
        let time = page.rows.last().map(|row| row.updated_at).or(self.time);
        let etag = etag.filter(|_| time == self.time);
        let stage = match page.next {
            Some(next) => Stage::Rows { next },
            None => Stage::Comments { page: 1 },
        };
        let mut part = repo_part(time, etag, Some(stage))?;
        let key = self.cx.key();
        for row in &page.rows {
            let token = encode(&row.token(None))?;
            let number = row.data.number;
            part = match row.item().kind {
                ItemKind::Issues => part.inner::<Issue>(
                    &IssueKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: Facet(StateFilter::All),
                        number,
                    },
                    token,
                ),
                ItemKind::Pulls => part.inner::<PullRequest>(
                    &PullKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: Facet(StateFilter::All),
                        number,
                    },
                    token,
                ),
            };
        }
        Ok(part.into())
    }

    /// Write the comment of a row under its item in the projection, its
    /// `comments/` as complete as it is; a comment of an item outside the
    /// projection waits for its item.
    fn take_comment(&self, item: ItemRef, comment: &Comment) -> Result<()> {
        let comments = comments_path(&self.repo, item);
        let Some(facts) = self.cx.listing(&comments, ListingForm::Count)? else {
            return Ok(());
        };
        let write = if facts.complete {
            ListingWrite::Complete
        } else {
            ListingWrite::Partial
        };
        write_comment(self.cx, &self.repo, item, write, comment)
    }

    /// A page of the comments of the repository updated since a time.
    async fn comments_page(
        &self,
        since: Option<OffsetDateTime>,
        page: usize,
    ) -> Result<Vec<CommentRow>> {
        self.cx
            .endpoint(GitHubApi)
            .get(format!(
                "/repos/{}/issues/comments?sort=created&direction=asc&per_page={PAGE}{}&page={page}",
                self.repo.id,
                since_pair(since)?
            ))
            .json()
            .await
    }

    /// A page of the comments of the first full listing.
    async fn listing_comments(&self, page: usize) -> Result<Feed> {
        let rows = self.comments_page(None, page).await?;
        for row in &rows {
            self.take_comment(row.item()?, &row.comment)?;
        }
        let stage = if rows.len() < PAGE {
            Stage::Counts {
                connection: ItemKind::Issues,
                cursor: None,
            }
        } else {
            Stage::Comments { page: page + 1 }
        };
        Ok(repo_part(self.time, self.etag.clone(), Some(stage))?.into())
    }

    /// A page of the counts of the first full listing: the `comments/` of an
    /// item with the count of its `comments/` in the projection becomes
    /// complete, and the items with another count take their IDs next.
    async fn listing_counts(&self, connection: ItemKind, cursor: Option<String>) -> Result<Feed> {
        let Paged::<Counted> { page } = graphql(
            self.cx,
            &self.repo,
            &page_query(connection, COUNTED),
            json!({ "after": cursor }),
        )
        .await?;
        let mut items = BTreeSet::new();
        for node in page.nodes {
            let item = ItemRef {
                kind: connection,
                number: node.number,
            };
            let comments = comments_path(&self.repo, item);
            // An item that the rows did not bring is outside the projection.
            let Some(count) = count(self.cx, &comments)? else {
                continue;
            };
            if count == node.comments.total_count {
                self.cx.dir(&comments, ListingWrite::Complete)?;
            } else {
                items.insert(node.number);
            }
        }
        let then = match (page.page_info.next()?, connection) {
            (Some(cursor), _) => Some(Stage::Counts {
                connection,
                cursor: Some(cursor),
            }),
            (None, ItemKind::Issues) => Some(Stage::Counts {
                connection: ItemKind::Pulls,
                cursor: None,
            }),
            (None, ItemKind::Pulls) => None,
        };
        self.listing_part(items_stage(connection, items, then))
    }

    /// A page of the ids of the first of `items` in the first full listing:
    /// the comments that the projection lacks, which come by their IDs next,
    /// and after its last page the end of its IDs; an item that GitHub
    /// GraphQL cannot resolve leaves the repository.
    async fn listing_ids(
        &self,
        kind: ItemKind,
        mut items: BTreeSet<u64>,
        after: Option<String>,
        mut seen: BTreeSet<u64>,
        then: Option<Box<Stage>>,
    ) -> Result<Feed> {
        let number = *items.first().ok_or_else(|| {
            ProviderError::invalid_input("the first full listing takes the IDs of no item")
        })?;
        let item = ItemRef { kind, number };
        let Some(page) = ids_page(self.cx, &self.repo, item, after).await? else {
            remove_item(self.cx, &self.repo, item)?;
            items.remove(&number);
            return self.listing_part(items_stage(kind, items, then.map(|then| *then)));
        };
        let known = listed(self.cx, &comments_path(&self.repo, item))?;
        let fetch = ids_to_fetch(&page.nodes, &known, &BTreeSet::new(), None, &mut seen);
        let next = if let Some(after) = page.page_info.next()? {
            Some(Stage::Ids {
                kind,
                items,
                after: Some(after),
                seen,
                then,
            })
        } else {
            end_ids(
                self.cx,
                &self.repo,
                item,
                &known,
                &BTreeSet::new(),
                &seen,
                &fetch,
            )?;
            items.remove(&number);
            items_stage(kind, items, then.map(|then| *then))
        };
        if fetch.is_empty() {
            return self.listing_part(next);
        }
        self.listing_part(Some(Stage::Fetch {
            item,
            ids: fetch,
            then: next.map(Box::new),
        }))
    }

    /// A comment of the first full listing by its ID, the last of the last
    /// page of the IDs of its item with its `comments/` complete.
    async fn listing_fetch(
        &self,
        item: ItemRef,
        mut ids: BTreeSet<u64>,
        then: Option<Box<Stage>>,
    ) -> Result<Feed> {
        let id = ids.pop_first().ok_or_else(|| {
            ProviderError::invalid_input("the first full listing fetches no comment")
        })?;
        let more_pages = matches!(
            then.as_deref(),
            Some(Stage::Ids { kind, items, after: Some(_), .. })
                if *kind == item.kind && items.first() == Some(&item.number)
        );
        let write = fetch_write(ids.is_empty() && !more_pages);
        fetch_comment(self.cx, &self.repo, item, id, write).await?;
        if ids.is_empty() {
            return self.listing_part(then.map(|then| *then));
        }
        self.listing_part(Some(Stage::Fetch { item, ids, then }))
    }

    /// A part of the first full listing with its next stage; the last part,
    /// without one, completes the four folders.
    fn listing_part(&self, stage: Option<Stage>) -> Result<Feed> {
        if stage.is_none() {
            for kind in [ItemKind::Issues, ItemKind::Pulls] {
                for filter in [StateFilter::All, StateFilter::Open] {
                    self.cx
                        .dir(folder(&self.repo, kind, filter), ListingWrite::Complete)?;
                }
            }
        }
        Ok(repo_part(self.time, self.etag.clone(), stage)?.into())
    }

    /// The start of a check: the first page of the rows since the time, with
    /// the `ETag` of that time. For `304`, the counts, `unchanged` when they
    /// are those of the projection, else the numbers of each kind with
    /// another count.
    async fn check(&self) -> Result<Feed> {
        let mut check = Check {
            next: CheckRequest::Counts,
            time: self.time,
            etag: self.etag.clone(),
            changed: BTreeMap::new(),
            totals: None,
            removed: BTreeSet::new(),
        };
        let Some(page) = self.first_rows().await? else {
            let counts: Counts =
                graphql(self.cx, &self.repo, &check_query(TOTALS, &[]), json!({})).await?;
            check.totals = Some(Totals {
                issues: counts.issues.total_count,
                pulls: counts.pull_requests.total_count,
            });
            return match self.numbers_from(&check, ItemKind::Issues)? {
                Some(next) => {
                    check.next = next;
                    self.check_part(check)
                },
                None => Ok(Feed::unchanged()),
            };
        };
        check.etag = page.etag;
        self.take_check_rows(&mut check, &page.rows)?;
        check.next = match page.next {
            Some(next) => CheckRequest::Rows { next },
            None => CheckRequest::Comments { page: 1 },
        };
        self.check_part(check)
    }

    /// The next request of a check, a part each.
    async fn check_step(&self, mut check: Check) -> Result<Feed> {
        match std::mem::replace(&mut check.next, CheckRequest::Counts) {
            CheckRequest::Rows { next } => {
                let page = self.next_rows(&next).await?;
                self.take_check_rows(&mut check, &page.rows)?;
                check.next = match page.next {
                    Some(next) => CheckRequest::Rows { next },
                    None => CheckRequest::Comments { page: 1 },
                };
                return self.check_part(check);
            },
            CheckRequest::Comments { page } => {
                let rows = self.comments_page(self.time, page).await?;
                for row in &rows {
                    let item = row.item()?;
                    match check.changed.get_mut(&item.number) {
                        // Its row emptied its `comments/`: the comment changed
                        // the item after the row, so the next check takes it.
                        Some(Changed { comments: 0, .. }) => {},
                        Some(changed) => {
                            write_comment(
                                self.cx,
                                &self.repo,
                                item,
                                ListingWrite::Partial,
                                &row.comment,
                            )?;
                            changed.brought.insert(row.comment.id);
                        },
                        None => self.take_comment(item, &row.comment)?,
                    }
                }
                if rows.len() == PAGE {
                    check.next = CheckRequest::Comments { page: page + 1 };
                    return self.check_part(check);
                }
            },
            CheckRequest::Counts => {
                let comments = self.take_counts(&mut check).await?;
                if !comments.is_empty() {
                    check.next = CheckRequest::Fetch { comments };
                    return self.check_part(check);
                }
            },
            CheckRequest::Ids { number } => {
                let changed = check.changed.get(&number).ok_or_else(|| {
                    ProviderError::invalid_input(format!("item {number} is not a changed item"))
                })?;
                let item = ItemRef {
                    kind: changed.kind,
                    number,
                };
                let after = changed.ids.as_ref().map(|ids| ids.after.clone());
                match ids_page(self.cx, &self.repo, item, after).await? {
                    Some(page) => {
                        let ids = self.take_ids_page(&mut check, item, page)?;
                        if !ids.is_empty() {
                            check.next = CheckRequest::Fetch {
                                comments: BTreeMap::from([(number, ids)]),
                            };
                            return self.check_part(check);
                        }
                    },
                    None => self.remove_changed(&mut check, item)?,
                }
            },
            CheckRequest::Fetch { mut comments } => {
                let mut entry = comments
                    .first_entry()
                    .ok_or_else(|| ProviderError::invalid_input("the check fetches no comment"))?;
                let number = *entry.key();
                let id = entry.get_mut().pop_first().ok_or_else(|| {
                    ProviderError::invalid_input(format!(
                        "the check fetches no comment of {number}"
                    ))
                })?;
                if entry.get().is_empty() {
                    entry.remove();
                }
                let last = !comments.contains_key(&number);
                let changed = check.changed.get(&number).ok_or_else(|| {
                    ProviderError::invalid_input(format!("item {number} is not a changed item"))
                })?;
                let item = ItemRef {
                    kind: changed.kind,
                    number,
                };
                let write = fetch_write(last && changed.taken);
                fetch_comment(self.cx, &self.repo, item, id, write).await?;
                if !comments.is_empty() {
                    check.next = CheckRequest::Fetch { comments };
                    return self.check_part(check);
                }
            },
            CheckRequest::Numbers {
                kind,
                after,
                mut numbers,
            } => {
                let Paged::<Numbered> { page } = graphql(
                    self.cx,
                    &self.repo,
                    &page_query(kind, NUMBERED),
                    json!({ "after": after }),
                )
                .await?;
                numbers.extend(page.nodes.iter().map(|node| node.number));
                if let Some(after) = page.page_info.next()? {
                    check.next = CheckRequest::Numbers {
                        kind,
                        after: Some(after),
                        numbers,
                    };
                    return self.check_part(check);
                }
                self.remove_unnumbered(&check, kind, &numbers)?;
                let next = match kind {
                    ItemKind::Issues => self.numbers_from(&check, ItemKind::Pulls)?,
                    ItemKind::Pulls => None,
                };
                return match next {
                    Some(next) => {
                        check.next = next;
                        self.check_part(check)
                    },
                    None => self.check_end(&check),
                };
            },
        }
        match self.after_ids(&check)? {
            Some(next) => {
                check.next = next;
                self.check_part(check)
            },
            None => self.check_end(&check),
        }
    }

    /// The facts of a page of the rows of a check, and its changed items.
    fn take_check_rows(&self, check: &mut Check, rows: &[ItemRow]) -> Result<()> {
        for row in rows {
            write_item(self.cx, &self.repo, row, ListingWrite::Complete)?;
            let item = row.item();
            check.changed.insert(
                item.number,
                Changed {
                    kind: item.kind,
                    comments: row.comments,
                    brought: BTreeSet::new(),
                    taken: false,
                    ids: None,
                },
            );
            check.time = Some(row.updated_at);
        }
        Ok(())
    }

    /// The `POST` of the counts, while the check lacks them, with the first
    /// page of the ids of the next changed items with a count `comments`
    /// other than `0` whose IDs it lacks, a page of items: the IDs of each,
    /// and the removal of each that GitHub GraphQL cannot resolve. Returns
    /// the IDs of the comments that come by their IDs next, under the
    /// numbers of their items.
    async fn take_counts(&self, check: &mut Check) -> Result<BTreeMap<u64, BTreeSet<u64>>> {
        let asked = check
            .changed
            .iter()
            .filter(|(_, changed)| changed.comments != 0 && !changed.taken)
            .take(PAGE)
            .map(|(number, changed)| ItemRef {
                kind: changed.kind,
                number: *number,
            })
            .collect::<Vec<_>>();
        let mut items = if check.totals.is_some() {
            graphql::<BTreeMap<String, Option<Commented>>>(
                self.cx,
                &self.repo,
                &check_query("", &asked),
                json!({}),
            )
            .await?
        } else {
            let counts: Counts =
                graphql(self.cx, &self.repo, &check_query(TOTALS, &asked), json!({})).await?;
            check.totals = Some(Totals {
                issues: counts.issues.total_count,
                pulls: counts.pull_requests.total_count,
            });
            counts.items
        };
        let mut comments = BTreeMap::new();
        for item in asked {
            match items.remove(&alias(item)).ok_or_else(|| {
                ProviderError::invalid_input(format!("the counts lack the item {}", item.number))
            })? {
                Some(commented) => {
                    let ids = self.take_ids_page(check, item, commented.comments)?;
                    if !ids.is_empty() {
                        comments.insert(item.number, ids);
                    }
                },
                None => self.remove_changed(check, item)?,
            }
        }
        Ok(comments)
    }

    /// A page of the ids of a changed item, and after its last page the end of
    /// its IDs. Returns the IDs of the comments that its list did not bring
    /// and that the projection lacks or that were updated at or after the
    /// time of the check, which come by their IDs next.
    fn take_ids_page(
        &self,
        check: &mut Check,
        item: ItemRef,
        page: Connection<CommentId>,
    ) -> Result<BTreeSet<u64>> {
        let known = listed(self.cx, &comments_path(&self.repo, item))?;
        let next = page.page_info.next()?;
        let changed = check.changed.get_mut(&item.number).ok_or_else(|| {
            ProviderError::internal(format!("item {} is not a changed item", item.number))
        })?;
        let mut seen = match changed.ids.take() {
            Some(ids) => ids.seen,
            None => BTreeSet::new(),
        };
        let fetch = ids_to_fetch(&page.nodes, &known, &changed.brought, self.time, &mut seen);
        if let Some(after) = next {
            changed.ids = Some(IdsPage { after, seen });
        } else {
            end_ids(
                self.cx,
                &self.repo,
                item,
                &known,
                &changed.brought,
                &seen,
                &fetch,
            )?;
            changed.taken = true;
        }
        Ok(fetch)
    }

    /// The removal of a changed item that GitHub GraphQL cannot resolve.
    fn remove_changed(&self, check: &mut Check, item: ItemRef) -> Result<()> {
        remove_item(self.cx, &self.repo, item)?;
        check.removed.insert(item);
        let changed = check.changed.get_mut(&item.number).ok_or_else(|| {
            ProviderError::internal(format!("item {} is not a changed item", item.number))
        })?;
        changed.taken = true;
        changed.ids = None;
        Ok(())
    }

    /// The request of a check after the ids of its changed items: the next
    /// page of the ids of an item, the counts while the check lacks them or
    /// the ids of a changed item with comments, the numbers of each kind with
    /// another count, or none at its end.
    fn after_ids(&self, check: &Check) -> Result<Option<CheckRequest>> {
        if let Some((number, _)) = check
            .changed
            .iter()
            .find(|(_, changed)| changed.ids.is_some())
        {
            return Ok(Some(CheckRequest::Ids { number: *number }));
        }
        if check.totals.is_none()
            || check
                .changed
                .values()
                .any(|changed| changed.comments != 0 && !changed.taken)
        {
            return Ok(Some(CheckRequest::Counts));
        }
        self.numbers_from(check, ItemKind::Issues)
    }

    /// The first page of the numbers of the first kind from `kind` on whose
    /// count `totalCount` is other than the count of `{kind}/all/` in the
    /// projection with the new items of the check and without the items that
    /// it removed; none for the same counts.
    fn numbers_from(&self, check: &Check, kind: ItemKind) -> Result<Option<CheckRequest>> {
        let totals = check
            .totals
            .ok_or_else(|| ProviderError::internal("a check compares counts without them"))?;
        let kinds = match kind {
            ItemKind::Issues => &[ItemKind::Issues, ItemKind::Pulls][..],
            ItemKind::Pulls => &[ItemKind::Pulls][..],
        };
        for kind in kinds {
            let all = folder(&self.repo, *kind, StateFilter::All);
            let listed_count = count(self.cx, &all)?.ok_or_else(|| {
                ProviderError::internal(format!("the projection has no listing of {all}"))
            })?;
            let mut new = 0;
            for (number, changed) in &check.changed {
                let item = ItemRef {
                    kind: changed.kind,
                    number: *number,
                };
                if changed.kind == *kind
                    && count(self.cx, &item_path(&self.repo, item, StateFilter::All))?.is_none()
                {
                    new += 1;
                }
            }
            let gone = check
                .removed
                .iter()
                .filter(|item| item.kind == *kind)
                .count();
            let total = match kind {
                ItemKind::Issues => totals.issues,
                ItemKind::Pulls => totals.pulls,
            };
            if listed_count + new - gone != total {
                return Ok(Some(CheckRequest::Numbers {
                    kind: *kind,
                    after: None,
                    numbers: BTreeSet::new(),
                }));
            }
        }
        Ok(None)
    }

    /// The removal of each item of a kind in the projection or in the rows of
    /// the check that `numbers` lack.
    fn remove_unnumbered(
        &self,
        check: &Check,
        kind: ItemKind,
        numbers: &BTreeSet<u64>,
    ) -> Result<()> {
        let changed = check
            .changed
            .iter()
            .filter(|(_, changed)| changed.kind == kind)
            .map(|(number, _)| *number);
        let items = listed(self.cx, &folder(&self.repo, kind, StateFilter::All))?
            .into_iter()
            .chain(changed)
            .filter(|number| !numbers.contains(number))
            .collect::<BTreeSet<_>>();
        for number in items {
            let item = ItemRef { kind, number };
            if !check.removed.contains(&item) {
                remove_item(self.cx, &self.repo, item)?;
            }
        }
        Ok(())
    }

    /// A part of an unfinished check, with its next request.
    fn check_part(&self, check: Check) -> Result<Feed> {
        Ok(repo_part(
            self.time,
            self.etag.clone(),
            Some(Stage::Check(Box::new(check))),
        )?
        .into())
    }

    /// The last part of a check: the time of its last row, and the `ETag` of
    /// its first page while that time stands.
    fn check_end(&self, check: &Check) -> Result<Feed> {
        let etag = check.etag.clone().filter(|_| check.time == self.time);
        Ok(repo_part(check.time, etag, None)?.into())
    }
}

/// A part of the feed of a repository with the token of `time`, `etag`, and
/// `stage`, followed by more parts while a stage remains.
fn repo_part(
    time: Option<OffsetDateTime>,
    etag: Option<String>,
    stage: Option<Stage>,
) -> Result<FeedPart> {
    let more = stage.is_some();
    Ok(Feed::part(encode(&RepoToken { time, etag, stage })?).more(more))
}

/// The stage of the first full listing that takes the IDs of `items` from
/// their first page, then `then`; `then` without items.
fn items_stage(kind: ItemKind, items: BTreeSet<u64>, then: Option<Stage>) -> Option<Stage> {
    if items.is_empty() {
        return then;
    }
    Some(Stage::Ids {
        kind,
        items,
        after: None,
        seen: BTreeSet::new(),
        then: then.map(Box::new),
    })
}

fn unconditional_304(what: &str) -> ProviderError {
    ProviderError::internal(format!("{what} answered 304 to a request without an ETag"))
}

// ===========================================================================
// The feed of an item
// ===========================================================================

impl Issue {
    /// The feed of an issue before the first full listing of its repository
    /// ends.
    pub(crate) async fn feed(cx: FeedCx<IssueKey>) -> Result<Feed> {
        let key = cx.key();
        let item = ItemRef {
            kind: ItemKind::Issues,
            number: key.number,
        };
        item_feed(&cx, &Repository::of(&cx, &key.owner, &key.repo)?, item).await
    }
}

impl PullRequest {
    /// The feed of a pull request before the first full listing of its
    /// repository ends.
    pub(crate) async fn feed(cx: FeedCx<PullKey>) -> Result<Feed> {
        let key = cx.key();
        let item = ItemRef {
            kind: ItemKind::Pulls,
            number: key.number,
        };
        item_feed(&cx, &Repository::of(&cx, &key.owner, &key.repo)?, item).await
    }
}

/// The feed of an item: the next request of an unfinished answer; else, from
/// its `comments/`, the pages of its comments; else its answer, and for a
/// change of an item with its comments in the projection and a count
/// `comments` other than `0`, the comments since the `updated_at` of its token
/// and its IDs; a first answer from its `comments/` takes its answer, then the
/// pages of its comments. Each answer makes a part of each request.
async fn item_feed<K>(cx: &FeedCx<K>, repo: &Repository, item: ItemRef) -> Result<Feed> {
    let token = cx
        .token()
        .map(decode::<Option<ItemToken>>)
        .transpose()?
        .flatten();
    if let Some(mut token) = token {
        if let Some(next) = token.next.take() {
            return item_step(cx, repo, item, token, next).await;
        }
        if cx.start() == Some("comments") {
            return item_comments_of(cx, repo, item, token).await;
        }
        return match item_answer(cx, repo, item, Some(&token)).await? {
            ItemAnswer::Unchanged => Ok(Feed::unchanged()),
            ItemAnswer::Removed => removal(cx, repo, item),
            ItemAnswer::Found(row, etag) => {
                write_found(cx, repo, item, &row)?;
                let mut next = row.token(etag);
                if row.comments != 0 && complete(cx, &comments_path(repo, item))? {
                    next.next = Some(ItemRequest::Since {
                        since: token.updated_at,
                        page: 1,
                        brought: BTreeSet::new(),
                    });
                }
                item_part(&next)
            },
        };
    }
    match item_answer(cx, repo, item, None).await? {
        ItemAnswer::Found(row, etag) => {
            write_found(cx, repo, item, &row)?;
            let mut token = row.token(etag);
            // The answer of an item with the count `comments` `0` empties its
            // `comments/`, and completes it.
            if cx.start() == Some("comments") && row.comments != 0 {
                token.next = Some(ItemRequest::Comments {
                    page: 1,
                    brought: BTreeSet::new(),
                });
            }
            item_part(&token)
        },
        ItemAnswer::Removed => removal(cx, repo, item),
        ItemAnswer::Unchanged => Err(unconditional_304("the item")),
    }
}

/// The comments of an item from its `comments/`: none for the count
/// `comments` `0`, with an empty and complete `comments/`, else the pages of
/// its comments, a part each.
async fn item_comments_of<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    token: ItemToken,
) -> Result<Feed> {
    if token.comments == 0 {
        let comments = comments_path(repo, item);
        for id in listed(cx, &comments)? {
            cx.remove(format!("{comments}/{id}"))?;
        }
        cx.dir(&comments, ListingWrite::Complete)?;
        return item_part(&token);
    }
    let next = ItemRequest::Comments {
        page: 1,
        brought: BTreeSet::new(),
    };
    item_step(cx, repo, item, token, next).await
}

/// One request of an unfinished answer of an item, a part with the next
/// request in its token, or its last part.
async fn item_step<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    mut token: ItemToken,
    request: ItemRequest,
) -> Result<Feed> {
    match take_item_request(cx, repo, item, &token, request).await? {
        ItemStep::Next(request) => {
            token.next = Some(request);
            item_part(&token)
        },
        ItemStep::Done => item_part(&token),
        ItemStep::Left => removal(cx, repo, item),
    }
}

/// What a request of the comments of an item leaves: its next request, its
/// end, or the item gone from the repository.
enum ItemStep {
    Next(ItemRequest),
    Done,
    Left,
}

/// One request of the comments of an item: a page of its comments since a
/// time, then its IDs; a page of all of its comments, a complete
/// `comments/` without the comments they lack for the count of its token,
/// else its IDs; a page of its IDs, then each comment that no list brought
/// and that the projection lacks or that was updated at or after the time by
/// its ID, and after the last page the removal of each other comment; a
/// comment by its ID.
async fn take_item_request<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    token: &ItemToken,
    request: ItemRequest,
) -> Result<ItemStep> {
    let comments = comments_path(repo, item);
    match request {
        ItemRequest::Since {
            since,
            page,
            mut brought,
        } => {
            let rows = item_comments(cx, repo, item, Some(since), page).await?;
            for comment in &rows {
                write_comment(cx, repo, item, ListingWrite::Partial, comment)?;
                brought.insert(comment.id);
            }
            Ok(ItemStep::Next(if rows.len() == PAGE {
                ItemRequest::Since {
                    since,
                    page: page + 1,
                    brought,
                }
            } else {
                ItemRequest::Ids {
                    time: since,
                    after: None,
                    brought,
                    seen: BTreeSet::new(),
                }
            }))
        },
        ItemRequest::Comments { page, mut brought } => {
            let rows = item_comments(cx, repo, item, None, page).await?;
            for comment in &rows {
                write_comment(cx, repo, item, ListingWrite::Partial, comment)?;
                brought.insert(comment.id);
            }
            if rows.len() == PAGE {
                return Ok(ItemStep::Next(ItemRequest::Comments {
                    page: page + 1,
                    brought,
                }));
            }
            if brought.len() != token.comments {
                return Ok(ItemStep::Next(ItemRequest::Ids {
                    time: token.updated_at,
                    after: None,
                    brought,
                    seen: BTreeSet::new(),
                }));
            }
            cx.dir(&comments, ListingWrite::Complete)?;
            for id in listed(cx, &comments)?.difference(&brought) {
                cx.remove(format!("{comments}/{id}"))?;
            }
            Ok(ItemStep::Done)
        },
        ItemRequest::Ids {
            time,
            after,
            brought,
            mut seen,
        } => {
            let Some(page) = ids_page(cx, repo, item, after).await? else {
                return Ok(ItemStep::Left);
            };
            let known = listed(cx, &comments)?;
            let fetch = ids_to_fetch(&page.nodes, &known, &brought, Some(time), &mut seen);
            let then = if let Some(after) = page.page_info.next()? {
                Some(Box::new(ItemRequest::Ids {
                    time,
                    after: Some(after),
                    brought,
                    seen,
                }))
            } else {
                end_ids(cx, repo, item, &known, &brought, &seen, &fetch)?;
                None
            };
            Ok(fetch_then(fetch, then))
        },
        ItemRequest::Fetch { mut ids, then } => {
            let id = ids
                .pop_first()
                .ok_or_else(|| ProviderError::invalid_input("the item fetches no comment"))?;
            let write = fetch_write(ids.is_empty() && then.is_none());
            fetch_comment(cx, repo, item, id, write).await?;
            Ok(fetch_then(ids, then))
        },
    }
}

/// The request of an item after a request of its ids: the comments of `ids`
/// by their IDs, then `then`, its end without it.
fn fetch_then(ids: BTreeSet<u64>, then: Option<Box<ItemRequest>>) -> ItemStep {
    if !ids.is_empty() {
        return ItemStep::Next(ItemRequest::Fetch { ids, then });
    }
    match then {
        Some(then) => ItemStep::Next(*then),
        None => ItemStep::Done,
    }
}

/// A part of the feed of an item with `token`, followed by more parts while
/// its answer has a next request.
fn item_part(token: &ItemToken) -> Result<Feed> {
    Ok(Feed::part(encode(&Some(token))?)
        .more(token.next.is_some())
        .into())
}

/// The facts of the answer of an item, not found for an item of the other
/// kind.
fn write_found<K>(cx: &FeedCx<K>, repo: &Repository, item: ItemRef, row: &ItemRow) -> Result<()> {
    if row.item() != item {
        return Err(ProviderError::not_found(format!(
            "{}#{} is not in {}",
            repo.id, item.number, item.kind
        )));
    }
    write_item(cx, repo, row, ListingWrite::Partial)
}

/// The removal of an item from its folders, with the token of a removed
/// item.
fn removal<K>(cx: &FeedCx<K>, repo: &Repository, item: ItemRef) -> Result<Feed> {
    remove_item(cx, repo, item)?;
    Ok(Feed::part(encode(&None::<ItemToken>)?).into())
}
