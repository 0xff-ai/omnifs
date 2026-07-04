#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

//! linear-provider: Linear virtual filesystem provider for omnifs.

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
mod objects;

use core::str::FromStr;

use hashbrown::HashSet;
use omnifs_sdk::prelude::*;
#[cfg(not(target_arch = "wasm32"))]
use omnifs_sdk::{
    OauthScheme, ProviderAuthManifest, SchemeGuidance, StaticTokenScheme, TokenValidation,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::{
    GqlResponse, ISSUE_BY_IDENTIFIER_QUERY, ISSUES_QUERY, IssueNodeData, IssuePage, IssuesData,
    TEAMS_QUERY, Team, TeamsData, gql_request, gql_unwrap,
};
use crate::objects::Issue;

/// State filter directories under `/teams/{team}/issues/`.
#[omnifs_sdk::path_segment]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[strum(serialize_all = "snake_case")]
pub enum StateFilter {
    /// Open issues. Linear state types in `{triage, backlog, unstarted, started}`.
    Open,
    /// All issues regardless of state.
    All,
}

/// A Linear team key (e.g. `ENG`, `OPS`). Uppercase ASCII alphanumeric.
#[omnifs_sdk::path_segment(validate = is_valid_team_key)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TeamKey(String);

fn is_valid_team_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A Linear issue identifier (e.g. `ENG-1234`). The textual form is
/// what users type and what Linear's API accepts in `Issue.identifier`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IssueIdent {
    team: TeamKey,
    number: u64,
}

impl IssueIdent {
    pub fn team(&self) -> &TeamKey {
        &self.team
    }
}

impl FromStr for IssueIdent {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (team, number) = s.rsplit_once('-').ok_or(())?;
        let team = team.parse::<TeamKey>()?;
        let number = number.parse::<u64>().map_err(|_| ())?;
        if number == 0 {
            return Err(());
        }
        Ok(Self { team, number })
    }
}

impl std::fmt::Display for IssueIdent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.team, self.number)
    }
}

/// Linear's GraphQL host. Every request POSTs a query to `/graphql`; auth is
/// injected from the embedded manifest into the `Authorization` header.
#[derive(omnifs_sdk::Endpoint)]
#[endpoint(
    base = "https://api.linear.app",
    default_header = "Accept: application/json"
)]
struct LinearApi;

#[omnifs_sdk::path_captures]
struct IssuesRootKey {
    team: TeamKey,
}

#[omnifs_sdk::path_captures]
struct IssueListKey {
    team: TeamKey,
    filter: StateFilter,
}

#[omnifs_sdk::path_captures]
pub(crate) struct IssueKey {
    // Identity: an issue belongs to exactly one team, and the collection uses
    // `team` to render each child anchor (`/teams/{team}/issues/{filter}/{ident}`).
    // TeamKey has no finite `choices()`, so it cannot be a facet axis.
    pub(crate) team: TeamKey,
    // Drives facet-axis view-leaf expansion; not read in provider code.
    #[allow(dead_code)]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) ident: IssueIdent,
}

impl Issue {
    /// Fetch a single Linear issue by identifier. Linear's GraphQL has no
    /// `If-None-Match`; `since` is unused so we always full-fetch and never
    /// return `Load::Unchanged`.
    pub(crate) async fn load(
        cx: &Cx<()>,
        key: &IssueKey,
        _since: Option<Validator>,
    ) -> Result<Load<Issue>> {
        if key.ident.team() != &key.team {
            return Ok(Load::NotFound);
        }
        let vars = json!({ "id": key.ident.to_string() });
        let resp: GqlResponse<IssueNodeData> = cx
            .endpoint(LinearApi)
            .post("/graphql")
            .body_json(&gql_request(ISSUE_BY_IDENTIFIER_QUERY, &vars))
            .json()
            .await?;
        let Some(node) = gql_unwrap(resp)?.issue else {
            return Ok(Load::NotFound);
        };
        let bytes = node.get().as_bytes().to_vec();
        let value: Issue = serde_json::from_str(node.get())
            .map_err(|e| ProviderError::invalid_input(format!("linear issue node parse: {e}")))?;
        let validator = value.version().map(Validator::from);
        Ok(Load::fresh(value, Canonical::new(bytes, validator)))
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn auth() -> ProviderAuthManifest {
    ProviderAuthManifest::builder("oauth")
        .static_token(
            StaticTokenScheme::new("pat", "Linear personal access token or API key")
                .inject(["api.linear.app"])
                .prefix("")
                .creation_url("https://linear.app/settings/api")
                .validation(
                    TokenValidation::post(
                        "https://api.linear.app/graphql",
                        "{\"query\":\"query { viewer { id name email organization { name urlKey } } }\"}",
                    )
                    .json_pointer("/data/viewer/id")
                    .extract([
                        ("identity", "/data/viewer/email"),
                        ("workspace", "/data/viewer/organization/urlKey"),
                    ]),
                ),
            SchemeGuidance::new().summary("A personal API key created in Linear's API settings."),
        )
        .oauth(
            OauthScheme::pkce_loopback(
                "oauth",
                "Linear OAuth",
                "https://linear.app/oauth/authorize",
                "https://api.linear.app/oauth/token",
                "http://127.0.0.1:{port}/callback",
            )
            .inject(["api.linear.app"])
            .prefix("")
            .client_id("4dc7b7c05f651306a318de6f9f963b40")
            .scopes(["read"]),
            SchemeGuidance::new().summary(
                "Browser sign-in through omnifs's Linear app, granting read access to your workspace.",
            ),
        )
        .build()
}

#[omnifs_sdk::provider(
    id = "linear",
    display_name = "Linear",
    mount = "linear",
    capabilities(
        domain(
            "api.linear.app",
            "Fetch Linear GraphQL resources for teams, issues, projects, and workflow metadata."
        ),
    ),
    limits(
        memory_mb(
            128,
            "Leave room for GraphQL response decoding and issue tree projections."
        ),
    ),
    auth = auth()
)]
impl LinearProvider {
    fn start(r: &mut Router) -> Result<()> {
        r.dir("/teams").handler(teams_list)?;
        r.dir("/teams/{team}/issues")
            .handler(IssuesRootKey::filters)?;
        // The `/teams/{team}` anchor hosts the issue listing as a typed
        // `Collection<Issue>`. A NESTED collection is declared on a parent
        // object, so `TeamAnchor` exists purely to carry the `issues/{filter}`
        // collection; it declares no readable face of its own.
        r.object::<TeamAnchor>("/teams/{team}", |o| {
            o.dynamic();
            o.dir("issues/{filter}").collection(TeamAnchor::issues)?;
            Ok(())
        })?;
        r.object::<Issue>("/teams/{team}/issues/{filter}/{ident}", |o| {
            o.dynamic();
            o.file("item.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("title").computed(Issue::title)?;
            o.file("state").computed(Issue::state)?;
            o.file("priority").computed(Issue::priority)?;
            o.file("assignee").computed(Issue::assignee)?;
            o.file("description.md")
                .lazy()
                .computed(Issue::description)?;
            Ok(())
        })?;
        Ok(())
    }
}

async fn teams_list(cx: DirCx) -> Result<DirListing> {
    let teams = fetch_all_teams(&cx).await?;
    let entries = teams
        .into_iter()
        .filter(|team| team.key.parse::<TeamKey>().is_ok())
        .map(|team| Entry::dir(team.key));
    Ok(DirListing::exhaustive(entries))
}

impl IssuesRootKey {
    #[allow(clippy::unused_self)]
    fn filters(self, _cx: DirCx) -> Result<DirListing> {
        Ok(DirListing::exhaustive(
            StateFilter::choices()
                .into_iter()
                .flatten()
                .map(|&name| Entry::dir(name.to_string())),
        ))
    }
}

/// The parent object anchoring the issue collection at `/teams/{team}`.
///
/// It declares no canonical or readable face; a NESTED `Collection` must hang
/// off an object anchor, and this is the minimal one. Because it has no
/// canonical face, the SDK never loads it (the anchor-listing path
/// early-returns for a no-canonical object and no read target resolves to it),
/// so [`Self::load`] exists only to satisfy the `Object` trait.
#[omnifs_sdk::object(kind = "linear.team", key = crate::IssuesRootKey)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TeamAnchor {}

impl TeamAnchor {
    pub(crate) async fn load(
        _cx: &Cx<()>,
        _key: &IssuesRootKey,
        _since: Option<Validator>,
    ) -> Result<Load<TeamAnchor>> {
        // Unreachable: the `/teams/{team}` anchor has no canonical/readable
        // face, so neither the list nor read path ever calls it.
        Ok(Load::NotFound)
    }

    /// List a team's issues as child `Issue` objects. The listing carries only
    /// each issue's identifier (the child anchor name); the issue canonical
    /// loads on the child's own first read.
    async fn issues(
        key: IssueListKey,
        cx: ListCx<NoCursor>,
    ) -> Result<Collection<Issue, NoCursor>> {
        let team = key.team;
        let filter = key.filter;
        let page = fetch_all_issues(&cx, &team, filter).await?;
        let mut seen: HashSet<String> = HashSet::with_capacity(page.items.len());
        let mut entries: Vec<CollectionEntry<Issue>> = Vec::new();
        for issue in page.items {
            if !seen.insert(issue.identifier.clone()) {
                continue;
            }
            let Ok(ident) = issue.identifier.parse::<IssueIdent>() else {
                continue;
            };
            if ident.team() != &team {
                continue;
            }
            entries.push(CollectionEntry::key(IssueKey {
                team: team.clone(),
                filter: Facet(filter),
                ident,
            }));
        }
        Ok(if page.truncated {
            Collection::partial(entries)
        } else {
            Collection::complete(entries)
        })
    }
}

async fn fetch_all_teams(cx: &Cx) -> Result<Vec<Team>> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let vars = json!({ "after": after });
        let data: TeamsData = gql_unwrap(
            cx.endpoint(LinearApi)
                .post("/graphql")
                .body_json(&gql_request(TEAMS_QUERY, &vars))
                .json()
                .await?,
        )?;
        out.extend(data.teams.nodes);
        let Some(cursor) = data.teams.page_info.next_cursor() else {
            break;
        };
        after = Some(cursor);
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

async fn fetch_all_issues(cx: &Cx, team: &TeamKey, filter: StateFilter) -> Result<IssuePage> {
    let state_types = match filter {
        StateFilter::Open => vec!["triage", "backlog", "unstarted", "started"],
        StateFilter::All => Vec::new(),
    };
    let state_filter: serde_json::Value = if state_types.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Array(
            state_types
                .into_iter()
                .map(serde_json::Value::from)
                .collect(),
        )
    };

    let mut items: Vec<Issue> = Vec::new();
    let mut after: Option<String> = None;
    let mut truncated = false;
    loop {
        let vars = json!({
            "teamKey": team.as_str(),
            "stateTypes": state_filter,
            "after": after,
        });
        let data: IssuesData = gql_unwrap(
            cx.endpoint(LinearApi)
                .post("/graphql")
                .body_json(&gql_request(ISSUES_QUERY, &vars))
                .json()
                .await?,
        )?;
        items.extend(data.issues.nodes);
        let Some(cursor) = data.issues.page_info.next_cursor() else {
            break;
        };
        after = Some(cursor);
        if items.len() >= 2000 {
            truncated = true;
            break;
        }
    }
    Ok(IssuePage { items, truncated })
}
