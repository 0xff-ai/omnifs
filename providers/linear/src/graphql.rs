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

use crate::types::StateType;

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

/// Issues filtered by state type, sorted by `updatedAt` desc. The
/// `description` field is the raw markdown body; it can be large, so the
/// provider should not preload it for every issue at scale.
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
      description
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

#[derive(Debug, Deserialize)]
pub(crate) struct PageInfo {
    #[serde(rename = "hasNextPage")]
    pub(crate) has_next_page: bool,
    #[serde(rename = "endCursor")]
    pub(crate) end_cursor: Option<String>,
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

#[derive(Debug, Deserialize)]
pub(crate) struct Team {
    pub(crate) key: String,
    pub(crate) name: String,
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
#[serde(rename_all = "camelCase")]
pub(crate) struct Issue {
    pub(crate) identifier: String,
    pub(crate) number: u64,
    pub(crate) title: String,
    /// Priority is an integer 0-4: 0=No priority, 1=Urgent, 2=High,
    /// 3=Medium, 4=Low (Linear's docs use these labels).
    pub(crate) priority: Option<f64>,
    pub(crate) updated_at: Option<String>,
    pub(crate) state: Option<IssueState>,
    pub(crate) assignee: Option<IssueAssignee>,
    pub(crate) description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct IssueState {
    pub(crate) name: String,
    pub(crate) r#type: StateType,
}

#[derive(Debug, Deserialize)]
pub(crate) struct IssueAssignee {
    /// `displayName` is the at-mention handle; `name` is the full name;
    /// `email` is the address. We surface whichever is non-empty, with
    /// `displayName` preferred for at-a-glance reads.
    #[serde(rename = "displayName")]
    pub(crate) display_name: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) email: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct IssueByIdentifierData {
    #[serde(rename = "issueVcsBranchSearch")]
    pub(crate) issue: Option<Issue>,
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

/// Render a GraphQL request body. The host's HTTP callout encodes this
/// as the request's JSON body.
pub(crate) fn gql_body<T: serde::Serialize>(query: &str, variables: &T) -> serde_json::Value {
    json!({
        "query": query,
        "variables": variables,
    })
}
