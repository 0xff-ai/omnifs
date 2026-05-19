use omnifs_sdk::prelude::*;

use crate::api::fetch_json;
use crate::container_subtree::{ContainerKey, ContainerSubtree, strip_leading_slash};
use crate::system::pretty_json;
use crate::types::{ContainerId, ContainerName};
use crate::wire::ContainerSummary;
use crate::{Result, State};

pub struct ContainerHandlers;

#[handlers]
impl ContainerHandlers {
    #[file("/containers/_listing.json")]
    async fn listing(cx: &Cx<State>) -> Result<FileContent> {
        let summaries = list_containers(cx).await?;
        Ok(FileContent::bytes(pretty_json(&summaries)?))
    }

    #[dir("/containers/by-name")]
    async fn by_name(cx: &DirCx<State>) -> Result<Projection> {
        let summaries = list_containers(cx).await?;
        let mut p = Projection::new();
        for name in container_names(&summaries) {
            p.dir(name);
        }
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[bind("/containers/by-name/{name}")]
    fn by_name_container(_cx: &Cx<State>, name: ContainerName) -> Result<ContainerSubtree> {
        Ok(ContainerSubtree {
            key: ContainerKey::Name(name.to_string()),
        })
    }

    #[dir("/containers/by-id")]
    async fn by_id(cx: &DirCx<State>) -> Result<Projection> {
        let summaries = list_containers(cx).await?;
        let mut p = Projection::new();
        for id in container_ids(&summaries) {
            p.dir(id);
        }
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[bind("/containers/by-id/{id}")]
    fn by_id_container(_cx: &Cx<State>, id: ContainerId) -> Result<ContainerSubtree> {
        Ok(ContainerSubtree {
            key: ContainerKey::Id(id.to_string()),
        })
    }

    #[dir("/containers/_running")]
    async fn running(cx: &DirCx<State>) -> Result<Projection> {
        listing_filtered(cx, |summary| {
            // Match by `state` (the structured enum) when the daemon
            // returns it; fall back to the free-form `status` text
            // for older daemons that omit the new field.
            if let Some(state) = summary.state {
                state.as_ref() == "running"
            } else {
                summary
                    .status
                    .as_deref()
                    .is_some_and(|status| status.starts_with("Up"))
            }
        })
        .await
    }

    #[bind("/containers/_running/{name}")]
    fn running_container(_cx: &Cx<State>, name: ContainerName) -> Result<ContainerSubtree> {
        Ok(ContainerSubtree {
            key: ContainerKey::Name(name.to_string()),
        })
    }

    #[dir("/containers/_stopped")]
    async fn stopped(cx: &DirCx<State>) -> Result<Projection> {
        listing_filtered(cx, |summary| {
            if let Some(state) = summary.state {
                let s = state.as_ref();
                s == "exited" || s == "dead" || s == "created"
            } else {
                summary
                    .status
                    .as_deref()
                    .is_some_and(|status| status.starts_with("Exited"))
            }
        })
        .await
    }

    #[bind("/containers/_stopped/{name}")]
    fn stopped_container(_cx: &Cx<State>, name: ContainerName) -> Result<ContainerSubtree> {
        Ok(ContainerSubtree {
            key: ContainerKey::Name(name.to_string()),
        })
    }
}

async fn listing_filtered<F>(cx: &Cx<State>, predicate: F) -> Result<Projection>
where
    F: Fn(ContainerSummary) -> bool,
{
    let summaries = list_containers(cx).await?;
    let mut p = Projection::new();
    for summary in summaries {
        let names = summary.names.clone().unwrap_or_default();
        if !predicate(summary) {
            continue;
        }
        for raw in names {
            let trimmed = strip_leading_slash(&raw).to_string();
            if !trimmed.is_empty() {
                p.dir(trimmed);
            }
        }
    }
    p.page(PageStatus::Exhaustive);
    Ok(p)
}

pub(crate) async fn list_containers(cx: &Cx<State>) -> Result<Vec<ContainerSummary>> {
    fetch_json(cx, "/containers/json", &[("all", "true")]).await
}

fn container_names(summaries: &[ContainerSummary]) -> Vec<String> {
    let mut names: Vec<String> = summaries
        .iter()
        .flat_map(|s| s.names.iter().flatten())
        .map(|raw| strip_leading_slash(raw).to_string())
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn container_ids(summaries: &[ContainerSummary]) -> Vec<String> {
    summaries
        .iter()
        .filter_map(|s| {
            s.id.as_ref()
                .and_then(|id| id.get(..12).map(str::to_string))
        })
        .collect()
}
