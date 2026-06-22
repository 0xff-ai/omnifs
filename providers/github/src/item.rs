//! Path keys, object loads, and route handlers for the GitHub provider.

use rc_zip_sync::ReadZip;

use omnifs_core::ContentType;
use omnifs_sdk::prelude::*;

use crate::api::GithubRest;
use crate::objects::{Issue, ItemData, PullRequest, Repo, Run};
use crate::{
    COMMENT_PAGE_SIZE, CommentRecord, ListPage, OwnerName, RepoId, RepoName, StateFilter,
    WorkflowRunsResponse, fetch_owner_repos, github_check_status, list_items, resolve_owner_kind,
};

/// List-only discriminator for the search+REST issue listing seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemKind {
    Issues,
    Pulls,
}

impl ItemKind {
    pub(crate) const fn search_qualifier(self) -> &'static str {
        match self {
            Self::Issues => "+is:issue",
            Self::Pulls => "+is:pr",
        }
    }

    pub(crate) const fn rest_resource(self) -> &'static str {
        match self {
            Self::Issues => "issues",
            Self::Pulls => "pulls",
        }
    }
}

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
pub(crate) struct IssuesRootKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
}

#[omnifs_sdk::path_captures]
pub(crate) struct PullsRootKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
}

#[omnifs_sdk::path_captures]
pub(crate) struct IssueListKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    pub(crate) filter: Facet<StateFilter>,
}

#[omnifs_sdk::path_captures]
pub(crate) struct PullListKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    pub(crate) filter: Facet<StateFilter>,
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
pub(crate) struct IssueCommentKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[allow(dead_code)]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
    pub(crate) idx: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct PullCommentKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[allow(dead_code)]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
    pub(crate) idx: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct RunListKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
}

#[omnifs_sdk::path_captures]
pub(crate) struct RunKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    pub(crate) run_id: u64,
}

impl OwnerKey {
    pub(crate) async fn repos(self, cx: DirCx) -> Result<DirProjection> {
        let kind = resolve_owner_kind(&cx, &self.owner)
            .await?
            .ok_or_else(|| ProviderError::not_found("owner not found"))?;
        let mut names = fetch_owner_repos(&cx, &self.owner, kind).await?;
        names.sort();
        Ok(DirProjection::exhaustive(names.into_iter().map(Entry::dir)))
    }
}

impl IssuesRootKey {
    #[allow(clippy::unused_self)]
    pub(crate) fn filters(self, _cx: DirCx) -> Result<DirProjection> {
        Ok(DirProjection::exhaustive(
            StateFilter::choices()
                .into_iter()
                .flatten()
                .map(|&name| Entry::dir(name.to_string())),
        ))
    }
}

impl PullsRootKey {
    #[allow(clippy::unused_self)]
    pub(crate) fn filters(self, _cx: DirCx) -> Result<DirProjection> {
        Ok(DirProjection::exhaustive(
            StateFilter::choices()
                .into_iter()
                .flatten()
                .map(|&name| Entry::dir(name.to_string())),
        ))
    }
}

impl IssueListKey {
    pub(crate) async fn list(self, cx: DirCx) -> Result<DirProjection> {
        let route = ItemListRoute::Issues(self);
        let page = list_items(
            &cx,
            route.owner(),
            route.repo(),
            route.kind(),
            route.filter(),
        )
        .await?;
        page.project(route)
    }
}

impl PullListKey {
    pub(crate) async fn list(self, cx: DirCx) -> Result<DirProjection> {
        let route = ItemListRoute::Pulls(self);
        let page = list_items(
            &cx,
            route.owner(),
            route.repo(),
            route.kind(),
            route.filter(),
        )
        .await?;
        page.project(route)
    }
}

impl ListPage {
    fn project(self, route: ItemListRoute) -> Result<DirProjection> {
        let visible_items: Vec<_> = self
            .items
            .iter()
            .filter(|item| route.includes(item))
            .collect();
        let entries = visible_items
            .iter()
            .map(|item| Entry::dir(item.number.to_string()));
        let mut projection = if self.exhaustive {
            DirProjection::exhaustive(entries)
        } else {
            DirProjection::open(entries)
        };
        // One aggregate eager budget across the whole listing: the host rejects
        // a terminal response whose inline preloads exceed it, so per-item body
        // inlining draws down a shared pool rather than each item assuming the
        // full cap.
        let mut budget = MAX_EAGER_RESPONSE_BYTES;
        for item in visible_items {
            projection = projection.preload_dir(
                route.anchor(item.number),
                route.listed_item_dir(item, &mut budget)?,
            );
        }
        Ok(projection)
    }
}

enum ItemListRoute {
    Issues(IssueListKey),
    Pulls(PullListKey),
}

impl ItemListRoute {
    fn owner(&self) -> &OwnerName {
        match self {
            Self::Issues(key) => &key.owner,
            Self::Pulls(key) => &key.owner,
        }
    }

    fn repo(&self) -> &RepoName {
        match self {
            Self::Issues(key) => &key.repo,
            Self::Pulls(key) => &key.repo,
        }
    }

    fn filter(&self) -> StateFilter {
        match self {
            Self::Issues(key) => *key.filter,
            Self::Pulls(key) => *key.filter,
        }
    }

    const fn kind(&self) -> ItemKind {
        match self {
            Self::Issues(_) => ItemKind::Issues,
            Self::Pulls(_) => ItemKind::Pulls,
        }
    }

    fn includes(&self, item: &ItemData) -> bool {
        match self {
            Self::Issues(_) => !item.is_pull_request(),
            Self::Pulls(_) => true,
        }
    }

    fn anchor(&self, number: u64) -> String {
        match self {
            Self::Issues(key) => {
                format!(
                    "/{}/{}/issues/{}/{}",
                    key.owner, key.repo, *key.filter, number
                )
            },
            Self::Pulls(key) => {
                format!(
                    "/{}/{}/pulls/{}/{}",
                    key.owner, key.repo, *key.filter, number
                )
            },
        }
    }

    fn listed_item_dir(&self, item: &ItemData, budget: &mut usize) -> Result<DirProjection> {
        item.listed_dir(matches!(self, Self::Pulls(_)), budget)
    }
}

impl Key for IssueKey {
    type Object = Issue;
    type State = ();

    async fn load(&self, cx: &Cx, since: Option<Validator>) -> Result<Load<Issue>> {
        let repo = RepoId::new(&self.owner, &self.repo);
        match cx
            .github_load::<ItemData>(format!("/repos/{repo}/issues/{}", self.number), since)
            .await?
        {
            Load::Fresh { value, .. } if value.is_pull_request() => Ok(Load::NotFound),
            Load::Fresh {
                value,
                canonical,
                effects,
            } => Ok(Load::fresh_with_effects(Issue(value), canonical, effects)),
            Load::Unchanged => Ok(Load::Unchanged),
            Load::NotFound => Ok(Load::NotFound),
        }
    }
}

impl Key for PullKey {
    type Object = PullRequest;
    type State = ();

    async fn load(&self, cx: &Cx, since: Option<Validator>) -> Result<Load<PullRequest>> {
        let repo = RepoId::new(&self.owner, &self.repo);
        match cx
            .github_load::<ItemData>(format!("/repos/{repo}/pulls/{}", self.number), since)
            .await?
        {
            Load::Fresh {
                value,
                canonical,
                effects,
            } => Ok(Load::fresh_with_effects(
                PullRequest(value),
                canonical,
                effects,
            )),
            Load::Unchanged => Ok(Load::Unchanged),
            Load::NotFound => Ok(Load::NotFound),
        }
    }
}

impl Key for RepoKey {
    type Object = Repo;
    type State = ();

    async fn load(&self, cx: &Cx, since: Option<Validator>) -> Result<Load<Repo>> {
        cx.github_load::<Repo>(format!("/repos/{}/{}", self.owner, self.repo), since)
            .await
    }
}

impl Key for RunKey {
    type Object = Run;
    type State = ();

    async fn load(&self, cx: &Cx, since: Option<Validator>) -> Result<Load<Run>> {
        let repo = RepoId::new(&self.owner, &self.repo);
        cx.github_load::<Run>(format!("/repos/{repo}/actions/runs/{}", self.run_id), since)
            .await
    }
}

impl RepoKey {
    pub(crate) async fn tree(self, cx: Cx) -> Result<TreeRef> {
        let repo_id = RepoId::new(&self.owner, &self.repo);
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

impl IssueKey {
    pub(crate) async fn comments(self, cx: DirCx) -> Result<DirProjection> {
        comments_dir(&cx, &self.owner, &self.repo, self.number).await
    }
}

impl PullKey {
    pub(crate) async fn comments(self, cx: DirCx) -> Result<DirProjection> {
        comments_dir(&cx, &self.owner, &self.repo, self.number).await
    }

    pub(crate) async fn diff(self, cx: Cx) -> Result<FileProjection> {
        let repo_id = RepoId::new(&self.owner, &self.repo);
        let blob = cx
            .github_get(format!("/repos/{repo_id}/pulls/{}", self.number))
            .header("Accept", "application/vnd.github.diff")
            .into_blob()
            .with_cache_key(format!("github/pulls/{repo_id}/{}/diff", self.number))
            .send()
            .await?
            .error_for_status()?;
        Ok(FileProjection::blob(blob.id())
            .size(Size::Exact(blob.size))
            .dynamic()
            .content_type(ContentType::Custom("text/x-diff"))
            .build())
    }
}

impl IssueCommentKey {
    pub(crate) async fn read(self, cx: Cx) -> Result<FileProjection> {
        comment_read(&cx, &self.owner, &self.repo, self.number, self.idx).await
    }
}

impl PullCommentKey {
    pub(crate) async fn read(self, cx: Cx) -> Result<FileProjection> {
        comment_read(&cx, &self.owner, &self.repo, self.number, self.idx).await
    }
}

impl RunListKey {
    pub(crate) async fn list(self, cx: DirCx) -> Result<DirProjection> {
        let repo_id = RepoId::new(&self.owner, &self.repo);
        let runs: WorkflowRunsResponse = cx
            .github_json(format!("/repos/{repo_id}/actions/runs?per_page=30"))
            .await?;
        let mut projection = DirProjection::exhaustive(
            runs.workflow_runs
                .iter()
                .map(|run| Entry::dir(run.id.to_string())),
        );
        for run in &runs.workflow_runs {
            let base = format!("/{}/{}/actions/runs/{}", self.owner, self.repo, run.id);
            projection = projection
                .preload_file(
                    format!("{base}/status"),
                    FileProjection::inline(run.status.clone().into_bytes())
                        .dynamic()
                        .build(),
                )
                .preload_file(
                    format!("{base}/conclusion"),
                    FileProjection::inline(run.conclusion.clone().unwrap_or_default().into_bytes())
                        .dynamic()
                        .build(),
                );
        }
        Ok(projection)
    }
}

impl RunKey {
    pub(crate) async fn log(self, cx: Cx) -> Result<FileProjection> {
        let repo = RepoId::new(&self.owner, &self.repo);
        let resp = cx
            .github_get(format!("/repos/{repo}/actions/runs/{}/logs", self.run_id))
            .send()
            .await?;
        let body = github_check_status(resp)?.into_body();
        Ok(FileProjection::body(unzip_logs(&body)).dynamic().build())
    }
}

async fn comments_dir(
    cx: &DirCx,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
) -> Result<DirProjection> {
    let page = cx.page_cursor(1);
    let comments: Vec<CommentRecord> = cx
        .github_json(format!(
            "/repos/{owner}/{repo}/issues/{number}/comments?per_page={COMMENT_PAGE_SIZE}&page={page}"
        ))
        .await?;
    let base = (u64::from(page) - 1) * COMMENT_PAGE_SIZE;
    let entries = (1..=comments.len() as u64).map(|idx| Entry::file((base + idx).to_string()));
    let mut projection = if (comments.len() as u64) < COMMENT_PAGE_SIZE {
        DirProjection::exhaustive(entries)
    } else {
        DirProjection::paged(entries, Cursor::Page(page + 1))
    };
    // The page fetch already holds every comment body, so preload each file
    // (byte-identical to comment_read) instead of forcing a per-read refetch.
    // Bodies vary in size: inline within the per-file cap and the shared
    // aggregate budget; comments that don't fit fall back to comment_read.
    let mut budget = MAX_EAGER_RESPONSE_BYTES;
    for (offset, comment) in comments.iter().enumerate() {
        let bytes = comment_bytes(comment);
        if bytes.len() > MAX_PROJECTED_BYTES || bytes.len() > budget {
            continue;
        }
        budget -= bytes.len();
        let idx = base + offset as u64 + 1;
        projection = projection.preload_file(
            idx.to_string(),
            FileProjection::inline(bytes).dynamic().build(),
        );
    }
    Ok(projection)
}

/// The rendered bytes for one comment leaf, shared by the listing preload and
/// the on-demand read so both paths are byte-identical.
fn comment_bytes(comment: &CommentRecord) -> Vec<u8> {
    let body = comment.body.as_deref().unwrap_or("");
    format!("{}:\n{body}\n", comment.user.login).into_bytes()
}

async fn comment_read(
    cx: &Cx,
    owner: &OwnerName,
    repo: &RepoName,
    number: u64,
    idx: u64,
) -> Result<FileProjection> {
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
    Ok(FileProjection::body(comment_bytes(comment))
        .dynamic()
        .build())
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
