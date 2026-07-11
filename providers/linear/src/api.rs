//! Hand-rolled Linear GraphQL queries and response types.
//!
//! We deliberately avoid the codegen route (cynic): Linear's GraphQL
//! endpoint rejects the full introspection query with `Query too complex`,
//! and the v1 surface uses only three operations. Hand-written query
//! strings paired with `serde` response structs are smaller, easier to
//! audit, and avoid pulling a procedural-macro chain into the provider's
//! wasm32-wasip2 build.

use serde::Deserialize;
use serde_json::json;

use crate::objects::{Issue, Team};

/// One page of Linear's `teams` query.
pub(crate) const TEAMS_QUERY: &str = r"
query Teams($after: String) {
  teams(first: 50, after: $after) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id
      key
      name
    }
  }
}
";

/// Issues filtered by state type, sorted by `updatedAt` desc. The list query
/// omits `description`; that field is loaded lazily via the single-item fetch.
pub(crate) const ISSUES_QUERY: &str = r"
query Issues($teamKey: String!, $stateTypes: [String!], $after: String) {
  issues(
    first: 50,
    after: $after,
    filter: {
      team: { key: { eq: $teamKey } },
      state: { type: { in: $stateTypes } }
    },
    orderBy: updatedAt
  ) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id
      identifier
      number
      title
      priority
      updatedAt
      state { name type }
      assignee { name displayName email }
    }
  }
}
";

/// Single-issue fetch by identifier (e.g. `ENG-42`).
pub(crate) const ISSUE_BY_IDENTIFIER_QUERY: &str = r"
query IssueByIdentifier($id: String!) {
  issueVcsBranchSearch: issue(id: $id) {
    id
    identifier
    number
    title
    priority
    updatedAt
    state { name type }
    assignee { name displayName email }
    description
  }
}
";

/// Single-team fetch by key (e.g. `ENG`). Linear's `team(id:)` takes the
/// team UUID, not the key, so we filter the `teams` connection by key and take
/// the first node.
pub(crate) const TEAM_BY_KEY_QUERY: &str = r"
query TeamByKey($teamKey: String!) {
  teams(first: 1, filter: { key: { eq: $teamKey } }) {
    nodes {
      id
      key
      name
      description
      updatedAt
    }
  }
}
";

/// Outer GraphQL envelope: `{ "data": ..., "errors": ... }`.
#[derive(Debug, Deserialize)]
pub(crate) struct GqlResponse<T> {
    pub(crate) data: Option<T>,
    #[serde(default)]
    pub(crate) errors: Vec<GqlError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlError {
    pub(crate) message: String,
}

/// Unwrap a GraphQL envelope: surface `errors` as a `ProviderError`, then
/// return the `data` field (a missing `data` with no errors is itself an error).
pub(crate) fn gql_unwrap<T>(resp: GqlResponse<T>) -> crate::Result<T> {
    if !resp.errors.is_empty() {
        let msg = resp
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(omnifs_sdk::prelude::ProviderError::internal(format!(
            "linear: GraphQL errors: {msg}"
        )));
    }
    resp.data.ok_or_else(|| {
        omnifs_sdk::prelude::ProviderError::internal("linear: GraphQL response missing data field")
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct PageInfo {
    #[serde(rename = "hasNextPage")]
    pub(crate) has_next_page: bool,
    #[serde(rename = "endCursor")]
    pub(crate) end_cursor: Option<String>,
}

impl PageInfo {
    pub(crate) fn next_cursor(self) -> Option<String> {
        self.end_cursor.filter(|_| self.has_next_page)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamsConnection {
    pub(crate) page_info: PageInfo,
    pub(crate) nodes: Vec<Team>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TeamsData {
    pub(crate) teams: TeamsConnection,
}

/// Single-team fetch: keep each node's raw JSON so the object's canonical bytes
/// are exactly what Linear returned, mirroring the single-issue fetch.
#[derive(Debug, Deserialize)]
pub(crate) struct TeamByKeyData {
    pub(crate) teams: TeamRawConnection,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TeamRawConnection {
    pub(crate) nodes: Vec<Box<serde_json::value::RawValue>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IssuesConnection {
    pub(crate) page_info: PageInfo,
    pub(crate) nodes: Vec<Issue>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct IssuesData {
    pub(crate) issues: IssuesConnection,
}

#[derive(Debug, Deserialize)]
pub(crate) struct IssueNodeData {
    #[serde(rename = "issueVcsBranchSearch")]
    pub(crate) issue: Option<Box<serde_json::value::RawValue>>,
}

pub(crate) struct IssuePage {
    pub(crate) items: Vec<Issue>,
    pub(crate) truncated: bool,
}

/// Convert a numeric priority to its label. Linear uses 0=No priority,
/// 1=Urgent, 2=High, 3=Medium, 4=Low. The API returns the value as a
/// JSON number; only the integer values 0..=4 are meaningful.
pub(crate) fn priority_label(priority: Option<f64>) -> &'static str {
    match priority {
        Some(p) if (0.5..1.5).contains(&p) => "Urgent",
        Some(p) if (1.5..2.5).contains(&p) => "High",
        Some(p) if (2.5..3.5).contains(&p) => "Medium",
        Some(p) if (3.5..4.5).contains(&p) => "Low",
        _ => "No priority",
    }
}

/// Render a GraphQL request body. The endpoint's `body_json` encodes this as
/// the request's JSON body.
pub(crate) fn gql_request<T: serde::Serialize>(query: &str, variables: &T) -> serde_json::Value {
    json!({
        "query": query,
        "variables": variables,
    })
}
