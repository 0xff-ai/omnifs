//! Compose grouping. Project membership is detected from the
//! `com.docker.compose.project` and `com.docker.compose.service`
//! labels Docker Compose stamps on every container in a stack.

use hashbrown::{HashMap, HashSet};
use omnifs_sdk::prelude::*;
use serde::Serialize;

use crate::container_subtree::{ContainerKey, ContainerSubtree, strip_leading_slash};
use crate::containers::list_containers;
use crate::system::pretty_json;
use crate::types::{ContainerName, ProjectName, ServiceName};
use crate::wire::ContainerSummary;
use crate::{Result, State};

const PROJECT_LABEL: &str = "com.docker.compose.project";
const SERVICE_LABEL: &str = "com.docker.compose.service";

pub struct ComposeHandlers;

#[handlers]
impl ComposeHandlers {
    #[file("/compose/_listing.json")]
    async fn listing(cx: &Cx<State>) -> Result<FileContent> {
        let summaries = list_containers(cx).await?;
        let listing = ComposeListing::from(&summaries);
        Ok(FileContent::bytes(pretty_json(&listing)?))
    }

    /// `/compose/{project}` carries no scalar files of its own; the
    /// `services/` subdirectory auto-derives from the deeper routes.
    /// We just declare the listing exhaustive so absence-of-name
    /// lookups (`stat /compose/foo/missing`) don't ping the provider.
    #[dir("/compose/{project}")]
    fn project(_cx: &DirCx<State>, _project: ProjectName) -> Result<Projection> {
        let mut p = Projection::new();
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[dir("/compose/{project}/services")]
    async fn services(cx: &DirCx<State>, project: ProjectName) -> Result<Projection> {
        let summaries = list_containers(cx).await?;
        let services = services_for_project(&summaries, project.as_str());
        let mut p = Projection::new();
        for service in services {
            p.dir(service);
        }
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[dir("/compose/{project}/services/{service}")]
    fn service(
        _cx: &DirCx<State>,
        _project: ProjectName,
        _service: ServiceName,
    ) -> Result<Projection> {
        let mut p = Projection::new();
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[dir("/compose/{project}/services/{service}/containers")]
    async fn service_containers(
        cx: &DirCx<State>,
        project: ProjectName,
        service: ServiceName,
    ) -> Result<Projection> {
        let summaries = list_containers(cx).await?;
        let mut p = Projection::new();
        for name in containers_for_service(&summaries, project.as_str(), service.as_str()) {
            p.dir(name);
        }
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[bind("/compose/{project}/services/{service}/containers/{name}")]
    fn container(
        _cx: &Cx<State>,
        _project: ProjectName,
        _service: ServiceName,
        name: ContainerName,
    ) -> Result<ContainerSubtree> {
        Ok(ContainerSubtree {
            key: ContainerKey::Name(name.to_string()),
        })
    }
}

fn label<'a>(summary: &'a ContainerSummary, key: &str) -> Option<&'a str> {
    summary.labels.as_ref()?.get(key).map(String::as_str)
}

fn services_for_project(summaries: &[ContainerSummary], project: &str) -> Vec<String> {
    let mut services: HashSet<String> = HashSet::new();
    for summary in summaries {
        if label(summary, PROJECT_LABEL) != Some(project) {
            continue;
        }
        if let Some(service) = label(summary, SERVICE_LABEL) {
            services.insert(service.to_string());
        }
    }
    let mut sorted: Vec<String> = services.into_iter().collect();
    sorted.sort();
    sorted
}

fn containers_for_service(
    summaries: &[ContainerSummary],
    project: &str,
    service: &str,
) -> Vec<String> {
    let mut names: Vec<String> = summaries
        .iter()
        .filter(|s| label(s, PROJECT_LABEL) == Some(project))
        .filter(|s| label(s, SERVICE_LABEL) == Some(service))
        .flat_map(|s| s.names.iter().flatten())
        .map(|raw| strip_leading_slash(raw).to_string())
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

#[derive(Serialize)]
struct ComposeListing {
    projects: Vec<ProjectEntry>,
}

#[derive(Serialize)]
struct ProjectEntry {
    name: String,
    services: Vec<ServiceEntry>,
}

#[derive(Serialize)]
struct ServiceEntry {
    name: String,
    containers: Vec<String>,
}

impl ComposeListing {
    fn from(summaries: &[ContainerSummary]) -> Self {
        // project -> service -> container names. BTreeMap-ordered
        // output would be fine, but we keep things alphabetical via
        // a final sort so the snapshot is stable across daemon
        // restarts (the daemon does not guarantee summary order).
        let mut grouped: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
        for summary in summaries {
            let Some(project) = label(summary, PROJECT_LABEL) else {
                continue;
            };
            let Some(service) = label(summary, SERVICE_LABEL) else {
                continue;
            };
            let names: Vec<String> = summary
                .names
                .iter()
                .flatten()
                .map(|raw| strip_leading_slash(raw).to_string())
                .collect();
            let project_entry = grouped.entry(project.to_string()).or_default();
            project_entry
                .entry(service.to_string())
                .or_default()
                .extend(names);
        }

        let mut projects: Vec<ProjectEntry> = grouped
            .into_iter()
            .map(|(project, services)| {
                let mut svc_entries: Vec<ServiceEntry> = services
                    .into_iter()
                    .map(|(name, mut containers)| {
                        containers.sort();
                        containers.dedup();
                        ServiceEntry { name, containers }
                    })
                    .collect();
                svc_entries.sort_by(|a, b| a.name.cmp(&b.name));
                ProjectEntry {
                    name: project,
                    services: svc_entries,
                }
            })
            .collect();
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        Self { projects }
    }
}
