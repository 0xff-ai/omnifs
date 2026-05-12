use http::Response;
use omnifs_sdk::Cx;
use omnifs_sdk::prelude::*;
use serde::Deserialize;

use crate::http_ext::GithubHttpExt;
use crate::parse_model;
use crate::repo::RepoPath;
use crate::types::RepoId;
use crate::{Result, State};

#[derive(Debug, Deserialize)]
struct GithubEvent {
    #[serde(rename = "type")]
    event_type: String,
}

struct TickOutcome {
    repo_id: RepoId,
    response: Result<Response<Vec<u8>>>,
}

pub(crate) async fn timer_tick(cx: Cx<State>) -> Result<Effects> {
    let mut effects = Effects::new();

    let mut repo_paths = cx.active_paths(RepoPath::MOUNT_ID, RepoId::parse);
    repo_paths.sort();
    repo_paths.dedup();

    if repo_paths.is_empty() {
        return Ok(effects);
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

    for prefix in invalidations {
        effects.invalidate_prefix(prefix);
    }

    Ok(effects)
}
