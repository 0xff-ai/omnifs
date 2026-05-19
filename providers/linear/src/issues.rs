use hashbrown::HashSet;
use omnifs_sdk::prelude::*;
use serde_json::json;

use crate::graphql::{
    ISSUE_BY_IDENTIFIER_QUERY, ISSUES_QUERY, Issue, IssueByIdentifierData, IssuesData,
    priority_label,
};
use crate::http_ext::LinearHttpExt;
use crate::types::{IssueIdent, StateFilter, TeamKey};
use crate::{Result, State};

pub struct IssueHandlers;

/// Conservative inline budget for description.md preloads. Keeping the
/// budget under `MAX_PROJECTED_BYTES` (64 KiB) is per-file; the response
/// also has to fit `MAX_EAGER_RESPONSE_BYTES` (512 KiB). With 50 issues
/// per page, 4 KiB per description keeps the worst-case payload under
/// cap even when titles/assignees are also inlined.
const MAX_INLINE_DESCRIPTION_BYTES: usize = 4 * 1024;

#[handlers]
impl IssueHandlers {
    /// `/teams/{KEY}/issues/{filter}` lists issue identifiers (e.g.
    /// `ENG-42`) for the team, filtered by `_open` or `_all`. Each
    /// listing also preloads small per-issue inline files so a `cat`
    /// after the `ls` avoids a round trip per file.
    #[dir("/teams/{team}/issues/{filter}")]
    async fn issues_list(
        cx: &DirCx<State>,
        team: TeamKey,
        filter: StateFilter,
    ) -> Result<Projection> {
        let issues = fetch_all_issues(cx, &team, filter).await?;
        let mut projection = Projection::new();
        let mut seen: HashSet<String> = HashSet::with_capacity(issues.len());
        for issue in &issues {
            if !seen.insert(issue.identifier.clone()) {
                continue;
            }
            if issue.identifier.parse::<IssueIdent>().is_err() {
                continue;
            }
            projection.dir(issue.identifier.clone());
            project_issue_files(&mut projection, &team, filter, issue);
        }
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    /// `/teams/{KEY}/issues/{filter}/{IDENT}` projects the five inline
    /// files for one issue. We re-fetch the issue here when the host
    /// asks for the directory (e.g. through `lookup_child` after a cold
    /// cache); on listings, the parent handler already preloads them.
    #[dir("/teams/{team}/issues/{filter}/{ident}")]
    async fn issue_dir(
        cx: &DirCx<State>,
        team: TeamKey,
        filter: StateFilter,
        ident: IssueIdent,
    ) -> Result<Projection> {
        validate_ident_team(&team, &ident)?;
        let issue = fetch_issue_by_identifier(cx, &ident).await?;
        let mut projection = Projection::new();
        write_inline_files(&mut projection, &issue, /*include_description=*/ true);
        // Preserve `_open` filter parents even though the parent listing
        // would normally have already projected the same files.
        let _ = filter;
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }
}

fn project_issue_files(
    projection: &mut Projection,
    team: &TeamKey,
    filter: StateFilter,
    issue: &Issue,
) {
    let base = format!(
        "teams/{team}/issues/{}/{}/",
        filter.as_ref(),
        issue.identifier
    );
    let version = issue.updated_at.as_deref();

    projection.proj_dir(base.trim_end_matches('/'));

    projection.proj_file(
        format!("{base}title"),
        inline_file(issue.title.clone(), version),
    );
    projection.proj_file(
        format!("{base}state"),
        inline_file(state_text(issue), version),
    );
    projection.proj_file(
        format!("{base}priority"),
        inline_file(priority_text(issue), version),
    );
    projection.proj_file(
        format!("{base}assignee"),
        inline_file(assignee_text(issue), version),
    );

    let description = description_text(issue);
    if description.len() <= MAX_INLINE_DESCRIPTION_BYTES {
        projection.proj_file(
            format!("{base}description.md"),
            inline_file(description, version),
        );
    } else {
        // Larger descriptions are served via the per-issue subtree
        // through a fresh fetch on read; declare the deferred file in
        // the listing so it still appears in `ls`.
        projection.proj_file(format!("{base}description.md"), deferred_text_file(version));
    }
}

fn write_inline_files(projection: &mut Projection, issue: &Issue, include_description: bool) {
    let version = issue.updated_at.as_deref();
    projection.file("title", inline_file(issue.title.clone(), version));
    projection.file("state", inline_file(state_text(issue), version));
    projection.file("priority", inline_file(priority_text(issue), version));
    projection.file("assignee", inline_file(assignee_text(issue), version));
    if include_description {
        let description = description_text(issue);
        if description.len() <= MAX_INLINE_DESCRIPTION_BYTES {
            projection.file("description.md", inline_file(description, version));
        } else {
            projection.file("description.md", deferred_text_file(version));
        }
    }
}

fn inline_file(text: impl Into<String>, version: Option<&str>) -> FileProj {
    let mut bytes = text.into().into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    FileProj::inline(bytes, Stability::Mutable, version_token(version))
}

fn deferred_text_file(version: Option<&str>) -> FileProj {
    let file = FileProj::deferred(Size::Unknown, ReadMode::Full, Stability::Mutable);
    match version_token(version) {
        Some(tok) => file.with_version(tok),
        None => file,
    }
}

fn version_token(version: Option<&str>) -> Option<VersionToken> {
    version.filter(|v| !v.is_empty()).map(VersionToken::from)
}

fn state_text(issue: &Issue) -> String {
    issue
        .state
        .as_ref()
        .map(|state| state.name.clone())
        .unwrap_or_default()
}

fn priority_text(issue: &Issue) -> String {
    priority_label(issue.priority).to_string()
}

fn assignee_text(issue: &Issue) -> String {
    issue
        .assignee
        .as_ref()
        .and_then(|a| {
            a.display_name
                .clone()
                .or_else(|| a.name.clone())
                .or_else(|| a.email.clone())
        })
        .unwrap_or_default()
}

fn description_text(issue: &Issue) -> String {
    issue.description.clone().unwrap_or_default()
}

fn validate_ident_team(team: &TeamKey, ident: &IssueIdent) -> Result<()> {
    if ident.team() != team {
        return Err(ProviderError::not_found(
            "issue identifier does not match team key",
        ));
    }
    Ok(())
}

async fn fetch_all_issues(
    cx: &Cx<State>,
    team: &TeamKey,
    filter: StateFilter,
) -> Result<Vec<Issue>> {
    let state_types = match filter {
        StateFilter::Open => vec!["triage", "backlog", "unstarted", "started"],
        StateFilter::All => Vec::new(),
    };
    // `filter.state.type.in` is treated as "match anything" when omitted;
    // Linear's GraphQL accepts an empty list as "no constraint" in the
    // `_all` direction. Send `null` instead of `[]` to be safe.
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

    let mut out: Vec<Issue> = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let vars = json!({
            "teamKey": team.as_str(),
            "stateTypes": state_filter,
            "after": after,
        });
        let data: IssuesData = cx.graphql(ISSUES_QUERY, &vars).await?;
        out.extend(data.issues.nodes);
        if !data.issues.page_info.has_next_page {
            break;
        }
        match data.issues.page_info.end_cursor {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
        // Soft cap to avoid runaway pagination on huge workspaces. v1
        // assumes "modest scale" (hundreds of issues); raise this when
        // the use case grows.
        if out.len() >= 2000 {
            break;
        }
    }
    Ok(out)
}

pub(crate) async fn fetch_issue_by_identifier(cx: &Cx<State>, ident: &IssueIdent) -> Result<Issue> {
    let vars = json!({ "id": ident.to_string() });
    let data: IssueByIdentifierData = cx.graphql(ISSUE_BY_IDENTIFIER_QUERY, &vars).await?;
    data.issue
        .ok_or_else(|| ProviderError::not_found(format!("issue not found: {ident}")))
}
