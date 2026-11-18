//! The feeds of the issues, pull requests, and comments of a repository.
//!
//! The feed of a repository takes its first full listing a page at a time:
//! the rows of its issues and pull requests by `updated_at`, then its
//! comments, then the GitHub GraphQL counts of the comments of each item.
//! After it, each answer is a check of the changes since the time of its last
//! row. Before the first full listing ends, the feed of an item answers for
//! that item from `GET /repos/{owner}/{repo}/issues/{number}`.

use std::collections::{BTreeMap, BTreeSet};

use omnifs_sdk::hashbrown::HashMap;
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
/// the `ETag` of the last request for its changes since that time, and the
/// stage of an unfinished answer.
#[derive(Serialize, Deserialize)]
struct RepoToken {
    #[serde(with = "time::serde::rfc3339::option")]
    time: Option<OffsetDateTime>,
    etag: Option<String>,
    stage: Option<Stage>,
}

/// The stage of an unfinished answer of the feed of a repository.
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
    /// A check: the time it asks the changes since, the link `next` of its
    /// rows while they continue, and the items they brought.
    Check {
        #[serde(with = "time::serde::rfc3339::option")]
        since: Option<OffsetDateTime>,
        next: Option<String>,
        changed: BTreeSet<ItemRef>,
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

/// An answer of GitHub GraphQL: its errors, or its data.
#[derive(Deserialize)]
#[serde(untagged)]
enum GraphQl<T> {
    Errors { errors: Vec<GraphQlError> },
    Data { data: GraphQlData<T> },
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

#[derive(Deserialize)]
struct Item {
    item: Commented,
}

/// The answer of the query of a check: the counts of the issues and of the
/// pull requests, and each changed item under its alias.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Counts {
    issues: Total,
    pull_requests: Total,
    #[serde(flatten)]
    items: BTreeMap<String, Commented>,
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
/// repository as `$owner` and `$name`; an answer with errors fails, as rate
/// limited for the type `RATE_LIMITED`.
async fn graphql<T: DeserializeOwned>(
    cx: &Cx,
    repo: &Repository,
    query: &str,
    mut variables: serde_json::Value,
) -> Result<T> {
    variables["owner"] = repo.id.owner.to_string().into();
    variables["name"] = repo.id.repo.to_string().into();
    let answer = cx
        .endpoint(GitHubApi)
        .post("/graphql")
        .body_json(&json!({ "query": query, "variables": variables }))
        .json::<GraphQl<T>>()
        .await?;
    match answer {
        GraphQl::Data { data } => Ok(data.repository),
        GraphQl::Errors { errors } => {
            let message = format!(
                "GitHub GraphQL: {}",
                errors
                    .iter()
                    .map(|error| error.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            if errors
                .iter()
                .any(|error| error.kind.as_deref() == Some("RATE_LIMITED"))
            {
                Err(ProviderError::rate_limited(message))
            } else {
                Err(ProviderError::internal(message))
            }
        },
    }
}

/// The selection of the ids of the comments of an item, with `arguments`.
fn comment_ids(arguments: &str) -> String {
    let comments = format!(
        "comments({arguments}) {{ nodes {{ databaseId updatedAt }} pageInfo {{ hasNextPage endCursor }} }}"
    );
    format!("... on Issue {{ {comments} }} ... on PullRequest {{ {comments} }}")
}

/// A page of the items of a connection with the counts of their comments.
fn counts_query(connection: ItemKind) -> String {
    format!(
        "query($owner: String!, $name: String!, $after: String) {{ \
         repository(owner: $owner, name: $name) {{ page: {}(first: {PAGE}, after: $after) {{ \
         nodes {{ number comments {{ totalCount }} }} pageInfo {{ hasNextPage endCursor }} }} }} }}",
        connection.connection()
    )
}

/// A page of the numbers of the items of a connection.
fn numbers_query(connection: ItemKind) -> String {
    format!(
        "query($owner: String!, $name: String!, $after: String) {{ \
         repository(owner: $owner, name: $name) {{ page: {}(first: {PAGE}, after: $after) {{ \
         nodes {{ number }} pageInfo {{ hasNextPage endCursor }} }} }} }}",
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

/// The query of a check: the counts of the issues and of the pull requests,
/// and the first page of the ids of the comments of each changed item.
fn check_query(changed: &BTreeSet<ItemRef>) -> String {
    let items = changed
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
         issues {{ totalCount }} pullRequests {{ totalCount }} {items} }} }}"
    )
}

/// The alias of an item in the query of a check.
fn alias(item: ItemRef) -> String {
    format!("i{}", item.number)
}

/// A page of the ids of the comments of an item.
async fn ids_page<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    after: Option<String>,
) -> Result<Connection<CommentId>> {
    let answer: Item = graphql(
        cx,
        repo,
        &ids_query(),
        json!({ "number": item.number, "after": after }),
    )
    .await?;
    Ok(answer.item.comments)
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

fn item_path(repo: &Repository, item: ItemRef, filter: StateFilter) -> String {
    format!("{}/{}", folder(repo, item.kind, filter), item.number)
}

fn comments_path(repo: &Repository, item: ItemRef, filter: StateFilter) -> String {
    format!("{}/comments", item_path(repo, item, filter))
}

/// Write the facts of an item from its row: `title`, `body`, `state`, and
/// `user` under `{kind}/all/{number}/` and, for the state `open`, under
/// `{kind}/open/{number}/`, else the removal of that folder; its folder
/// `comments/`, empty and complete for a count `comments` of `0`; and its
/// entries in the listings of its folders, which they join as `folders`
/// says. Returns the folders of the item.
fn write_item<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    row: &ItemRow,
    folders: ListingWrite,
) -> Result<Vec<StateFilter>> {
    let item = row.item();
    let filters = if row.data.state == "open" {
        vec![StateFilter::All, StateFilter::Open]
    } else {
        cx.remove(item_path(repo, item, StateFilter::Open))?;
        vec![StateFilter::All]
    };
    let comments = if row.comments == 0 {
        ListingWrite::Complete
    } else {
        ListingWrite::Partial
    };
    for filter in &filters {
        cx.dir(folder(repo, item.kind, *filter), folders)?;
        let at = item_path(repo, item, *filter);
        cx.dir(&at, ListingWrite::Partial)?;
        cx.file(format!("{at}/title"), row.data.title.as_bytes())?;
        cx.file(format!("{at}/body"), row.data.body_text().as_bytes())?;
        cx.file(format!("{at}/state"), row.data.state.as_bytes())?;
        cx.file(format!("{at}/user"), row.data.login().as_bytes())?;
        cx.dir(format!("{at}/comments"), comments)?;
    }
    Ok(filters)
}

/// Write the facts of a comment: `author` and `body.md` under
/// `comments/{id}/` of its item in each of the folders of the item, each
/// `comments/` joined as its write says.
fn write_comment<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    folders: &[(StateFilter, ListingWrite)],
    comment: &Comment,
) -> Result<()> {
    for (filter, write) in folders {
        let comments = comments_path(repo, item, *filter);
        cx.dir(&comments, *write)?;
        let at = format!("{comments}/{}", comment.id);
        cx.dir(&at, ListingWrite::Partial)?;
        cx.file(format!("{at}/author"), comment.user.login.as_bytes())?;
        cx.file(format!("{at}/body.md"), comment.body_text().as_bytes())?;
    }
    Ok(())
}

/// Whether the projection has a complete listing of `path`.
fn complete<K>(cx: &FeedCx<K>, path: &str) -> Result<bool> {
    Ok(cx
        .listing(path, ListingForm::Count)?
        .is_some_and(|facts| facts.complete))
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
/// a folder of `known`, the comments of the item in each of its folders,
/// lacks or that was updated at or after `time`; and the removal of each
/// other comment. The `comments/` of the item is complete after them.
async fn take_ids<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    known: &[(StateFilter, BTreeSet<u64>)],
    time: Option<OffsetDateTime>,
    brought: &BTreeSet<u64>,
    page: Option<Connection<CommentId>>,
) -> Result<()> {
    let mut page = match page {
        Some(page) => page,
        None => ids_page(cx, repo, item, None).await?,
    };
    let mut ids = Vec::new();
    loop {
        let next = page.page_info.next()?;
        ids.extend(page.nodes);
        let Some(after) = next else { break };
        page = ids_page(cx, repo, item, Some(after)).await?;
    }
    let complete = known
        .iter()
        .map(|(filter, _)| (*filter, ListingWrite::Complete))
        .collect::<Vec<_>>();
    for id in &ids {
        if brought.contains(&id.database_id) {
            continue;
        }
        let lacking = known
            .iter()
            .any(|(_, comments)| !comments.contains(&id.database_id));
        if lacking || time.is_none_or(|time| id.updated_at >= time) {
            let comment: Comment = cx
                .endpoint(GitHubApi)
                .get(format!(
                    "/repos/{}/issues/comments/{}",
                    repo.id, id.database_id
                ))
                .json()
                .await?;
            write_comment(cx, repo, item, &complete, &comment)?;
        }
    }
    let ids = ids.iter().map(|id| id.database_id).collect::<BTreeSet<_>>();
    let others = known
        .iter()
        .flat_map(|(_, comments)| comments)
        .chain(brought)
        .filter(|id| !ids.contains(id))
        .collect::<BTreeSet<_>>();
    for (filter, _) in known {
        let comments = comments_path(repo, item, *filter);
        cx.dir(&comments, ListingWrite::Complete)?;
        for id in &others {
            cx.remove(format!("{comments}/{id}"))?;
        }
    }
    Ok(())
}

// ===========================================================================
// The feed of a repository
// ===========================================================================

/// The items of a repository in each of its folders in the projection.
struct Folders(HashMap<(ItemKind, StateFilter), BTreeSet<u64>>);

impl Folders {
    fn read(cx: &FeedCx<RepoKey>, repo: &Repository) -> Result<Self> {
        let mut folders = HashMap::new();
        for kind in [ItemKind::Issues, ItemKind::Pulls] {
            for filter in [StateFilter::All, StateFilter::Open] {
                folders.insert((kind, filter), listed(cx, &folder(repo, kind, filter))?);
            }
        }
        Ok(Self(folders))
    }

    /// The folders that list an item.
    fn of(&self, item: ItemRef) -> Vec<StateFilter> {
        [StateFilter::All, StateFilter::Open]
            .into_iter()
            .filter(|filter| self.0[&(item.kind, *filter)].contains(&item.number))
            .collect()
    }

    /// The comments of an item in each of its folders.
    fn known(
        &self,
        cx: &FeedCx<RepoKey>,
        repo: &Repository,
        item: ItemRef,
    ) -> Result<Vec<(StateFilter, BTreeSet<u64>)>> {
        self.of(item)
            .into_iter()
            .map(|filter| Ok((filter, listed(cx, &comments_path(repo, item, filter))?)))
            .collect()
    }
}

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
            Some(Stage::Check {
                since,
                next: Some(next),
                changed,
            }) => feed.check_rows(since, &next, changed).await,
            Some(Stage::Check {
                since,
                next: None,
                changed,
            }) => feed.check_end(since, &changed).await,
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

    /// Take a page of rows: the facts of their items, whose entries join the
    /// listings of their folders as `folders` says. Returns the time of the
    /// last row, and `etag` while the time stands.
    fn take_rows(
        &self,
        rows: &[ItemRow],
        etag: Option<String>,
        folders: ListingWrite,
    ) -> Result<(Option<OffsetDateTime>, Option<String>)> {
        for row in rows {
            write_item(self.cx, &self.repo, row, folders)?;
        }
        let time = rows.last().map(|row| row.updated_at).or(self.time);
        Ok((time, etag.filter(|_| time == self.time)))
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
        let (time, etag) = self.take_rows(&page.rows, etag, ListingWrite::Partial)?;
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

    /// The comments of rows written under the items of the projection, each
    /// `comments/` complete when it is complete in the projection and partial
    /// else; returns the comments written with their items.
    fn take_comments(&self, folders: &Folders, rows: &[CommentRow]) -> Result<Vec<(ItemRef, u64)>> {
        let mut taken = Vec::new();
        for row in rows {
            let item = row.item()?;
            let writes = folders
                .of(item)
                .into_iter()
                .map(|filter| {
                    let write = if complete(self.cx, &comments_path(&self.repo, item, filter))? {
                        ListingWrite::Complete
                    } else {
                        ListingWrite::Partial
                    };
                    Ok((filter, write))
                })
                .collect::<Result<Vec<_>>>()?;
            if !writes.is_empty() {
                write_comment(self.cx, &self.repo, item, &writes, &row.comment)?;
                taken.push((item, row.comment.id));
            }
        }
        Ok(taken)
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
        self.take_comments(&Folders::read(self.cx, &self.repo)?, &rows)?;
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
            &counts_query(connection),
            json!({ "after": cursor }),
        )
        .await?;
        let folders = Folders::read(self.cx, &self.repo)?;
        for node in page.nodes {
            let item = ItemRef {
                kind: connection,
                number: node.number,
            };
            let known = folders.known(self.cx, &self.repo, item)?;
            if known
                .iter()
                .any(|(_, comments)| comments.len() != node.comments.total_count)
            {
                take_ids(
                    self.cx,
                    &self.repo,
                    item,
                    &known,
                    self.time,
                    &BTreeSet::new(),
                    None,
                )
                .await?;
            } else {
                for (filter, _) in &known {
                    self.cx.dir(
                        comments_path(&self.repo, item, *filter),
                        ListingWrite::Complete,
                    )?;
                }
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

    /// The start of a check: the first page of the rows since the time, with
    /// the `ETag` of that time. For `304`, the counts, `unchanged` when they
    /// are the same.
    async fn check(&self) -> Result<Feed> {
        let Some(page) = self.first_rows().await? else {
            let folders = Folders::read(self.cx, &self.repo)?;
            return Ok(
                if self
                    .take_counts(&folders, self.time, &BTreeSet::new(), &[])
                    .await?
                {
                    repo_part(self.time, self.etag.clone(), None)?.into()
                } else {
                    Feed::unchanged()
                },
            );
        };
        let (time, etag) = self.take_rows(&page.rows, page.etag, ListingWrite::Complete)?;
        let stage = Stage::Check {
            since: self.time,
            next: page.next,
            changed: page.rows.iter().map(ItemRow::item).collect(),
        };
        Ok(repo_part(time, etag, Some(stage))?.into())
    }

    /// The next page of the rows of a check.
    async fn check_rows(
        &self,
        since: Option<OffsetDateTime>,
        next: &str,
        mut changed: BTreeSet<ItemRef>,
    ) -> Result<Feed> {
        let page = self.next_rows(next).await?;
        let (time, etag) = self.take_rows(&page.rows, self.etag.clone(), ListingWrite::Complete)?;
        changed.extend(page.rows.iter().map(ItemRow::item));
        let stage = Stage::Check {
            since,
            next: page.next,
            changed,
        };
        Ok(repo_part(time, etag, Some(stage))?.into())
    }

    /// The end of a check after its rows: for rows, the comments since the
    /// same time; then the counts.
    async fn check_end(
        &self,
        since: Option<OffsetDateTime>,
        changed: &BTreeSet<ItemRef>,
    ) -> Result<Feed> {
        let folders = Folders::read(self.cx, &self.repo)?;
        let mut brought = Vec::new();
        if !changed.is_empty() {
            for page in 1.. {
                let rows = self.comments_page(since, page).await?;
                brought.extend(self.take_comments(&folders, &rows)?);
                if rows.len() < PAGE {
                    break;
                }
            }
        }
        self.take_counts(&folders, since, changed, &brought).await?;
        Ok(repo_part(self.time, self.etag.clone(), None)?.into())
    }

    /// The counts of a check, with the first page of the ids of the comments
    /// of each changed item: the IDs of each changed item, and for another
    /// count of a kind than that of `{kind}/all/` in the projection, the
    /// numbers of the kind from GitHub GraphQL and the removal of each other
    /// item. Returns whether it wrote a fact.
    async fn take_counts(
        &self,
        folders: &Folders,
        since: Option<OffsetDateTime>,
        changed: &BTreeSet<ItemRef>,
        brought: &[(ItemRef, u64)],
    ) -> Result<bool> {
        let mut counts: Counts =
            graphql(self.cx, &self.repo, &check_query(changed), json!({})).await?;
        for item in changed {
            let commented = counts.items.remove(&alias(*item)).ok_or_else(|| {
                ProviderError::invalid_input(format!("the counts lack the item {}", item.number))
            })?;
            let brought = brought
                .iter()
                .filter(|(of, _)| of == item)
                .map(|(_, id)| *id)
                .collect();
            take_ids(
                self.cx,
                &self.repo,
                *item,
                &folders.known(self.cx, &self.repo, *item)?,
                since,
                &brought,
                Some(commented.comments),
            )
            .await?;
        }
        let mut wrote = !changed.is_empty();
        for (kind, total) in [
            (ItemKind::Issues, counts.issues.total_count),
            (ItemKind::Pulls, counts.pull_requests.total_count),
        ] {
            let listed = &folders.0[&(kind, StateFilter::All)];
            if listed.len() == total {
                continue;
            }
            let numbers = self.numbers(kind).await?;
            for number in listed.difference(&numbers) {
                let item = ItemRef {
                    kind,
                    number: *number,
                };
                for filter in [StateFilter::All, StateFilter::Open] {
                    self.cx.remove(item_path(&self.repo, item, filter))?;
                }
                wrote = true;
            }
        }
        Ok(wrote)
    }

    /// The numbers of the items of a kind, from GitHub GraphQL.
    async fn numbers(&self, kind: ItemKind) -> Result<BTreeSet<u64>> {
        let mut numbers = BTreeSet::new();
        let mut after = None;
        loop {
            let Paged::<Numbered> { page } = graphql(
                self.cx,
                &self.repo,
                &numbers_query(kind),
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
        item_feed(
            &cx,
            &Repository::of(&cx, &key.owner, &key.repo)?,
            item,
            *key.filter,
        )
        .await
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
        item_feed(
            &cx,
            &Repository::of(&cx, &key.owner, &key.repo)?,
            item,
            *key.filter,
        )
        .await
    }
}

/// The feed of an item, at its folder `filter` of the operation that asks
/// it: the projection reads of an item feed answer only under that folder.
async fn item_feed<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    filter: StateFilter,
) -> Result<Feed> {
    let token = cx
        .token()
        .map(decode::<Option<ItemToken>>)
        .transpose()?
        .flatten();
    if cx.start() == Some("comments") {
        return item_feed_comments(cx, repo, item, filter, token).await;
    }
    match item_answer(cx, repo, item, token.as_ref()).await? {
        ItemAnswer::Unchanged => Ok(Feed::unchanged()),
        ItemAnswer::Removed => removal(cx, repo, item),
        ItemAnswer::Found(row, etag) => {
            let folders = write_found(cx, repo, item, &row)?;
            let comments = comments_path(repo, item, filter);
            if let Some(previous) = token
                && complete(cx, &comments)?
            {
                let since = previous.updated_at;
                let rows = item_comments(cx, repo, item, Some(since)).await?;
                let writes = folders
                    .iter()
                    .map(|filter| (*filter, ListingWrite::Complete))
                    .collect::<Vec<_>>();
                for comment in &rows {
                    write_comment(cx, repo, item, &writes, comment)?;
                }
                let known = listed(cx, &comments)?;
                take_ids(
                    cx,
                    repo,
                    item,
                    &folders
                        .iter()
                        .map(|filter| (*filter, known.clone()))
                        .collect::<Vec<_>>(),
                    Some(since),
                    &rows.iter().map(|comment| comment.id).collect(),
                    None,
                )
                .await?;
            }
            Ok(Feed::part(encode(&Some(row.token(etag)))?).into())
        },
    }
}

/// The feed of an item started by its `comments/`: in place of its own
/// answer, which comes first only without a token, the pages of its
/// comments for a count other than `0`, a complete `comments/`, and its IDs
/// for another count than that of its token.
async fn item_feed_comments<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    filter: StateFilter,
    token: Option<ItemToken>,
) -> Result<Feed> {
    let (token, folders) = match token {
        // Without the state of the item, its folders are `all` and the
        // folder of the operation, which lists it.
        Some(token) => (
            token,
            match filter {
                StateFilter::All => vec![StateFilter::All],
                StateFilter::Open => vec![StateFilter::All, StateFilter::Open],
            },
        ),
        None => match item_answer(cx, repo, item, None).await? {
            ItemAnswer::Found(row, etag) => {
                let folders = write_found(cx, repo, item, &row)?;
                (row.token(etag), folders)
            },
            ItemAnswer::Removed => return removal(cx, repo, item),
            ItemAnswer::Unchanged => return Err(unconditional_304("the item")),
        },
    };
    let known = listed(cx, &comments_path(repo, item, filter))?;
    let rows = if token.comments == 0 {
        Vec::new()
    } else {
        item_comments(cx, repo, item, None).await?
    };
    let writes = folders
        .iter()
        .map(|filter| (*filter, ListingWrite::Complete))
        .collect::<Vec<_>>();
    for comment in &rows {
        write_comment(cx, repo, item, &writes, comment)?;
    }
    if rows.len() == token.comments {
        for filter in &folders {
            cx.dir(comments_path(repo, item, *filter), ListingWrite::Complete)?;
        }
    } else {
        take_ids(
            cx,
            repo,
            item,
            &folders
                .iter()
                .map(|filter| (*filter, known.clone()))
                .collect::<Vec<_>>(),
            Some(token.updated_at),
            &rows.iter().map(|comment| comment.id).collect(),
            None,
        )
        .await?;
    }
    Ok(Feed::part(encode(&Some(token))?).into())
}

/// The facts of the answer of an item, not found for an item of the other
/// kind. Returns the folders of the item.
fn write_found<K>(
    cx: &FeedCx<K>,
    repo: &Repository,
    item: ItemRef,
    row: &ItemRow,
) -> Result<Vec<StateFilter>> {
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
    for filter in [StateFilter::All, StateFilter::Open] {
        cx.remove(item_path(repo, item, filter))?;
    }
    Ok(Feed::part(encode(&None::<ItemToken>)?).into())
}
