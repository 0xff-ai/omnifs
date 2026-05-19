//! Timer-tick polling of `/events?since&until` with simple
//! invalidation. Each tick:
//!
//!   1. Records the previous tick's timestamp as `since` and `now` as
//!      `until`. The first tick uses `until` as both bounds (no
//!      backfill of unbounded history).
//!   2. Fetches the bounded event list. With both `since` and
//!      `until`, the daemon returns a finite NDJSON response instead
//!      of an unbounded stream.
//!   3. Translates each container event into invalidation prefixes
//!      that the host applies at the response boundary.

use omnifs_sdk::http::ResponseExt;
use omnifs_sdk::prelude::*;

use crate::wire::EventMessage;
use crate::{Result, State};

/// 64 entries is plenty for a single tick on a busy host; the
/// invalidation set is keyed by prefix so duplicates collapse.
const INVALIDATION_HINT: usize = 64;

pub(crate) async fn timer_tick(cx: Cx<State>) -> Result<Effects> {
    let now = cx.state(State::clock_now_secs);
    let since = cx.state_mut(|state| state.events.checkpoint(now));

    let url = cx.state(|state| {
        state.api.url(
            "/events",
            &[
                ("since", &since.to_string()),
                ("until", &now.to_string()),
                ("type", "container"),
            ],
        )
    });

    let response = cx.http().get(url).send().await?;
    let response = match response.error_for_status() {
        Ok(response) => response,
        Err(error) => {
            // A failed tick is not fatal; the next tick re-asks
            // from the same checkpoint and catches up.
            cx.state_mut(|state| state.events.rewind(since));
            return Err(error);
        },
    };

    let events = parse_ndjson(response.body());
    let mut outcome = Effects::new();
    let mut prefixes = hashbrown::HashSet::with_capacity(INVALIDATION_HINT);

    for event in events {
        // Container events identify the affected container in the
        // actor record. We emit prefix invalidations for every alias
        // the container is reachable through; the host coalesces
        // overlapping prefixes when applying the outcome.
        let Some(actor) = event.actor else {
            continue;
        };
        if let Some(id) = actor.id.as_deref()
            && let Some(short) = id.get(..12)
        {
            prefixes.insert(format!("containers/by-id/{short}"));
        }
        let attributes = actor.attributes.unwrap_or_default();
        if let Some(name) = attributes.get("name") {
            let name = name.trim_start_matches('/');
            if !name.is_empty() {
                prefixes.insert(format!("containers/by-name/{name}"));
                prefixes.insert(format!("containers/_running/{name}"));
                prefixes.insert(format!("containers/_stopped/{name}"));
            }
        }
        // Compose containers: invalidate the per-service container
        // listing so a freshly-up replica appears under
        // `/compose/{project}/services/{service}/containers/`.
        if let (Some(project), Some(service)) = (
            attributes.get("com.docker.compose.project"),
            attributes.get("com.docker.compose.service"),
        ) {
            prefixes.insert(format!("compose/{project}/services/{service}/containers"));
        }
    }

    // The cross-cutting facets always need to be re-evaluated when
    // any container event lands; they index by state, which the
    // event payload doesn't carry, so we just blow them away rather
    // than try to infer transitions.
    if !prefixes.is_empty() {
        prefixes.insert("containers/_listing.json".to_string());
        prefixes.insert("containers/_running".to_string());
        prefixes.insert("containers/_stopped".to_string());
        prefixes.insert("compose/_listing.json".to_string());
    }

    for prefix in prefixes {
        outcome.invalidate_prefix(prefix);
    }
    Ok(outcome)
}

fn parse_ndjson(body: &[u8]) -> Vec<EventMessage> {
    body.split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<EventMessage>(line).ok())
        .collect()
}
