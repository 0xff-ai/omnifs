#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

//! docker-provider: Docker daemon virtual filesystem provider for omnifs.

pub(crate) use omnifs_sdk::prelude::Result;

use core::fmt;
use core::str::FromStr;

use hashbrown::HashMap;
use omnifs_sdk::prelude::*;
use serde::Serialize;

mod api;

use crate::api::{
    ContainerInspectResponse, ContainerSummary, SystemDataUsageResponse, SystemInfo, SystemVersion,
    fetch_bytes, fetch_json,
};

#[derive(Clone)]
#[omnifs_sdk::config]
pub struct Config {
    #[allow(dead_code)]
    #[serde(default = "default_endpoint")]
    endpoint: String,
}

fn default_endpoint() -> String {
    "unix:///var/run/docker.sock".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
        }
    }
}

fn is_valid_docker_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn is_hex_id(value: &str) -> bool {
    (12..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContainerRef(String);

impl ContainerRef {
    fn parse_normalized(value: &str) -> std::result::Result<Self, ()> {
        let trimmed = value.strip_prefix('/').unwrap_or(value);
        if is_valid_docker_name(trimmed) || is_hex_id(trimmed) {
            Ok(Self(trimmed.to_string()))
        } else {
            Err(())
        }
    }
}

impl FromStr for ContainerRef {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse_normalized(value)
    }
}

impl fmt::Display for ContainerRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectName(String);

impl ProjectName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ProjectName {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if is_valid_docker_name(value) {
            Ok(Self(value.to_string()))
        } else {
            Err(())
        }
    }
}

impl fmt::Display for ProjectName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceName(String);

impl ServiceName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ServiceName {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if is_valid_docker_name(value) {
            Ok(Self(value.to_string()))
        } else {
            Err(())
        }
    }
}

impl fmt::Display for ServiceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[omnifs_sdk::path_captures]
pub struct ContainerKey {
    reference: ContainerRef,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Container(ContainerInspectResponse);

impl Container {
    fn state_bytes(&self) -> Vec<u8> {
        let status = self
            .0
            .state
            .as_ref()
            .and_then(|state| state.status.as_ref())
            .map_or_else(
                || "unknown".to_string(),
                |status| status.as_ref().to_string(),
            );
        let mut bytes = status.into_bytes();
        bytes.push(b'\n');
        bytes
    }

    fn summary_bytes(&self) -> Vec<u8> {
        use std::fmt::Write;

        let inspect = &self.0;
        let id = inspect.id.as_deref().unwrap_or("");
        let short_id = id.get(..12).unwrap_or(id);
        let name = inspect.name.as_deref().map_or("", strip_leading_slash);
        let image = inspect
            .config
            .as_ref()
            .and_then(|config| config.image.as_deref())
            .unwrap_or("");
        let state = inspect
            .state
            .as_ref()
            .and_then(|state| state.status.as_ref())
            .map(|status| status.as_ref().to_string())
            .unwrap_or_default();
        let status = inspect
            .state
            .as_ref()
            .and_then(|state| state.running)
            .map(|running| running.to_string())
            .unwrap_or_default();
        let mut text = String::new();
        let _ = writeln!(text, "id     {short_id}");
        let _ = writeln!(text, "name   {name}");
        let _ = writeln!(text, "image  {image}");
        let _ = writeln!(text, "state  {state}");
        let _ = writeln!(text, "status {status}");
        text.into_bytes()
    }
}

#[omnifs_sdk::path_captures]
struct ProjectKey {
    project: ProjectName,
}

#[omnifs_sdk::path_captures]
struct ProjectServiceKey {
    project: ProjectName,
    service: ServiceName,
}

#[omnifs_sdk::provider(
    metadata = "omnifs.provider.json",
    resources(endpoints = [crate::api::DockerApi]),
)]
impl DockerProvider {
    type Config = Config;

    fn start(_config: Config, r: &mut Router) -> Result<()> {
        r.file("/system/info.json").handler(system_info)?;
        r.file("/system/version.json").handler(system_version)?;
        r.file("/system/df.json").handler(system_df)?;
        r.file("/system/ping").handler(system_ping)?;

        r.file("/containers.json").handler(containers_listing)?;
        r.file("/compose.json").handler(compose_listing)?;

        r.dir("/containers/by-name").handler(by_name)?;
        r.dir("/containers/by-name/{reference}")
            .handler(container_dir)?;
        r.file("/containers/by-name/{reference}/inspect.json")
            .handler(container_inspect)?;
        r.file("/containers/by-name/{reference}/state")
            .handler(container_state)?;
        r.file("/containers/by-name/{reference}/summary.txt")
            .handler(container_summary)?;

        r.dir("/containers/by-id").handler(by_id)?;
        r.dir("/containers/by-id/{reference}")
            .handler(container_dir)?;
        r.file("/containers/by-id/{reference}/inspect.json")
            .handler(container_inspect)?;
        r.file("/containers/by-id/{reference}/state")
            .handler(container_state)?;
        r.file("/containers/by-id/{reference}/summary.txt")
            .handler(container_summary)?;

        r.dir("/containers/running").handler(running)?;
        r.dir("/containers/running/{reference}")
            .handler(container_dir)?;
        r.file("/containers/running/{reference}/inspect.json")
            .handler(container_inspect)?;
        r.file("/containers/running/{reference}/state")
            .handler(container_state)?;
        r.file("/containers/running/{reference}/summary.txt")
            .handler(container_summary)?;

        r.dir("/containers/stopped").handler(stopped)?;
        r.dir("/containers/stopped/{reference}")
            .handler(container_dir)?;
        r.file("/containers/stopped/{reference}/inspect.json")
            .handler(container_inspect)?;
        r.file("/containers/stopped/{reference}/state")
            .handler(container_state)?;
        r.file("/containers/stopped/{reference}/summary.txt")
            .handler(container_summary)?;

        r.dir("/compose/{project}").handler(project_dir)?;
        r.dir("/compose/{project}/services")
            .handler(project_services)?;
        r.dir("/compose/{project}/services/{service}")
            .handler(service_dir)?;
        r.dir("/compose/{project}/services/{service}/containers")
            .handler(service_containers)?;
        r.dir("/compose/{project}/services/{service}/containers/{reference}")
            .handler(container_dir)?;
        r.file("/compose/{project}/services/{service}/containers/{reference}/inspect.json")
            .handler(container_inspect)?;
        r.file("/compose/{project}/services/{service}/containers/{reference}/state")
            .handler(container_state)?;
        r.file("/compose/{project}/services/{service}/containers/{reference}/summary.txt")
            .handler(container_summary)?;

        Ok(())
    }
}

const PROJECT_LABEL: &str = "com.docker.compose.project";
const SERVICE_LABEL: &str = "com.docker.compose.service";

async fn project_dir(_cx: DirCx, _key: ProjectKey) -> Result<DirProjection> {
    Ok(DirProjection::exhaustive(core::iter::empty::<Entry>()))
}

async fn project_services(cx: DirCx, key: ProjectKey) -> Result<DirProjection> {
    let summaries = list_containers(&cx).await?;
    let services = services_for_project(&summaries, key.project.as_str());
    Ok(DirProjection::exhaustive(
        services.into_iter().map(Entry::dir),
    ))
}

async fn service_dir(_cx: DirCx, _key: ProjectServiceKey) -> Result<DirProjection> {
    Ok(DirProjection::exhaustive(core::iter::empty::<Entry>()))
}

async fn service_containers(cx: DirCx, key: ProjectServiceKey) -> Result<DirProjection> {
    let summaries = list_containers(&cx).await?;
    let names = containers_for_service(&summaries, key.project.as_str(), key.service.as_str());
    Ok(DirProjection::exhaustive(names.into_iter().map(Entry::dir)))
}

async fn system_info(cx: Cx) -> Result<FileProjection> {
    let info: SystemInfo = fetch_json(&cx, "/info", &[]).await?;
    Ok(snapshot_body(pretty_json(&info)?))
}

async fn system_version(cx: Cx) -> Result<FileProjection> {
    let version: SystemVersion = fetch_json(&cx, "/version", &[]).await?;
    Ok(snapshot_body(pretty_json(&version)?))
}

async fn system_df(cx: Cx) -> Result<FileProjection> {
    let usage: SystemDataUsageResponse = fetch_json(&cx, "/system/df", &[]).await?;
    Ok(snapshot_body(pretty_json(&usage)?))
}

async fn system_ping(cx: Cx) -> Result<FileProjection> {
    let mut bytes = fetch_bytes(&cx, "/_ping", &[]).await?;
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    Ok(snapshot_body(bytes))
}

async fn containers_listing(cx: Cx) -> Result<FileProjection> {
    let summaries = list_containers(&cx).await?;
    Ok(snapshot_body(pretty_json(&summaries)?))
}

async fn by_name(cx: DirCx) -> Result<DirProjection> {
    let summaries = list_containers(&cx).await?;
    Ok(DirProjection::exhaustive(
        container_names(&summaries).into_iter().map(Entry::dir),
    ))
}

async fn by_id(cx: DirCx) -> Result<DirProjection> {
    let summaries = list_containers(&cx).await?;
    Ok(DirProjection::exhaustive(
        container_ids(&summaries).into_iter().map(Entry::dir),
    ))
}

async fn running(cx: DirCx) -> Result<DirProjection> {
    listing_filtered(&cx, |summary| {
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

async fn stopped(cx: DirCx) -> Result<DirProjection> {
    listing_filtered(&cx, |summary| {
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

async fn listing_filtered<F>(cx: &DirCx, predicate: F) -> Result<DirProjection>
where
    F: Fn(&ContainerSummary) -> bool,
{
    let summaries = list_containers(cx).await?;
    let names = summaries
        .iter()
        .filter(|summary| predicate(summary))
        .flat_map(|summary| summary.names.iter().flatten())
        .map(|raw| strip_leading_slash(raw).to_string())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    Ok(DirProjection::exhaustive(names.into_iter().map(Entry::dir)))
}

async fn compose_listing(cx: Cx) -> Result<FileProjection> {
    let summaries = list_containers(&cx).await?;
    let listing = ComposeListing::from(&summaries);
    Ok(snapshot_body(pretty_json(&listing)?))
}

async fn container_dir(_cx: DirCx, _key: ContainerKey) -> Result<DirProjection> {
    Ok(DirProjection::exhaustive([
        Entry::file("inspect.json"),
        Entry::file("state"),
        Entry::file("summary.txt"),
    ]))
}

async fn container_inspect(cx: Cx, key: ContainerKey) -> Result<FileProjection> {
    let bytes = fetch_bytes(&cx, &format!("/containers/{}/json", key.reference), &[]).await?;
    Ok(snapshot_body(bytes))
}

async fn container_state(cx: Cx, key: ContainerKey) -> Result<FileProjection> {
    let container = fetch_container(&cx, &key.reference).await?;
    Ok(snapshot_body(container.state_bytes()))
}

async fn container_summary(cx: Cx, key: ContainerKey) -> Result<FileProjection> {
    let container = fetch_container(&cx, &key.reference).await?;
    Ok(snapshot_body(container.summary_bytes()))
}

async fn fetch_container(cx: &Cx, reference: &ContainerRef) -> Result<Container> {
    fetch_json(cx, &format!("/containers/{reference}/json"), &[])
        .await
        .map(Container)
}

fn snapshot_body(bytes: Vec<u8>) -> FileProjection {
    FileProjection::body(bytes).mutable().build()
}

pub(crate) async fn list_containers(cx: &Cx) -> Result<Vec<ContainerSummary>> {
    fetch_json(cx, "/containers/json", &[("all", "true")]).await
}

pub(crate) fn pretty_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| ProviderError::internal(format!("docker JSON encode error: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn strip_leading_slash(raw: &str) -> &str {
    raw.strip_prefix('/').unwrap_or(raw)
}

fn container_names(summaries: &[ContainerSummary]) -> Vec<String> {
    let mut names: Vec<String> = summaries
        .iter()
        .flat_map(|summary| summary.names.iter().flatten())
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
        .filter_map(|summary| {
            summary
                .id
                .as_ref()
                .and_then(|id| id.get(..12).map(str::to_string))
        })
        .collect()
}

fn label<'a>(summary: &'a ContainerSummary, key: &str) -> Option<&'a str> {
    summary.labels.as_ref()?.get(key).map(String::as_str)
}

fn services_for_project(summaries: &[ContainerSummary], project: &str) -> Vec<String> {
    let mut services: hashbrown::HashSet<String> = hashbrown::HashSet::new();
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
        .filter(|summary| label(summary, PROJECT_LABEL) == Some(project))
        .filter(|summary| label(summary, SERVICE_LABEL) == Some(service))
        .flat_map(|summary| summary.names.iter().flatten())
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
            grouped
                .entry(project.to_string())
                .or_default()
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
