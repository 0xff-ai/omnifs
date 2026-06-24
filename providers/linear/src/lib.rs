#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

//! linear-provider: Linear virtual filesystem provider for omnifs.

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
mod objects;

use core::str::FromStr;

use hashbrown::HashSet;
use omnifs_sdk::prelude::*;
use serde_json::json;

use crate::api::{
    GqlResponse, ISSUE_BY_IDENTIFIER_QUERY, ISSUES_QUERY, IssueNodeData, IssuePage, IssuesData,
    TEAMS_QUERY, Team, TeamsData, gql_request, gql_unwrap,
};
use crate::objects::Issue;

/// State filter directories under `/teams/{team}/issues/`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumString, strum::AsRefStr, strum::Display,
)]
pub enum StateFilter {
    /// Open issues. Linear state types in `{triage, backlog, unstarted, started}`.
    #[strum(serialize = "open")]
    Open,
    /// All issues regardless of state.
    #[strum(serialize = "all")]
    All,
}

impl PathSegment for StateFilter {
    fn choices() -> Option<&'static [&'static str]> {
        Some(&["open", "all"])
    }
}

/// A Linear team key (e.g. `ENG`, `OPS`). Uppercase ASCII alphanumeric.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TeamKey(String);

impl TeamKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for TeamKey {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.is_empty() || s.len() > 32 {
            return Err(());
        }
        let ok = s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !ok {
            return Err(());
        }
        Ok(Self(s.to_string()))
    }
}

impl AsRef<str> for TeamKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TeamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl PathSegment for TeamKey {}

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
    pub(crate) team: Facet<TeamKey>,
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
        if key.ident.team() != &*key.team {
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

#[omnifs_sdk::provider(metadata = "omnifs.provider.json")]
impl LinearProvider {
    fn start(r: &mut Router) -> Result<()> {
        r.dir("/teams").handler(teams_list)?;
        r.dir("/teams/{team}/issues")
            .handler(IssuesRootKey::filters)?;
        r.dir("/teams/{team}/issues/{filter}")
            .handler(IssueListKey::list)?;
        r.object::<Issue>("/teams/{team}/issues/{filter}/{ident}", |o| {
            o.dynamic();
            o.file("item.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("title").derive(Issue::title)?;
            o.file("state").derive(Issue::state)?;
            o.file("priority").derive(Issue::priority)?;
            o.file("assignee").derive(Issue::assignee)?;
            o.file("description.md").lazy().derive(Issue::description)?;
            Ok(())
        })?;
        Ok(())
    }
}

async fn teams_list(cx: DirCx) -> Result<DirProjection> {
    let teams = fetch_all_teams(&cx).await?;
    let entries = teams
        .into_iter()
        .filter(|team| team.key.parse::<TeamKey>().is_ok())
        .map(|team| Entry::dir(team.key));
    Ok(DirProjection::exhaustive(entries))
}

impl IssuesRootKey {
    #[allow(clippy::unused_self)]
    fn filters(self, _cx: DirCx) -> Result<DirProjection> {
        Ok(DirProjection::exhaustive(
            StateFilter::choices()
                .into_iter()
                .flatten()
                .map(|&name| Entry::dir(name.to_string())),
        ))
    }
}

impl IssueListKey {
    async fn list(self, cx: DirCx) -> Result<DirProjection> {
        let team = self.team;
        let filter = self.filter;
        let page = fetch_all_issues(&cx, &team, filter).await?;
        let mut seen: HashSet<String> = HashSet::with_capacity(page.items.len());
        let mut idents: Vec<IssueIdent> = Vec::new();
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
            idents.push(ident);
        }
        let projection = if page.truncated {
            DirProjection::open(idents.iter().map(|ident| Entry::dir(ident.to_string())))
        } else {
            DirProjection::exhaustive(idents.iter().map(|ident| Entry::dir(ident.to_string())))
        };
        Ok(projection)
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
        if !data.teams.page_info.has_next_page {
            break;
        }
        match data.teams.page_info.end_cursor {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
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
        if !data.issues.page_info.has_next_page {
            break;
        }
        match data.issues.page_info.end_cursor {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
        if items.len() >= 2000 {
            truncated = true;
            break;
        }
    }
    Ok(IssuePage { items, truncated })
}
