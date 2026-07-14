//! Path keys, object loads, collection methods, and live faces for the GitHub
//! provider.

use rc_zip_sync::ReadZip;
use serde::Deserialize;
use serde_json::value::RawValue;

use omnifs_core::ContentType;
use omnifs_sdk::prelude::*;

use crate::api::GitHubApi;
use crate::objects::{
    ChangedFile, CheckRun, Comment, Issue, ItemData, Notification, Owner, PullRequest, Repo,
    Review, ReviewComment, WorkflowRun,
};
use crate::{
    CHECK_RUN_PAGE_SIZE, COMMENT_PAGE_SIZE, CheckRunsResponse, FILE_PAGE_SIZE, FilePath,
    NOTIFICATION_PAGE_SIZE, OwnerName, REVIEW_PAGE_SIZE, RepoId, RepoName, StateFilter, ThreadId,
    WorkflowRunsResponse, resolve_owner_kind,
};

/// Identity discriminator for the comment anchor's `{item_kind}` segment. A
/// plain `PathSegment` capture (NOT a facet): an issue comment and a pull
/// comment are distinct objects, so `item_kind` is part of identity.
#[omnifs_sdk::path_segment]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemKind {
    #[strum(serialize = "issues")]
    Issues,
    #[strum(serialize = "pulls")]
    Pulls,
}

impl ItemKind {
    const fn rest_resource(self) -> &'static str {
        match self {
            Self::Issues => "issues",
            Self::Pulls => "pulls",
        }
    }

    async fn list_page(
        self,
        cx: &Cx,
        owner: &OwnerName,
        repo: &RepoName,
        filter: StateFilter,
        page: u64,
    ) -> Result<Vec<ItemData>> {
        let resource = self.rest_resource();
        let state = filter.rest_state();
        cx.endpoint(GitHubApi)
            .get(format!(
                "/repos/{owner}/{repo}/{resource}?state={state}\
                 &sort=created&direction=desc&per_page={ITEM_PAGE_SIZE}&page={page}"
            ))
            .json()
            .await
    }
}

const ITEM_PAGE_SIZE: u64 = 100;

// ===========================================================================
// Keys
// ===========================================================================

#[omnifs_sdk::path_captures]
pub(crate) struct OwnerKey {
    pub(crate) owner: OwnerName,
}

#[omnifs_sdk::path_captures]
pub(crate) struct RepoKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
}

#[omnifs_sdk::path_captures]
pub(crate) struct IssueKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[allow(dead_code)]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct PullKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[allow(dead_code)]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ChangedFileKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[allow(dead_code)]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
    pub(crate) path: FilePath,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ReviewKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[allow(dead_code)]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
    pub(crate) review_id: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ReviewCommentKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[allow(dead_code)]
    pub(crate) filter: Facet<StateFilter>,
    #[allow(dead_code)]
    pub(crate) number: u64,
    #[allow(dead_code)]
    pub(crate) review_id: u64,
    pub(crate) comment_id: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct CheckRunKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[allow(dead_code)]
    pub(crate) filter: Facet<StateFilter>,
    #[allow(dead_code)]
    pub(crate) number: u64,
    pub(crate) check_run_id: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct CommentKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    pub(crate) item_kind: ItemKind,
    #[allow(dead_code)]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
    pub(crate) comment_id: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct RunKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    pub(crate) run_id: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct NotificationKey {
    pub(crate) thread_id: ThreadId,
}

/// The collection-dir key for `issues/{filter}` and `pulls/{filter}`: it
/// carries the `{filter}` segment so the listing reads the real filter
/// (`ls issues/all` lists closed/all items, not just open). The parent anchor
/// key (`RepoKey`) has no filter; the collection method binds this instead.
#[omnifs_sdk::path_captures]
pub(crate) struct ItemListKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    pub(crate) filter: StateFilter,
}

// ===========================================================================
// Object loads (forwarded to by the #[object] macro)
// ===========================================================================

impl Owner {
    pub(crate) async fn load(
        cx: &Cx,
        key: &OwnerKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        // GitHub serves user and org profiles at distinct paths; probe the user
        // endpoint first, then orgs, mirroring `resolve_owner_kind`.
        match cx
            .endpoint(GitHubApi)
            .get(format!("/users/{}", key.owner))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
        {
            Ok(Load::NotFound) => {},
            other => return other,
        }
        cx.endpoint(GitHubApi)
            .get(format!("/orgs/{}", key.owner))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
    }
}

impl Repo {
    pub(crate) async fn load(
        cx: &Cx,
        key: &RepoKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        cx.endpoint(GitHubApi)
            .get(format!("/repos/{}/{}", key.owner, key.repo))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
    }
}

impl Issue {
    pub(crate) async fn load(
        cx: &Cx,
        key: &IssueKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        match cx
            .endpoint(GitHubApi)
            .get(format!("/repos/{repo}/issues/{}", key.number))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<ItemData>)
            .await?
        {
            Load::Fresh { value, .. } if value.is_pull_request() => Ok(Load::NotFound),
            Load::Fresh {
                value, canonical, ..
            } => Ok(Load::fresh(Self(value), canonical)),
            Load::Unchanged => Ok(Load::Unchanged),
            Load::NotFound => Ok(Load::NotFound),
        }
    }
}

impl PullRequest {
    pub(crate) async fn load(
        cx: &Cx,
        key: &PullKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        match cx
            .endpoint(GitHubApi)
            .get(format!("/repos/{repo}/pulls/{}", key.number))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<ItemData>)
            .await?
        {
            Load::Fresh {
                value, canonical, ..
            } => Ok(Load::fresh(Self(value), canonical)),
            Load::Unchanged => Ok(Load::Unchanged),
            Load::NotFound => Ok(Load::NotFound),
        }
    }
}

impl ChangedFile {
    pub(crate) async fn load(
        cx: &Cx,
        key: &ChangedFileKey,
        _since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let filename = key.path.decoded()?;
        let Some((file, raw)) = find_pull_file(cx, key, &filename).await? else {
            return Ok(Load::NotFound);
        };
        Ok(Load::fresh(
            file,
            Canonical::new(raw.get().as_bytes(), None),
        ))
    }
}

impl Review {
    pub(crate) async fn load(
        cx: &Cx,
        key: &ReviewKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        cx.endpoint(GitHubApi)
            .get(format!(
                "/repos/{repo}/pulls/{}/reviews/{}",
                key.number, key.review_id
            ))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
    }
}

impl ReviewComment {
    pub(crate) async fn load(
        cx: &Cx,
        key: &ReviewCommentKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        cx.endpoint(GitHubApi)
            .get(format!("/repos/{repo}/pulls/comments/{}", key.comment_id))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
    }
}

impl CheckRun {
    pub(crate) async fn load(
        cx: &Cx,
        key: &CheckRunKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        cx.endpoint(GitHubApi)
            .get(format!("/repos/{repo}/check-runs/{}", key.check_run_id))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
    }
}

impl Comment {
    pub(crate) async fn load(
        cx: &Cx,
        key: &CommentKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        // Comments live under the shared issues endpoint regardless of whether
        // the parent is an issue or a pull.
        cx.endpoint(GitHubApi)
            .get(format!("/repos/{repo}/issues/comments/{}", key.comment_id))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
    }
}

impl WorkflowRun {
    pub(crate) async fn load(
        cx: &Cx,
        key: &RunKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        cx.endpoint(GitHubApi)
            .get(format!("/repos/{repo}/actions/runs/{}", key.run_id))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
    }
}

impl Notification {
    pub(crate) async fn load(
        cx: &Cx,
        key: &NotificationKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        cx.endpoint(GitHubApi)
            .get(format!("/notifications/threads/{}", key.thread_id))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
    }
}

// ===========================================================================
// Collections
// ===========================================================================

#[derive(Debug, Deserialize)]
struct RepoListing {
    name: String,
}

impl Owner {
    /// Anchor-topology collection: the repo names listed under `/{owner}` ARE
    /// the child `Repo` anchors (`/{owner}/{repo}`). The repo listing carries
    /// only the name, not the single-repo canonical, so entries are `key`.
    pub(crate) async fn repos(
        key: OwnerKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<Repo, PageCursor>> {
        let kind = resolve_owner_kind(&cx, &key.owner)
            .await?
            .ok_or_else(|| ProviderError::not_found("owner not found"))?;
        let page = cx.cursor().map_or(1, |cursor| cursor.0);
        let scope = match kind {
            crate::OwnerKind::User => "users",
            crate::OwnerKind::Org => "orgs",
        };
        let repos: Vec<RepoListing> = cx
            .endpoint(GitHubApi)
            .get(format!(
                "/{scope}/{}/repos?sort=full_name&direction=asc&per_page=100&page={page}",
                key.owner
            ))
            .json()
            .await?;
        let len = repos.len() as u64;
        let entries = repos
            .into_iter()
            .filter_map(|row| {
                row.name.parse::<RepoName>().ok().map(|repo| {
                    CollectionEntry::key(RepoKey {
                        owner: key.owner.clone(),
                        repo,
                    })
                })
            })
            .collect();
        page_or_complete(entries, len, 100, page)
    }
}

impl Repo {
    pub(crate) async fn issues(
        key: ItemListKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<Issue, PageCursor>> {
        let filter = key.filter;
        let page = cx.cursor().map_or(1, |cursor| cursor.0);
        let items = ItemKind::Issues
            .list_page(&cx, &key.owner, &key.repo, filter, page)
            .await?;
        let len = items.len() as u64;
        let entries = items
            .iter()
            .filter(|item| !item.is_pull_request())
            .map(|item| {
                CollectionEntry::computed(
                    IssueKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: Facet(filter),
                        number: item.number,
                    },
                    item.eager_leaves(),
                )
            })
            .collect::<Vec<_>>();
        page_or_complete(entries, len, ITEM_PAGE_SIZE, page)
    }

    pub(crate) async fn pulls(
        key: ItemListKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<PullRequest, PageCursor>> {
        let filter = key.filter;
        let page = cx.cursor().map_or(1, |cursor| cursor.0);
        let items = ItemKind::Pulls
            .list_page(&cx, &key.owner, &key.repo, filter, page)
            .await?;
        let len = items.len() as u64;
        let entries = items
            .iter()
            .map(|item| {
                CollectionEntry::computed(
                    PullKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: Facet(filter),
                        number: item.number,
                    },
                    item.eager_leaves(),
                )
            })
            .collect::<Vec<_>>();
        page_or_complete(entries, len, ITEM_PAGE_SIZE, page)
    }

    pub(crate) async fn workflow_runs(
        key: RepoKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<WorkflowRun, PageCursor>> {
        let page = cx.cursor().map_or(1, |cursor| cursor.0);
        let repo_id = RepoId::new(&key.owner, &key.repo);
        let runs: WorkflowRunsResponse = cx
            .endpoint(GitHubApi)
            .get(format!(
                "/repos/{repo_id}/actions/runs?per_page=30&page={page}"
            ))
            .json()
            .await?;
        let len = runs.workflow_runs.len() as u64;
        let entries = runs
            .workflow_runs
            .into_iter()
            .map(|run| {
                let files = vec![
                    ("status".to_string(), inline_text(&run.status)),
                    (
                        "conclusion".to_string(),
                        inline_text(run.conclusion.as_deref().unwrap_or("")),
                    ),
                ];
                CollectionEntry::computed(
                    RunKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        run_id: run.id,
                    },
                    files,
                )
            })
            .collect();
        page_or_complete(entries, len, 30, page)
    }
}

impl Issue {
    pub(crate) async fn comments(
        key: IssueKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<Comment, PageCursor>> {
        let page = cx.cursor().map_or(1, |c| c.0);
        comments_collection(
            &cx,
            &key.owner,
            &key.repo,
            ItemKind::Issues,
            *key.filter,
            key.number,
            page,
        )
        .await
    }
}

impl PullRequest {
    pub(crate) async fn comments(
        key: PullKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<Comment, PageCursor>> {
        let page = cx.cursor().map_or(1, |c| c.0);
        comments_collection(
            &cx,
            &key.owner,
            &key.repo,
            ItemKind::Pulls,
            *key.filter,
            key.number,
            page,
        )
        .await
    }

    pub(crate) async fn files(
        key: PullKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<ChangedFile, PageCursor>> {
        let page = cx.cursor().map_or(1, |c| c.0);
        let files = pull_files_page(&cx, &key.owner, &key.repo, key.number, page).await?;
        let len = files.len() as u64;
        let entries = files
            .into_iter()
            .filter_map(|(file, _raw)| {
                let path = FilePath::from_github_path(&file.filename)?;
                Some(CollectionEntry::computed(
                    ChangedFileKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: key.filter,
                        number: key.number,
                        path,
                    },
                    file.eager_leaves(),
                ))
            })
            .collect::<Vec<_>>();
        page_or_complete(entries, len, FILE_PAGE_SIZE, page)
    }

    pub(crate) async fn reviews(
        key: PullKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<Review, PageCursor>> {
        let page = cx.cursor().map_or(1, |c| c.0);
        let reviews = reviews_page(&cx, &key.owner, &key.repo, key.number, page).await?;
        let len = reviews.len() as u64;
        let entries = reviews
            .into_iter()
            .map(|review| {
                let review_id = review.id;
                CollectionEntry::computed(
                    ReviewKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: key.filter,
                        number: key.number,
                        review_id,
                    },
                    review.eager_leaves(),
                )
            })
            .collect::<Vec<_>>();
        page_or_complete(entries, len, REVIEW_PAGE_SIZE, page)
    }

    pub(crate) async fn checks(
        key: PullKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<CheckRun, PageCursor>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        let pull: PullHeadResponse = cx
            .endpoint(GitHubApi)
            .get(format!("/repos/{repo}/pulls/{}", key.number))
            .json()
            .await?;
        let page = cx.cursor().map_or(1, |c| c.0);
        let runs: CheckRunsResponse = cx
            .endpoint(GitHubApi)
            .get(format!(
                "/repos/{repo}/commits/{}/check-runs?per_page={CHECK_RUN_PAGE_SIZE}&page={page}",
                pull.head.sha
            ))
            .json()
            .await?;
        let len = runs.check_runs.len() as u64;
        let entries = runs
            .check_runs
            .into_iter()
            .map(|check| {
                let check_run_id = check.id;
                CollectionEntry::computed(
                    CheckRunKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: key.filter,
                        number: key.number,
                        check_run_id,
                    },
                    check.eager_leaves(),
                )
            })
            .collect::<Vec<_>>();
        page_or_complete(entries, len, CHECK_RUN_PAGE_SIZE, page)
    }
}

impl Review {
    pub(crate) async fn comments(
        key: ReviewKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<ReviewComment, PageCursor>> {
        let page = cx.cursor().map_or(1, |c| c.0);
        let comments =
            review_comments_page(&cx, &key.owner, &key.repo, key.number, key.review_id, page)
                .await?;
        let len = comments.len() as u64;
        let entries = comments
            .into_iter()
            .map(|comment| {
                let comment_id = comment.id;
                CollectionEntry::computed(
                    ReviewCommentKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: key.filter,
                        number: key.number,
                        review_id: key.review_id,
                        comment_id,
                    },
                    comment.eager_leaves(),
                )
            })
            .collect::<Vec<_>>();
        page_or_complete(entries, len, REVIEW_PAGE_SIZE, page)
    }
}

impl Notification {
    pub(crate) async fn list(cx: DirCx) -> Result<DirListing> {
        match cx.intent() {
            DirIntent::Lookup { child } => {
                let entries = child
                    .strip_prefix("thread-")
                    .and_then(|id| id.parse::<ThreadId>().ok())
                    .map(|_| Entry::dir(child.clone()));
                Ok(DirListing::exhaustive(entries))
            },
            DirIntent::ReadFile { .. } => Ok(DirListing::exhaustive([])),
            DirIntent::List { .. } => {
                let page = u64::from(cx.page_cursor(1));
                let notifications = notifications_page(&cx, page).await?;
                let len = notifications.len() as u64;
                let entries = notifications
                    .iter()
                    .filter_map(|notification| {
                        notification
                            .id
                            .parse::<ThreadId>()
                            .ok()
                            .map(|_| Entry::dir(format!("thread-{}", notification.id)))
                    })
                    .collect::<Vec<_>>();
                let mut listing = if len < NOTIFICATION_PAGE_SIZE {
                    DirListing::exhaustive(entries)
                } else {
                    DirListing::paged(entries, Cursor::Page(cx.page_cursor(1) + 1))
                };
                for notification in notifications {
                    if notification.id.parse::<ThreadId>().is_ok() {
                        listing = listing.preload_file(
                            format!("thread-{}/item.md", notification.id),
                            FileProjection::inline(notification.item_markdown())
                                .content_type(ContentType::Markdown)
                                .dynamic()
                                .build(),
                        );
                    }
                }
                Ok(listing)
            },
        }
    }
}

// ===========================================================================
// Live faces (blob, tree, direct)
// ===========================================================================

impl PullRequest {
    pub(crate) async fn diff(cx: Cx, key: PullKey) -> Result<BlobFile<omnifs_sdk::repr::Json>> {
        let repo_id = RepoId::new(&key.owner, &key.repo);
        let blob = cx
            .endpoint(GitHubApi)
            .get(format!("/repos/{repo_id}/pulls/{}", key.number))
            .header("Accept", "application/vnd.github.diff")
            .into_blob()
            .cache_key(format!("github/pulls/{repo_id}/{}/diff.patch", key.number))
            .fetch()
            .await?;
        Ok(BlobFile::new(blob.id)
            .size(Size::Exact(blob.size))
            .content_type(ContentType::Custom("text/x-diff")))
    }
}

impl Repo {
    pub(crate) async fn tree(cx: Cx, key: RepoKey) -> Result<TreeRef> {
        let repo_id = RepoId::new(&key.owner, &key.repo);
        let opened = cx
            .git()
            .open_repo(
                format!("github.com/{repo_id}"),
                format!("git@github.com:{repo_id}.git"),
            )
            .await?;
        Ok(TreeRef::new(opened.tree))
    }
}

impl WorkflowRun {
    pub(crate) async fn log(cx: Cx, key: RunKey) -> Result<FileProjection> {
        let repo = RepoId::new(&key.owner, &key.repo);
        let resp = cx
            .endpoint(GitHubApi)
            .get(format!("/repos/{repo}/actions/runs/{}/logs", key.run_id))
            .send_checked()
            .await?;
        Ok(FileProjection::body(unzip_logs(resp.body()))
            .dynamic()
            .build())
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

/// A dynamic `text/plain` preload leaf: a mutable upstream field cached at
/// listing time, so it revalidates rather than pinning a stale value.
fn inline_text(s: &str) -> FileProjection {
    FileProjection::text(s, TextFormat::Raw).dynamic().build()
}

/// The shallow eager leaves an issue/PR listing row can fill without the
/// single-item canonical: the tiny `title`/`state`/`user` fields. `body`,
/// `item.md`, and `item.json` come from the canonical, which the lossy row
/// cannot reproduce, so they load on first read.
impl ItemData {
    fn eager_leaves(&self) -> Vec<(String, FileProjection)> {
        let login = self.user.as_ref().map_or("", |user| user.login.as_str());
        vec![
            ("title".to_string(), inline_text(&self.title)),
            ("state".to_string(), inline_text(&self.state)),
            ("user".to_string(), inline_text(login)),
        ]
    }
}

impl ChangedFile {
    fn eager_leaves(&self) -> Vec<(String, FileProjection)> {
        vec![
            ("filename".to_string(), inline_text(&self.filename)),
            ("status".to_string(), inline_text(&self.status)),
        ]
    }
}

impl Review {
    fn eager_leaves(&self) -> Vec<(String, FileProjection)> {
        let login = self.user.as_ref().map_or("", |user| user.login.as_str());
        vec![
            (
                "state".to_string(),
                inline_text(self.state.as_deref().unwrap_or("")),
            ),
            ("user".to_string(), inline_text(login)),
        ]
    }
}

impl ReviewComment {
    fn eager_leaves(&self) -> Vec<(String, FileProjection)> {
        let login = self.user.as_ref().map_or("", |user| user.login.as_str());
        vec![
            ("author".to_string(), inline_text(login)),
            (
                "path".to_string(),
                inline_text(self.path.as_deref().unwrap_or("")),
            ),
        ]
    }
}

impl CheckRun {
    fn eager_leaves(&self) -> Vec<(String, FileProjection)> {
        vec![
            ("name".to_string(), inline_text(&self.name)),
            ("status".to_string(), inline_text(&self.status)),
            (
                "conclusion".to_string(),
                inline_text(self.conclusion.as_deref().unwrap_or("")),
            ),
        ]
    }
}

/// The shallow eager leaves a comment listing row can fill from the lossy list
/// payload: `author` and `body.md` render the same bytes from any source, so
/// they preload at listing time. `comment.json`/`comment.md` come from the
/// verbatim standalone GET, which the list response cannot reproduce, so they
/// load on first read.
impl Comment {
    fn eager_leaves(&self, key: &CommentKey) -> Result<Vec<(String, FileProjection)>> {
        Ok(vec![
            ("author".to_string(), self.author(key)?),
            ("body.md".to_string(), self.body_md(key)?),
        ])
    }
}

fn page_or_complete<T: omnifs_sdk::object::Object>(
    entries: Vec<CollectionEntry<T>>,
    len: u64,
    page_size: u64,
    page: u64,
) -> Result<Collection<T, PageCursor>> {
    if len < page_size {
        Ok(Collection::complete(entries))
    } else {
        Ok(Collection::page(entries).next(PageCursor(page + 1)))
    }
}

async fn pull_files_page(
    cx: &Cx,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
    page: u64,
) -> Result<Vec<(ChangedFile, Box<RawValue>)>> {
    let repo = RepoId::new(owner, repo);
    let raw_files: Vec<Box<RawValue>> = cx
        .endpoint(GitHubApi)
        .get(format!(
            "/repos/{repo}/pulls/{number}/files?per_page={FILE_PAGE_SIZE}&page={page}"
        ))
        .json()
        .await?;
    raw_files
        .into_iter()
        .map(|raw| {
            let file = serde_json::from_str::<ChangedFile>(raw.get())
                .map_err(|err| ProviderError::invalid_input(format!("json decode: {err}")))?;
            Ok((file, raw))
        })
        .collect()
}

async fn find_pull_file(
    cx: &Cx,
    key: &ChangedFileKey,
    filename: &str,
) -> Result<Option<(ChangedFile, Box<RawValue>)>> {
    const MAX_FILE_PAGES: u64 = 30;

    for page in 1..=MAX_FILE_PAGES {
        let files = pull_files_page(cx, &key.owner, &key.repo, key.number, page).await?;
        let len = files.len() as u64;
        if let Some(file) = files
            .into_iter()
            .find(|(file, _raw)| file.filename == filename)
        {
            return Ok(Some(file));
        }
        if len < FILE_PAGE_SIZE {
            return Ok(None);
        }
    }
    Ok(None)
}

async fn reviews_page(
    cx: &Cx,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
    page: u64,
) -> Result<Vec<Review>> {
    let repo = RepoId::new(owner, repo);
    cx.endpoint(GitHubApi)
        .get(format!(
            "/repos/{repo}/pulls/{number}/reviews?per_page={REVIEW_PAGE_SIZE}&page={page}"
        ))
        .json()
        .await
}

async fn review_comments_page(
    cx: &Cx,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
    review_id: u64,
    page: u64,
) -> Result<Vec<ReviewComment>> {
    let repo = RepoId::new(owner, repo);
    cx.endpoint(GitHubApi)
        .get(format!(
            "/repos/{repo}/pulls/{number}/reviews/{review_id}/comments?per_page={REVIEW_PAGE_SIZE}&page={page}"
        ))
        .json()
        .await
}

async fn notifications_page(cx: &Cx, page: u64) -> Result<Vec<Notification>> {
    cx.endpoint(GitHubApi)
        .get(format!(
            "/notifications?per_page={NOTIFICATION_PAGE_SIZE}&page={page}"
        ))
        .json()
        .await
}

#[derive(Debug, Deserialize)]
struct PullHeadResponse {
    head: PullHead,
}

#[derive(Debug, Deserialize)]
struct PullHead {
    sha: String,
}

async fn comments_collection(
    cx: &Cx,
    owner: &OwnerName,
    repo: &RepoName,
    item_kind: ItemKind,
    filter: StateFilter,
    number: u64,
    page: u64,
) -> Result<Collection<Comment, PageCursor>> {
    // The list response is a lossy view: serde_json would re-serialize each row
    // key-sorted and compact, which is NOT byte-identical to the verbatim
    // standalone comment GET that `Comment::load` stores. Storing that as the
    // canonical would content-address `comment.json` inconsistently. So list
    // only the eagerly-derivable leaves (author, body.md, which render the same
    // from any source) and let `comment.json`/`comment.md` load from the
    // verbatim GET on first read, mirroring issues/pulls/runs.
    let comments: Vec<Comment> = cx
        .endpoint(GitHubApi)
        .get(format!(
            "/repos/{owner}/{repo}/issues/{number}/comments?per_page={COMMENT_PAGE_SIZE}&page={page}"
        ))
        .json()
        .await?;
    let len = comments.len() as u64;
    let mut entries = Vec::with_capacity(comments.len());
    for comment in comments {
        let key = CommentKey {
            owner: owner.clone(),
            repo: repo.clone(),
            item_kind,
            filter: Facet(filter),
            number,
            comment_id: comment.id,
        };
        let files = comment.eager_leaves(&key)?;
        entries.push(CollectionEntry::computed(key, files));
    }
    page_or_complete(entries, len, COMMENT_PAGE_SIZE, page)
}

pub(crate) fn unzip_logs(bytes: &[u8]) -> Vec<u8> {
    let Ok(archive) = bytes.read_zip() else {
        return bytes.to_vec();
    };
    let mut output = Vec::new();
    for entry in archive.entries() {
        if entry.name.ends_with('/') {
            continue;
        }
        output.extend_from_slice(format!("=== {} ===\n", entry.name).as_bytes());
        if let Ok(data) = entry.bytes() {
            output.extend_from_slice(&data);
        }
        if !output.ends_with(b"\n") {
            output.push(b'\n');
        }
        if output.len() >= 10 * 1024 * 1024 {
            output.truncate(10 * 1024 * 1024);
            output.extend_from_slice(b"\n[truncated at 10MB]\n");
            return output;
        }
    }
    output
}
