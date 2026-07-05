#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

//! github-provider: GitHub virtual filesystem provider for omnifs.

use hashbrown::HashSet;
use omnifs_sdk::prelude::*;
use serde::{Deserialize, Serialize};

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
mod item;
mod objects;

use api::GitHubApi;
use item::ItemKind;
pub(crate) use objects::ItemData;
use objects::{
    ChangedFile, CheckRun, Comment, Issue, Notification, Owner, PullRequest, Repo, Review,
    ReviewComment, WorkflowRun,
};
#[cfg(not(target_arch = "wasm32"))]
use omnifs_sdk::{
    AmbientSource, DevicePollCompat, OauthScheme, ProviderAuthManifest, SchemeGuidance,
    StaticTokenScheme, TokenValidation,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerKind {
    User,
    Org,
}

/// State filter for resources.
#[omnifs_sdk::path_segment]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[strum(serialize_all = "snake_case")]
pub enum StateFilter {
    Open,
    All,
}

#[cfg(not(target_arch = "wasm32"))]
fn auth() -> ProviderAuthManifest {
    ProviderAuthManifest::builder("device")
        .static_token(
            StaticTokenScheme::new("pat", "GitHub personal access token")
                .inject(["api.github.com"])
                .creation_url(
                    "https://github.com/settings/tokens/new?scopes=read:user&description=omnifs",
                )
                .validation(
                    TokenValidation::get("https://api.github.com/user")
                        .extract([("identity", "/login")]),
                )
                .ambient([
                    AmbientSource::env_var("GITHUB_TOKEN"),
                    AmbientSource::command(["gh", "auth", "token"]).note("gh CLI login"),
                ]),
            SchemeGuidance::new()
                .summary(
                    "A classic personal access token; the create link pre-selects the read:user scope.",
                )
                .setup([
                    "Add the repo scope as well if you want to browse private repositories and their issues or pull requests.",
                ])
                .docs_url(
                    "https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens",
                ),
        )
        .oauth(
            OauthScheme::device_code(
                "device",
                "GitHub OAuth device flow",
                "https://github.com/login/oauth/authorize",
                "https://github.com/login/device/code",
                "https://github.com/login/oauth/access_token",
            )
            .inject(["api.github.com"])
            .client_id("Ov23licogxMDzS47s9sF")
            .device_poll_compat(DevicePollCompat::ErrorInOkBody),
            SchemeGuidance::new().summary(
                "Approve a one-time code at github.com/login/device using omnifs's GitHub app; nothing to copy back.",
            ),
        )
        .build()
}

#[omnifs_sdk::provider(
    id = "github",
    display_name = "GitHub",
    mount = "github",
    capabilities(
        domain(
            "api.github.com",
            "Fetch GitHub API resources such as repository metadata, issues, pull requests, actions, and events."
        ),
        git_repo(
            "git@github.com:*",
            "Clone repository contents over SSH when browsing repo paths."
        ),
    ),
    limits(
        memory_mb(
            256,
            "Leave room for larger GitHub API payloads and repository tree projections."
        ),
    ),
    auth = auth()
)]
impl GithubProvider {
    fn start(r: &mut Router) -> Result<()> {
        r.object::<Owner>("/{owner}", |o| {
            o.dynamic();
            o.file("owner.json").canonical::<Json>()?;
            o.file("profile.md").representation::<Markdown>()?;
            o.dir("{repo}").collection(Owner::repos)?;
            Ok(())
        })?;

        r.object::<Repo>("/{owner}/{repo}", |o| {
            o.dynamic();
            o.file("repo.json").canonical::<Json>()?;
            o.dir("repo").tree(Repo::tree)?;
            o.dir("issues")
                .choices(StateFilter::choices().expect("StateFilter has finite choices"))?;
            o.dir("issues/{filter}").collection(Repo::issues)?;
            o.dir("pulls")
                .choices(StateFilter::choices().expect("StateFilter has finite choices"))?;
            o.dir("pulls/{filter}").collection(Repo::pulls)?;
            o.dir("actions/runs").collection(Repo::workflow_runs)?;
            Ok(())
        })?;

        r.object::<Issue>("/{owner}/{repo}/issues/{filter}/{number}", |o| {
            o.dynamic();
            o.file("item.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("title").computed(Issue::title)?;
            o.file("state").computed(Issue::state)?;
            o.file("user").computed(Issue::user)?;
            o.file("body").lazy().computed(Issue::body)?;
            o.dir("comments").collection(Issue::comments)?;
            Ok(())
        })?;

        r.object::<PullRequest>("/{owner}/{repo}/pulls/{filter}/{number}", |o| {
            o.dynamic();
            o.file("item.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("title").computed(PullRequest::title)?;
            o.file("state").computed(PullRequest::state)?;
            o.file("user").computed(PullRequest::user)?;
            o.file("body").lazy().computed(PullRequest::body)?;
            o.file("diff.patch").blob(PullRequest::diff)?;
            o.dir("comments").collection(PullRequest::comments)?;
            o.dir("files").collection(PullRequest::files)?;
            o.dir("reviews").collection(PullRequest::reviews)?;
            o.dir("checks").collection(PullRequest::checks)?;
            Ok(())
        })?;

        r.object::<ChangedFile>(
            "/{owner}/{repo}/pulls/{filter}/{number}/files/{path}",
            |o| {
                o.dynamic();
                o.file("file.json").canonical::<Json>()?;
                o.file("file.md").representation::<Markdown>()?;
                o.file("filename").computed(ChangedFile::filename)?;
                o.file("status").computed(ChangedFile::status)?;
                o.file("patch.patch").lazy().computed(ChangedFile::patch)?;
                Ok(())
            },
        )?;

        r.object::<Review>(
            "/{owner}/{repo}/pulls/{filter}/{number}/reviews/{review_id}",
            |o| {
                o.dynamic();
                o.file("review.json").canonical::<Json>()?;
                o.file("review.md").representation::<Markdown>()?;
                o.file("state").computed(Review::state)?;
                o.file("user").computed(Review::user)?;
                o.file("body.md").lazy().computed(Review::body_md)?;
                o.dir("comments").collection(Review::comments)?;
                Ok(())
            },
        )?;

        r.object::<ReviewComment>(
            "/{owner}/{repo}/pulls/{filter}/{number}/reviews/{review_id}/comments/{comment_id}",
            |o| {
                o.dynamic();
                o.file("comment.json").canonical::<Json>()?;
                o.file("comment.md").representation::<Markdown>()?;
                o.file("body.md").lazy().computed(ReviewComment::body_md)?;
                o.file("author").computed(ReviewComment::author)?;
                o.file("path").computed(ReviewComment::path)?;
                Ok(())
            },
        )?;

        r.object::<CheckRun>(
            "/{owner}/{repo}/pulls/{filter}/{number}/checks/{check_run_id}",
            |o| {
                o.dynamic();
                o.file("check.json").canonical::<Json>()?;
                o.file("check.md").representation::<Markdown>()?;
                o.file("name").computed(CheckRun::name)?;
                o.file("status").computed(CheckRun::status)?;
                o.file("conclusion").computed(CheckRun::conclusion)?;
                o.file("summary.md").lazy().computed(CheckRun::summary_md)?;
                Ok(())
            },
        )?;

        r.object::<WorkflowRun>("/{owner}/{repo}/actions/runs/{run_id}", |o| {
            o.dynamic();
            o.file("run.json").canonical::<Json>()?;
            o.file("status").computed(WorkflowRun::status)?;
            o.file("conclusion").computed(WorkflowRun::conclusion)?;
            o.file("log").direct(WorkflowRun::log)?;
            Ok(())
        })?;

        r.object::<Comment>(
            "/{owner}/{repo}/{item_kind}/{filter}/{number}/comments/{comment_id}",
            |o| {
                o.dynamic();
                o.file("comment.json").canonical::<Json>()?;
                o.file("comment.md").representation::<Markdown>()?;
                o.file("body.md").lazy().computed(Comment::body_md)?;
                o.file("author").computed(Comment::author)?;
                Ok(())
            },
        )?;

        r.dir("/notifications").handler(Notification::list)?;
        r.object::<Notification>("/notifications/thread-{thread_id}", |o| {
            o.dynamic();
            o.file("notification.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("reason").computed(Notification::reason)?;
            o.file("subject").computed(Notification::subject)?;
            Ok(())
        })?;

        Ok(())
    }
}

#[omnifs_sdk::path_segment(validate = is_safe_owner, normalize = str::to_ascii_lowercase)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerName(String);

#[omnifs_sdk::path_segment(validate = is_safe_segment)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoName(String);

#[omnifs_sdk::path_segment(validate = FilePath::is_valid_segment)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct FilePath(String);

impl FilePath {
    pub(crate) fn from_github_path(path: &str) -> Option<Self> {
        let encoded = Self::encode(path);
        encoded.parse().ok()
    }

    pub(crate) fn decoded(&self) -> Result<String> {
        let bytes = Self::decode_bytes(&self.0)?;
        String::from_utf8(bytes)
            .map_err(|err| ProviderError::invalid_input(format!("file path is not utf-8: {err}")))
    }

    pub(crate) fn is_valid_segment(s: &str) -> bool {
        if s.is_empty() || s == "." || s == ".." || s.as_bytes().contains(&b'/') {
            return false;
        }
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' => {
                    if i + 2 >= bytes.len()
                        || !bytes[i + 1].is_ascii_hexdigit()
                        || !bytes[i + 2].is_ascii_hexdigit()
                    {
                        return false;
                    }
                    i += 3;
                },
                b if is_unreserved_path_byte(b) => i += 1,
                _ => return false,
            }
        }
        true
    }

    fn encode(path: &str) -> String {
        let mut encoded = String::with_capacity(path.len());
        for byte in path.bytes() {
            if is_unreserved_path_byte(byte) {
                encoded.push(char::from(byte));
            } else {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                encoded.push('%');
                encoded.push(char::from(HEX[(byte >> 4) as usize]));
                encoded.push(char::from(HEX[(byte & 0x0F) as usize]));
            }
        }
        if encoded == "." {
            "%2E".to_string()
        } else if encoded == ".." {
            "%2E%2E".to_string()
        } else {
            encoded
        }
    }

    fn decode_bytes(s: &str) -> Result<Vec<u8>> {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let hi = hex_value(bytes[i + 1])?;
                let lo = hex_value(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        Ok(out)
    }
}

#[omnifs_sdk::path_segment(validate = is_safe_segment)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ThreadId(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RepoId {
    owner: OwnerName,
    repo: RepoName,
}

impl RepoId {
    pub(crate) fn new(owner: &OwnerName, repo: &RepoName) -> Self {
        Self {
            owner: owner.clone(),
            repo: repo.clone(),
        }
    }
}

impl std::fmt::Display for RepoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct User {
    pub(crate) login: String,
}

/// Validates that a path segment is a safe GitHub repo or numeric ID.
/// GitHub permits leading dots in repo names (`.github` is the canonical
/// per-org community-config repo), so we only block the two path-traversal
/// cases.
pub fn is_safe_segment(s: &str) -> bool {
    if s.is_empty() || s == "." || s == ".." {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Validates that a path segment is a safe GitHub owner (user or org).
/// Unlike repos, owners never start with a dot, so rejecting leading-dot
/// names keeps the `{owner}` capture from binding the host's mount-root
/// ignore files (`.gitignore`/`.ignore`/`.rgignore`) as phantom owner
/// directories, which would shadow them and defeat the ignore mechanism.
pub fn is_safe_owner(s: &str) -> bool {
    !s.eq_ignore_ascii_case("notifications") && !s.starts_with('.') && is_safe_segment(s)
}

const fn is_unreserved_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
}

fn hex_value(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(ProviderError::invalid_input(
            "bad percent-encoded file path",
        )),
    }
}

#[derive(Debug, Deserialize)]
struct UserProfile {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct RepoListing {
    name: String,
}

pub(crate) async fn fetch_owner_repos(
    cx: &Cx,
    owner: &OwnerName,
    kind: OwnerKind,
) -> Result<Vec<String>> {
    const MAX_PAGES: u32 = 50;
    const PAGE_SIZE: usize = 100;
    const CONCURRENCY: u32 = 5;

    let scope = match kind {
        OwnerKind::User => "users",
        OwnerKind::Org => "orgs",
    };
    let base = format!("/{scope}/{owner}/repos?per_page={PAGE_SIZE}&sort=updated");

    let first: Vec<RepoListing> = cx
        .endpoint(GitHubApi)
        .get(format!("{base}&page=1"))
        .json()
        .await?;
    if first.len() < PAGE_SIZE {
        return Ok(first.into_iter().map(|r| r.name).collect());
    }

    let mut names: Vec<String> = first.into_iter().map(|r| r.name).collect();
    let mut next_page = 2u32;

    while next_page <= MAX_PAGES {
        let batch_end = (next_page + CONCURRENCY - 1).min(MAX_PAGES);
        let requests = (next_page..=batch_end).map(|page| {
            cx.endpoint(GitHubApi)
                .get(format!("{base}&page={page}"))
                .json::<Vec<RepoListing>>()
        });

        for batch in join_all(requests).await {
            let repos = batch?;
            let done = repos.len() < PAGE_SIZE;
            names.extend(repos.into_iter().map(|r| r.name));
            if done {
                return Ok(names);
            }
        }

        next_page = batch_end + 1;
    }

    Ok(names)
}

pub(crate) async fn resolve_owner_kind(cx: &Cx, owner: &OwnerName) -> Result<Option<OwnerKind>> {
    use omnifs_sdk::error::ProviderErrorKind;

    match cx
        .endpoint(GitHubApi)
        .get(format!("/users/{owner}"))
        .json::<UserProfile>()
        .await
    {
        Ok(profile) => {
            return Ok(Some(if profile.kind == "Organization" {
                OwnerKind::Org
            } else {
                OwnerKind::User
            }));
        },
        Err(error) if matches!(error.kind(), ProviderErrorKind::NotFound) => {},
        Err(error) => return Err(error),
    }

    match cx
        .endpoint(GitHubApi)
        .get(format!("/orgs/{owner}"))
        .json::<serde::de::IgnoredAny>()
        .await
    {
        Ok(_) => Ok(Some(OwnerKind::Org)),
        Err(error) if matches!(error.kind(), ProviderErrorKind::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) const FILE_PAGE_SIZE: u64 = 100;
pub(crate) const REVIEW_PAGE_SIZE: u64 = 100;
pub(crate) const CHECK_RUN_PAGE_SIZE: u64 = 100;
pub(crate) const COMMENT_PAGE_SIZE: u64 = 100;
pub(crate) const NOTIFICATION_PAGE_SIZE: u64 = 50;

const PAGE_SIZE: u64 = 100;
const SEARCH_RESULT_CAP: u64 = 1000;

#[derive(Debug, Deserialize)]
struct SearchResults {
    #[serde(default)]
    total_count: u64,
    #[serde(default)]
    items: Vec<ItemData>,
}

pub(crate) struct ListPage {
    pub(crate) items: Vec<ItemData>,
    pub(crate) exhaustive: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CheckRunsResponse {
    #[serde(default)]
    pub(crate) check_runs: Vec<CheckRun>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkflowRunsResponse {
    #[serde(default)]
    pub(crate) workflow_runs: Vec<WorkflowRun>,
}

/// Search supplies `total_count` so we can size the rest of the work
/// without parsing Link headers. Pages 2..N use the typed REST endpoint
/// because Search caps result windows and mixes issue/PR data unless qualified.
pub(crate) async fn list_items(
    cx: &Cx,
    owner: &OwnerName,
    repo: &RepoName,
    kind: ItemKind,
    filter: StateFilter,
) -> Result<ListPage> {
    let search_state_clause = match filter {
        StateFilter::Open => "+state:open",
        StateFilter::All => "",
    };
    let qualifier = kind.search_qualifier();
    let search_path = format!(
        "/search/issues?q=repo:{owner}/{repo}{qualifier}{search_state_clause}\
         &sort=created&order=desc&per_page={PAGE_SIZE}"
    );
    let rest_resource = kind.rest_resource();
    let rest_state = rest_state(filter);
    let rest_path = format!(
        "/repos/{owner}/{repo}/{rest_resource}?state={rest_state}\
         &sort=created&direction=desc&per_page={PAGE_SIZE}"
    );

    let first: SearchResults = match cx.endpoint(GitHubApi).get(search_path).json().await {
        Ok(results) => results,
        Err(err) if is_search_repo_missing(&err) => {
            if repo_exists(cx, owner, repo).await? {
                return Err(err);
            }
            return Err(ProviderError::not_found(format!(
                "{owner}/{repo}: repository not found on GitHub"
            )));
        },
        Err(err) => return Err(err),
    };
    let capped_total = first.total_count.min(SEARCH_RESULT_CAP);
    let page_count = capped_total.div_ceil(PAGE_SIZE);
    let mut items = first.items;
    items.reserve((capped_total as usize).saturating_sub(items.len()));

    if page_count > 1 {
        let page_requests = (2..=page_count).map(|page| {
            cx.endpoint(GitHubApi)
                .get(format!("{rest_path}&page={page}"))
                .json::<Vec<ItemData>>()
        });
        for page in join_all(page_requests).await {
            items.extend(page?);
        }
        let mut seen = HashSet::with_capacity(items.len());
        items.retain(|item| seen.insert(item.number));
    }

    Ok(ListPage {
        items,
        exhaustive: first.total_count <= SEARCH_RESULT_CAP,
    })
}

const fn rest_state(filter: StateFilter) -> &'static str {
    match filter {
        StateFilter::Open => "open",
        StateFilter::All => "all",
    }
}

fn is_search_repo_missing(err: &ProviderError) -> bool {
    use omnifs_sdk::error::ProviderErrorKind;
    err.kind() == ProviderErrorKind::InvalidInput && err.message().contains("HTTP 422")
}

async fn repo_exists(cx: &Cx, owner: &OwnerName, repo: &RepoName) -> Result<bool> {
    use omnifs_sdk::error::ProviderErrorKind;

    match cx
        .endpoint(GitHubApi)
        .get(format!("/repos/{owner}/{repo}"))
        .json::<serde::de::IgnoredAny>()
        .await
    {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == ProviderErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}
