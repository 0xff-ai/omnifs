//! Path keys, object loads, collection methods, and live faces for the GitHub
//! provider.

use core::fmt;
use core::str::FromStr;

use rc_zip_sync::ReadZip;

use omnifs_core::ContentType;
use omnifs_sdk::prelude::*;

use crate::api::GitHubApi;
use crate::objects::{Comment, Issue, ItemData, Owner, PullRequest, Repo, WorkflowRun};
use crate::{
    COMMENT_PAGE_SIZE, OwnerName, RepoId, RepoName, StateFilter, WorkflowRunsResponse,
    fetch_owner_repos, list_items, parse_model, resolve_owner_kind,
};

/// Identity discriminator for the comment anchor's `{item_kind}` segment. A
/// plain `PathSegment` capture (NOT a facet): an issue comment and a pull
/// comment are distinct objects, so `item_kind` is part of identity.
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

impl FromStr for ItemKind {
    type Err = ProviderError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "issues" => Ok(Self::Issues),
            "pulls" => Ok(Self::Pulls),
            other => Err(ProviderError::invalid_input(format!(
                "unknown item kind {other}"
            ))),
        }
    }
}

impl fmt::Display for ItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.rest_resource())
    }
}

impl PathSegment for ItemKind {
    fn choices() -> Option<&'static [&'static str]> {
        Some(&["issues", "pulls"])
    }
}

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
            .load_with(parse_model::<Self>)
            .await
        {
            Ok(Load::NotFound) => {},
            other => return other,
        }
        cx.endpoint(GitHubApi)
            .get(format!("/orgs/{}", key.owner))
            .maybe_if_none_match(since.as_ref())
            .load_with(parse_model::<Self>)
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
            .load_with(parse_model::<Self>)
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
            .load_with(parse_model::<ItemData>)
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
            .load_with(parse_model::<ItemData>)
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
            .load_with(parse_model::<Self>)
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
            .load_with(parse_model::<Self>)
            .await
    }
}

// ===========================================================================
// Collections
// ===========================================================================

impl Owner {
    /// Anchor-topology collection: the repo names listed under `/{owner}` ARE
    /// the child `Repo` anchors (`/{owner}/{repo}`). The repo listing carries
    /// only the name, not the single-repo canonical, so entries are `key`.
    pub(crate) async fn repos(
        key: OwnerKey,
        cx: ListCx<NoCursor>,
    ) -> Result<Collection<Repo, NoCursor>> {
        let kind = resolve_owner_kind(&cx, &key.owner)
            .await?
            .ok_or_else(|| ProviderError::not_found("owner not found"))?;
        let mut names = fetch_owner_repos(&cx, &key.owner, kind).await?;
        names.sort();
        let entries = names.into_iter().filter_map(|name| {
            RepoName::from_str(&name).ok().map(|repo| {
                CollectionEntry::key(RepoKey {
                    owner: key.owner.clone(),
                    repo,
                })
            })
        });
        Ok(Collection::complete(entries))
    }
}

impl Repo {
    pub(crate) async fn issues(
        key: ItemListKey,
        cx: ListCx<NoCursor>,
    ) -> Result<Collection<Issue, NoCursor>> {
        let filter = key.filter;
        let page = list_items(&cx, &key.owner, &key.repo, ItemKind::Issues, filter).await?;
        let entries = page
            .items
            .iter()
            .filter(|item| !item.is_pull_request())
            .map(|item| {
                CollectionEntry::derived(
                    IssueKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: Facet(filter),
                        number: item.number,
                    },
                    eager_item_leaves(item),
                )
            })
            .collect::<Vec<_>>();
        Ok(complete_or_partial(entries, page.exhaustive))
    }

    pub(crate) async fn pulls(
        key: ItemListKey,
        cx: ListCx<NoCursor>,
    ) -> Result<Collection<PullRequest, NoCursor>> {
        let filter = key.filter;
        let page = list_items(&cx, &key.owner, &key.repo, ItemKind::Pulls, filter).await?;
        let entries = page
            .items
            .iter()
            .map(|item| {
                CollectionEntry::derived(
                    PullKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: Facet(filter),
                        number: item.number,
                    },
                    eager_item_leaves(item),
                )
            })
            .collect::<Vec<_>>();
        Ok(complete_or_partial(entries, page.exhaustive))
    }

    pub(crate) async fn workflow_runs(
        key: RepoKey,
        cx: ListCx<NoCursor>,
    ) -> Result<Collection<WorkflowRun, NoCursor>> {
        let repo_id = RepoId::new(&key.owner, &key.repo);
        let runs: WorkflowRunsResponse = cx
            .endpoint(GitHubApi)
            .get(format!("/repos/{repo_id}/actions/runs?per_page=30"))
            .json()
            .await?;
        let entries = runs.workflow_runs.into_iter().map(|run| {
            let files = vec![
                ("status".to_string(), inline_text(&run.status)),
                (
                    "conclusion".to_string(),
                    inline_text(run.conclusion.as_deref().unwrap_or("")),
                ),
            ];
            CollectionEntry::derived(
                RunKey {
                    owner: key.owner.clone(),
                    repo: key.repo.clone(),
                    run_id: run.id,
                },
                files,
            )
        });
        Ok(Collection::complete(entries))
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
            .cache_key(format!("github/pulls/{repo_id}/{}/diff", key.number))
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
/// `item.md`, and `item.json` derive from the canonical, which the lossy row
/// cannot reproduce, so they load on first read.
fn eager_item_leaves(item: &ItemData) -> Vec<(String, FileProjection)> {
    let login = item.user.as_ref().map_or("", |u| u.login.as_str());
    vec![
        ("title".to_string(), inline_text(&item.title)),
        ("state".to_string(), inline_text(&item.state)),
        ("user".to_string(), inline_text(login)),
    ]
}

/// The shallow eager leaves a comment listing row can fill from the lossy list
/// payload: `author` and `body.md` render the same bytes from any source, so
/// they preload at listing time. `comment.json`/`comment.md` derive from the
/// verbatim standalone GET, which the list response cannot reproduce, so they
/// load on first read.
fn eager_comment_leaves(
    comment: &Comment,
    key: &CommentKey,
) -> Result<Vec<(String, FileProjection)>> {
    Ok(vec![
        ("author".to_string(), comment.author(key)?),
        ("body.md".to_string(), comment.body_md(key)?),
    ])
}

fn complete_or_partial<T: omnifs_sdk::object::Object, C: ListCursor>(
    entries: Vec<CollectionEntry<T>>,
    exhaustive: bool,
) -> Collection<T, C> {
    if exhaustive {
        Collection::complete(entries)
    } else {
        Collection::partial(entries)
    }
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
        let files = eager_comment_leaves(&comment, &key)?;
        entries.push(CollectionEntry::derived(key, files));
    }
    if len < COMMENT_PAGE_SIZE {
        Ok(Collection::complete(entries))
    } else {
        Ok(Collection::page(entries).next(PageCursor(page + 1)))
    }
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
