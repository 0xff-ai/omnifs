//! The feeds of the issues, pull requests, and comments of a repository.
//!
//! The feed of a repository takes its first full listing a page at a time:
//! the rows of its issues and pull requests by `updated_at`, then its
//! comments, then the GitHub GraphQL counts of the comments of each item.
//! After it, each answer is a check of the changes since the time of its last
//! row. Before the first full listing ends, the feed of an item answers for
//! that item from `GET /repos/{owner}/{repo}/issues/{number}`.
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
/// its first full listing is unfinished, its stage.
#[derive(Serialize, Deserialize)]
struct RepoToken {
    #[serde(with = "time::serde::rfc3339::option")]
    time: Option<OffsetDateTime>,
    etag: Option<String>,
    stage: Option<Stage>,
}

/// The stage of an unfinished first full listing of a repository.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    /// The rows, at their link `next`.
    Rows { next: String },
    /// The comments, at a page.
    Comments { page: usize },
    /// The counts, at the cursor of a connection.
    Counts {
        connection: ItemKind,
        cursor: Option<String>,
    },
}

/// The token of the feed of an item: the `ETag` of its last answer, none for
/// an item from a page of rows, its `updated_at`, and its count `comments`.
/// The token of a removed item is `null`.
#[derive(Serialize, Deserialize)]
struct ItemToken {
    etag: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    comments: usize,
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

/// The comments of an item updated since a time, a page at a time.
async fn item_comments<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    since: Option<OffsetDateTime>,
) -> Result<Vec<Comment>> {
    let mut comments = Vec::new();
    for page in 1.. {
        let rows: Vec<Comment> = cx
            .endpoint(GitHubApi)
            .get(format!(
                "/repos/{}/issues/{}/comments?per_page={PAGE}{}&page={page}",
                repo.id,
                item.number,
                since_pair(since)?
            ))
            .json()
            .await?;
        let last = rows.len() < PAGE;
        comments.extend(rows);
        if last {
            break;
        }
    }
    Ok(comments)
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
            .with_retry_after(crate::api::retry_window(|name| response.header(name))));
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

/// Take the IDs of an item: the pages of the ids of its comments from GitHub
/// GraphQL, from `page` when the answer has the first one;
/// `GET /repos/{owner}/{repo}/issues/comments/{id}` for each comment that is
/// not in `brought`, the comments the lists of the answer brought, and that
/// `known`, the comments of the item in the projection, lacks or that was
/// updated at or after `time`; and the removal of each other comment. The
/// `comments/` of the item is complete after them. Returns whether the item
/// stands: GitHub GraphQL cannot resolve an item that left the repository.
async fn take_ids<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    known: &BTreeSet<u64>,
    time: Option<OffsetDateTime>,
    brought: &BTreeSet<u64>,
    page: Option<Connection<CommentId>>,
) -> Result<bool> {
    let page = match page {
        Some(page) => Some(page),
        None => ids_page(cx, repo, item, None).await?,
    };
    let Some(mut page) = page else {
        return Ok(false);
    };
    let mut ids = Vec::new();
    loop {
        let next = page.page_info.next()?;
        ids.extend(page.nodes);
        let Some(after) = next else { break };
        let Some(next) = ids_page(cx, repo, item, Some(after)).await? else {
            return Ok(false);
        };
        page = next;
    }
    for id in &ids {
        if brought.contains(&id.database_id) {
            continue;
        }
        if !known.contains(&id.database_id) || time.is_none_or(|time| id.updated_at >= time) {
            let comment: Comment = cx
                .endpoint(GitHubApi)
                .get(format!(
                    "/repos/{}/issues/comments/{}",
                    repo.id, id.database_id
                ))
                .json()
                .await?;
            write_comment(cx, repo, item, ListingWrite::Complete, &comment)?;
        }
    }
    let ids = ids.iter().map(|id| id.database_id).collect::<BTreeSet<_>>();
    let comments = comments_path(repo, item);
    cx.dir(&comments, ListingWrite::Complete)?;
    for id in known.iter().chain(brought).filter(|id| !ids.contains(id)) {
        cx.remove(format!("{comments}/{id}"))?;
    }
    Ok(true)
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

/// An item of the rows of a check: the count `comments` of its row, whether
/// the projection lacked it before the check, and the comments that the list
/// of the check brought for it.
struct Changed {
    comments: usize,
    new: bool,
    brought: BTreeSet<u64>,
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

    /// A page of the counts of the first full listing: an item with another
    /// count than its `comments/` in the projection takes its IDs, and each
    /// `comments/` becomes complete; the last part completes the four
    /// folders.
    async fn listing_counts(&self, connection: ItemKind, cursor: Option<String>) -> Result<Feed> {
        let Paged::<Counted> { page } = graphql(
            self.cx,
            &self.repo,
            &page_query(connection, COUNTED),
            json!({ "after": cursor }),
        )
        .await?;
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
                continue;
            }
            let known = listed(self.cx, &comments)?;
            if !take_ids(
                self.cx,
                &self.repo,
                item,
                &known,
                self.time,
                &BTreeSet::new(),
                None,
            )
            .await?
            {
                remove_item(self.cx, &self.repo, item)?;
            }
        }
        let stage = match (page.page_info.next()?, connection) {
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

    /// A check: each page of the rows since the time, the first with the
    /// `ETag` of that time; for rows, the comments since the same time; then
    /// the counts with the IDs of the changed items. For `304`, the counts
    /// alone, `unchanged` when they are the same.
    async fn check(&self) -> Result<Feed> {
        let Some(mut page) = self.first_rows().await? else {
            return Ok(if self.take_counts(&BTreeMap::new()).await? {
                repo_part(self.time, self.etag.clone(), None)?.into()
            } else {
                Feed::unchanged()
            });
        };
        let etag = page.etag.take();
        let mut time = self.time;
        let mut changed = BTreeMap::new();
        loop {
            for row in &page.rows {
                let item = row.item();
                let new = count(self.cx, &item_path(&self.repo, item, StateFilter::All))?.is_none();
                write_item(self.cx, &self.repo, row, ListingWrite::Complete)?;
                changed.insert(
                    item,
                    Changed {
                        comments: row.comments,
                        new,
                        brought: BTreeSet::new(),
                    },
                );
                time = Some(row.updated_at);
            }
            let Some(next) = page.next.take() else { break };
            page = self.next_rows(&next).await?;
        }
        for number in 1.. {
            let rows = self.comments_page(self.time, number).await?;
            for row in &rows {
                let item = row.item()?;
                match changed.get_mut(&item) {
                    // Its row emptied its `comments/`: the comment changed the
                    // item after the row, so the next check takes it.
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
            if rows.len() < PAGE {
                break;
            }
        }
        self.take_counts(&changed).await?;
        let etag = etag.filter(|_| time == self.time);
        Ok(repo_part(time, etag, None)?.into())
    }

    /// The counts of a check, with the first page of the ids of the comments
    /// of each changed item with a count `comments` other than `0`, a page of
    /// items in a query: the IDs of each of them, the removal of each that
    /// GitHub GraphQL cannot resolve, and for another count of a kind than
    /// that of `{kind}/all/` in the projection with the new items, the
    /// numbers of the kind from GitHub GraphQL and the removal of each other
    /// item. Returns whether it wrote a fact.
    async fn take_counts(&self, changed: &BTreeMap<ItemRef, Changed>) -> Result<bool> {
        let asked = changed
            .iter()
            .filter(|(_, changed)| changed.comments != 0)
            .map(|(item, _)| *item)
            .collect::<Vec<_>>();
        let (first, rest) = asked.split_at(asked.len().min(PAGE));
        let Counts {
            issues,
            pull_requests,
            mut items,
        } = graphql(self.cx, &self.repo, &check_query(TOTALS, first), json!({})).await?;
        for batch in rest.chunks(PAGE) {
            items.extend(
                graphql::<BTreeMap<String, Option<Commented>>>(
                    self.cx,
                    &self.repo,
                    &check_query("", batch),
                    json!({}),
                )
                .await?,
            );
        }
        let mut left = BTreeSet::new();
        for item in asked {
            let commented = items.remove(&alias(item)).ok_or_else(|| {
                ProviderError::invalid_input(format!("the counts lack the item {}", item.number))
            })?;
            let stands = match commented {
                Some(commented) => {
                    take_ids(
                        self.cx,
                        &self.repo,
                        item,
                        &listed(self.cx, &comments_path(&self.repo, item))?,
                        self.time,
                        &changed[&item].brought,
                        Some(commented.comments),
                    )
                    .await?
                },
                None => false,
            };
            if !stands {
                remove_item(self.cx, &self.repo, item)?;
                left.insert(item);
            }
        }
        let mut wrote = !changed.is_empty();
        for (kind, total) in [
            (ItemKind::Issues, issues.total_count),
            (ItemKind::Pulls, pull_requests.total_count),
        ] {
            let all = folder(&self.repo, kind, StateFilter::All);
            let listed_count = count(self.cx, &all)?.ok_or_else(|| {
                ProviderError::internal(format!("the projection has no listing of {all}"))
            })?;
            let of_kind = |item: &&ItemRef| item.kind == kind;
            let new = changed
                .iter()
                .filter(|(item, changed)| item.kind == kind && changed.new)
                .count();
            let gone = left.iter().filter(of_kind).count();
            if listed_count + new - gone == total {
                continue;
            }
            let numbers = self.numbers(kind).await?;
            let items = listed(self.cx, &all)?
                .into_iter()
                .chain(changed.keys().filter(of_kind).map(|item| item.number));
            for number in items.filter(|number| !numbers.contains(number)) {
                let item = ItemRef { kind, number };
                if !left.contains(&item) {
                    remove_item(self.cx, &self.repo, item)?;
                    wrote = true;
                }
            }
        }
        Ok(wrote || !left.is_empty())
    }

    /// The numbers of the items of a kind, from GitHub GraphQL.
    async fn numbers(&self, kind: ItemKind) -> Result<BTreeSet<u64>> {
        let mut numbers = BTreeSet::new();
        let mut after = None;
        loop {
            let Paged::<Numbered> { page } = graphql(
                self.cx,
                &self.repo,
                &page_query(kind, NUMBERED),
                json!({ "after": after }),
            )
            .await?;
            numbers.extend(page.nodes.iter().map(|node| node.number));
            match page.page_info.next()? {
                Some(cursor) => after = Some(cursor),
                None => return Ok(numbers),
            }
        }
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

/// The feed of an item: its answer, and for a change of an item with its
/// comments in the projection and a count `comments` other than `0`, the
/// comments since the `updated_at` of its token and its IDs.
async fn item_feed<K>(cx: &FeedCx<K>, repo: &Repository, item: ItemRef) -> Result<Feed> {
    let token = cx
        .token()
        .map(decode::<Option<ItemToken>>)
        .transpose()?
        .flatten();
    if cx.start() == Some("comments") {
        return item_feed_comments(cx, repo, item, token).await;
    }
    match item_answer(cx, repo, item, token.as_ref()).await? {
        ItemAnswer::Unchanged => Ok(Feed::unchanged()),
        ItemAnswer::Removed => removal(cx, repo, item),
        ItemAnswer::Found(row, etag) => {
            write_found(cx, repo, item, &row)?;
            let comments = comments_path(repo, item);
            if row.comments != 0
                && let Some(previous) = token
                && complete(cx, &comments)?
            {
                let since = previous.updated_at;
                let rows = item_comments(cx, repo, item, Some(since)).await?;
                for comment in &rows {
                    write_comment(cx, repo, item, ListingWrite::Partial, comment)?;
                }
                let brought = rows.iter().map(|comment| comment.id).collect();
                let known = listed(cx, &comments)?;
                if !take_ids(cx, repo, item, &known, Some(since), &brought, None).await? {
                    return removal(cx, repo, item);
                }
            }
            Ok(Feed::part(encode(&Some(row.token(etag)))?).into())
        },
    }
}

/// The feed of an item started by its `comments/`: in place of its own
/// answer, which comes first only without a token, the pages of its
/// comments for a count other than `0`, a complete `comments/` without the
/// comments they lack for the count of its token, and its IDs for another
/// count.
async fn item_feed_comments<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    token: Option<ItemToken>,
) -> Result<Feed> {
    let token = match token {
        Some(token) => token,
        None => match item_answer(cx, repo, item, None).await? {
            ItemAnswer::Found(row, etag) => {
                write_found(cx, repo, item, &row)?;
                row.token(etag)
            },
            ItemAnswer::Removed => return removal(cx, repo, item),
            ItemAnswer::Unchanged => return Err(unconditional_304("the item")),
        },
    };
    let comments = comments_path(repo, item);
    let known = listed(cx, &comments)?;
    let rows = if token.comments == 0 {
        Vec::new()
    } else {
        item_comments(cx, repo, item, None).await?
    };
    for comment in &rows {
        write_comment(cx, repo, item, ListingWrite::Partial, comment)?;
    }
    let brought = rows
        .iter()
        .map(|comment| comment.id)
        .collect::<BTreeSet<_>>();
    if rows.len() == token.comments {
        cx.dir(&comments, ListingWrite::Complete)?;
        for id in known.difference(&brought) {
            cx.remove(format!("{comments}/{id}"))?;
        }
    } else if !take_ids(
        cx,
        repo,
        item,
        &known,
        Some(token.updated_at),
        &brought,
        None,
    )
    .await?
    {
        return removal(cx, repo, item);
    }
    Ok(Feed::part(encode(&Some(token))?).into())
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
