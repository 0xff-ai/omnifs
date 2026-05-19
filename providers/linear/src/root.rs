use omnifs_sdk::prelude::*;
use serde_json::json;

use crate::graphql::{TEAMS_QUERY, TeamsData};
use crate::http_ext::LinearHttpExt;
use crate::types::TeamKey;
use crate::{Result, State};

pub struct RootHandlers;

#[handlers]
impl RootHandlers {
    /// `/teams` lists every team in the workspace by team key. Linear's
    /// `teams` connection paginates; the v1 surface flattens all pages
    /// into a single listing so plain `ls` works without per-team
    /// follow-ups.
    #[dir("/teams")]
    async fn teams(cx: &DirCx<State>) -> Result<Projection> {
        let teams = fetch_all_teams(cx).await?;
        let mut projection = Projection::new();
        for team in teams {
            // Reject team keys that wouldn't parse as a `TeamKey`; the
            // alternative would be to surface them as directories the
            // SDK can't resolve back through `lookup_child`.
            if team.key.parse::<TeamKey>().is_ok() {
                projection.dir(team.key);
            }
        }
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }
}

async fn fetch_all_teams(cx: &Cx<State>) -> Result<Vec<crate::graphql::Team>> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let vars = json!({ "after": after });
        let data: TeamsData = cx.graphql(TEAMS_QUERY, &vars).await?;
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
