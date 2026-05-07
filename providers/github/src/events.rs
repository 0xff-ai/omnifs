use http::Response;
use omnifs_sdk::Cx;
use omnifs_sdk::prelude::*;
use serde::Deserialize;

use crate::http_ext::GithubHttpExt;
use crate::parse_model;
use crate::repo::RepoPath;
use crate::types::RepoId;
use crate::{EVENT_LOG_CAPACITY, Result, State};

/// Absolute path of the in-memory event tail file, exposed at the mount root.
const EVENTS_LOG_PATH: &str = "/.events";

#[derive(Debug, Deserialize)]
struct GithubEvent {
    #[serde(rename = "type")]
    event_type: String,
}

struct TickOutcome {
    repo_id: RepoId,
    response: Result<Response<Vec<u8>>>,
}

pub(crate) async fn timer_tick(cx: Cx<State>) -> Result<EventOutcome> {
    let mut outcome = EventOutcome::new();

    let mut repo_paths = cx.active_paths(RepoPath::MOUNT_ID, RepoId::parse);
    repo_paths.sort();
    repo_paths.dedup();

    if repo_paths.is_empty() {
        return Ok(outcome);
    }

    let fetches = repo_paths.into_iter().map(|repo_id| {
        let cx = cx.clone();
        let etag = cx.state(|state| state.event_etags.get(&repo_id).cloned());
        async move {
            let path = format!("/repos/{repo_id}/events?per_page=30");
            let mut req = cx.github_json_request(path);
            if let Some(etag) = etag {
                req = req.header("If-None-Match", etag);
            }
            let response = req.send().await;
            TickOutcome { repo_id, response }
        }
    });
    let outcomes = join_all(fetches).await;

    let mut etag_updates = Vec::new();
    let mut invalidations = hashbrown::HashSet::new();
    for tick in outcomes {
        let Ok(response) = tick.response else {
            continue;
        };
        let status = response.status().as_u16();
        if status == 304 || status >= 400 {
            continue;
        }
        if let Some(etag) = response
            .headers()
            .get(http::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
        {
            etag_updates.push((tick.repo_id.clone(), etag));
        }
        let Ok(events) = parse_model::<Vec<GithubEvent>>(response.body()) else {
            continue;
        };
        for event in events {
            let base = format!("{}/_", tick.repo_id);
            match event.event_type.as_str() {
                "IssuesEvent" => {
                    invalidations.insert(format!("{base}issues"));
                },
                "PullRequestEvent" => {
                    invalidations.insert(format!("{base}prs"));
                },
                "WorkflowRunEvent" => {
                    invalidations.insert(format!("{base}actions/runs"));
                },
                "IssueCommentEvent" => {
                    invalidations.insert(format!("{base}issues"));
                    invalidations.insert(format!("{base}prs"));
                },
                _ => {},
            }
        }
    }

    if !etag_updates.is_empty() {
        cx.state_mut(|state| {
            for (repo, etag) in etag_updates.drain(..) {
                state.event_etags.insert(repo, etag);
            }
        });
    }

    let invalidation_summary = invalidations.iter().cloned().collect::<Vec<_>>();
    for prefix in invalidations {
        outcome.invalidate_prefix(prefix);
    }

    let log_line = build_tick_log_line(&invalidation_summary);
    cx.state_mut(|state| append_event_line(state, log_line));
    outcome.invalidate_path(EVENTS_LOG_PATH);

    Ok(outcome)
}

fn build_tick_log_line(invalidations: &[String]) -> String {
    // NDJSON: keep field order deterministic so cached reads diff cleanly.
    let kinds = invalidations
        .iter()
        .map(|p| serde_json::Value::String(p.clone()))
        .collect::<Vec<_>>();
    let entry = serde_json::json!({
        "event": "timer_tick",
        "invalidate_prefixes": kinds,
    });
    let mut line = entry.to_string();
    line.push('\n');
    line
}

pub(crate) fn append_event_line(state: &mut State, line: String) {
    if state.event_log.len() == EVENT_LOG_CAPACITY {
        state.event_log.pop_front();
    }
    state.event_log.push_back(line);
}

/// Render the current event ring buffer as NDJSON bytes for the
/// `<mount>/.events` projected file. Oldest entry first.
pub(crate) fn events_log_bytes(state: &State) -> Vec<u8> {
    let mut out = Vec::new();
    for line in &state.event_log {
        out.extend_from_slice(line.as_bytes());
    }
    out
}
