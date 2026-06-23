#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

//! github-provider: GitHub virtual filesystem provider for omnifs.

use core::str::FromStr;
use hashbrown::HashSet;
use omnifs_sdk::prelude::*;
use serde::{Deserialize, Serialize};

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
mod item;
mod objects;

pub(crate) use api::{GithubRest, github_check_status};
use item::{
    IssueCommentKey, IssueKey, IssueListKey, IssuesRootKey, OwnerKey, PullCommentKey, PullKey,
    PullListKey, PullsRootKey, RepoKey, RunKey, RunListKey,
};
pub(crate) use objects::ItemData;
use objects::{Issue, PullRequest, Repo, Run};

/// Base URL for the GitHub REST API. Compose with a leading-slash path.
pub(crate) const API_BASE: &str = "https://api.github.com";

/// Parse a JSON API response body into a model type.
pub(crate) fn parse_model<T>(body: &[u8]) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_slice(body)
        .map_err(|error| ProviderError::invalid_input(format!("JSON parse error: {error}")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerKind {
    User,
    Org,
}

/// State filter for resources.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumString, strum::AsRefStr, strum::Display,
)]
pub enum StateFilter {
    #[strum(serialize = "open")]
    Open,
    #[strum(serialize = "all")]
    All,
}

impl PathSegment for StateFilter {
    fn choices() -> Option<&'static [&'static str]> {
        Some(&["open", "all"])
    }
}

#[omnifs_sdk::provider(metadata = "omnifs.provider.json", resources(git = true))]
impl GithubProvider {
    fn start(r: &mut Router) -> Result<()> {
        r.dir("/{owner}").handler(OwnerKey::repos)?;

        r.object::<Repo>("/{owner}/{repo}", |o| {
            o.dynamic();
            o.representations("repo", ())?;
            Ok(())
        })?;

        r.dir("/{owner}/{repo}/issues")
            .handler(IssuesRootKey::filters)?;
        r.dir("/{owner}/{repo}/issues/{filter}")
            .handler(IssueListKey::list)?;
        r.object::<Issue>("/{owner}/{repo}/issues/{filter}/{number}", |o| {
            o.dynamic();
            o.representations("item", (Markdown,))?;
            o.file("title")
                .project(|value: &Issue, _key| value.title())?;
            o.file("body")
                .lazy()
                .project(|value: &Issue, _key| value.body())?;
            o.file("state")
                .project(|value: &Issue, _key| value.state())?;
            o.file("user").project(|value: &Issue, _key| value.user())?;
            Ok(())
        })?;
        r.dir("/{owner}/{repo}/issues/{filter}/{number}/comments")
            .handler(IssueKey::comments)?;
        r.file("/{owner}/{repo}/issues/{filter}/{number}/comments/{idx}")
            .handler(IssueCommentKey::read)?;

        r.dir("/{owner}/{repo}/pulls")
            .handler(PullsRootKey::filters)?;
        r.dir("/{owner}/{repo}/pulls/{filter}")
            .handler(PullListKey::list)?;
        r.object::<PullRequest>("/{owner}/{repo}/pulls/{filter}/{number}", |o| {
            o.dynamic();
            o.representations("item", (Markdown,))?;
            o.file("title")
                .project(|value: &PullRequest, _key| value.title())?;
            o.file("body")
                .lazy()
                .project(|value: &PullRequest, _key| value.body())?;
            o.file("state")
                .project(|value: &PullRequest, _key| value.state())?;
            o.file("user")
                .project(|value: &PullRequest, _key| value.user())?;
            Ok(())
        })?;
        r.dir("/{owner}/{repo}/pulls/{filter}/{number}/comments")
            .handler(PullKey::comments)?;
        r.file("/{owner}/{repo}/pulls/{filter}/{number}/comments/{idx}")
            .handler(PullCommentKey::read)?;
        r.file("/{owner}/{repo}/pulls/{filter}/{number}/diff")
            .handler(PullKey::diff)?;

        r.treeref("/{owner}/{repo}/repo").handler(RepoKey::tree)?;

        r.dir("/{owner}/{repo}/actions/runs")
            .handler(RunListKey::list)?;
        r.object::<Run>("/{owner}/{repo}/actions/runs/{run_id}", |o| {
            o.dynamic();
            o.representations("run", ())?;
            o.file("status")
                .project(|value: &Run, _key| value.status())?;
            o.file("conclusion")
                .project(|value: &Run, _key| value.conclusion())?;
            Ok(())
        })?;
        r.file("/{owner}/{repo}/actions/runs/{run_id}/log")
            .handler(RunKey::log)?;

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerName(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoName(String);

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

impl FromStr for OwnerName {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        // GitHub owners are case-insensitive; normalize once here so
        // /Octocat/.. and /octocat/.. collapse to one LogicalId capture.
        is_safe_owner(s)
            .then(|| Self(s.to_ascii_lowercase()))
            .ok_or(())
    }
}

impl FromStr for RepoName {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        is_safe_segment(s).then(|| Self(s.to_string())).ok_or(())
    }
}

impl AsRef<str> for OwnerName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for OwnerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for RepoName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RepoName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
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
    !s.starts_with('.') && is_safe_segment(s)
}

#[derive(Debug, Deserialize)]
struct UserProfile {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct OrganizationProfile {}

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

    let first: Vec<RepoListing> = cx.github_json(format!("{base}&page=1")).await?;
    if first.len() < PAGE_SIZE {
        return Ok(first.into_iter().map(|r| r.name).collect());
    }

    let mut names: Vec<String> = first.into_iter().map(|r| r.name).collect();
    let mut next_page = 2u32;

    while next_page <= MAX_PAGES {
        let batch_end = (next_page + CONCURRENCY - 1).min(MAX_PAGES);
        let requests = (next_page..=batch_end)
            .map(|page| cx.github_json::<Vec<RepoListing>>(format!("{base}&page={page}")));

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
        .github_json::<UserProfile>(format!("/users/{owner}"))
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
        .github_json::<OrganizationProfile>(format!("/orgs/{owner}"))
        .await
    {
        Ok(_) => Ok(Some(OwnerKind::Org)),
        Err(error) if matches!(error.kind(), ProviderErrorKind::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) const COMMENT_PAGE_SIZE: u64 = 100;

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

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CommentRecord {
    pub(crate) user: CommentUser,
    pub(crate) body: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CommentUser {
    pub(crate) login: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkflowRunsResponse {
    #[serde(default)]
    pub(crate) workflow_runs: Vec<objects::Run>,
}

/// Search supplies `total_count` so we can size the rest of the work
/// without parsing Link headers. Pages 2..N use the typed REST endpoint
/// because Search caps result windows and mixes issue/PR data unless qualified.
pub(crate) async fn list_items(
    cx: &Cx,
    owner: &OwnerName,
    repo: &RepoName,
    kind: item::ItemKind,
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

    let first: SearchResults = match cx.github_json(&search_path).await {
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
        let page_requests = (2..=page_count)
            .map(|page| cx.github_json::<Vec<ItemData>>(format!("{rest_path}&page={page}")));
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
        .github_json::<serde::de::IgnoredAny>(format!("/repos/{owner}/{repo}"))
        .await
    {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == ProviderErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}
