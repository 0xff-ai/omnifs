//! Kubernetes API access, discovery cataloging, and related-view rendering.

use core::fmt::Write as _;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
    APIGroup, APIGroupList, APIResource as K8sApiResource, APIResourceList,
};
use omnifs_sdk::error::ProviderErrorKind;
use omnifs_sdk::hashbrown::HashMap;
use omnifs_sdk::prelude::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use core::cell::RefCell;

use omnifs_sdk::handler::BoxFuture;

use crate::State;
use crate::objects::KubeManifest;

const ACCEPT_JSON: &str = "application/json";

/// The Kubernetes API server endpoint. Its base is the in-cluster API URL
/// resolved from provider state at call time, so it carries a field rather
/// than a `#[derive(Endpoint)]` constant base.
struct KubeEndpoint {
    base: String,
}

impl Endpoint for KubeEndpoint {
    fn base(&self) -> &str {
        &self.base
    }
}
impl EndpointHooks for KubeEndpoint {}

/// One discovered, browsable Kubernetes resource.
#[derive(Clone, Debug)]
pub(crate) struct Resource {
    api_root: String,
    plural: String,
    kind: String,
    group: String,
    pub(crate) namespaced: bool,
}

impl Resource {
    pub(crate) fn collection_path(&self, namespace: Option<&str>) -> String {
        match namespace {
            Some(ns) => format!("{}/namespaces/{}/{}", self.api_root, ns, self.plural),
            None => format!("{}/{}", self.api_root, self.plural),
        }
    }

    pub(crate) fn object_path(&self, namespace: Option<&str>, name: &str) -> String {
        format!("{}/{}", self.collection_path(namespace), name)
    }

    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }
}

/// Resource discovery indexed by filesystem-facing type name.
#[derive(Debug, Default)]
pub(crate) struct Discovery {
    by_plural: HashMap<String, Resource>,
}

impl Discovery {
    fn insert(&mut self, fs_name: String, resource: Resource) {
        self.by_plural.entry(fs_name).or_insert(resource);
    }

    fn add_resources(&mut self, api_root: &str, group: &str, resources: &[K8sApiResource]) {
        for entry in resources {
            if !is_browsable(entry) || self.has_group_resource(group, &entry.name) {
                continue;
            }
            let resource = Resource {
                api_root: api_root.to_string(),
                plural: entry.name.clone(),
                kind: entry.kind.clone(),
                group: group.to_string(),
                namespaced: entry.namespaced,
            };
            let fs_name = if !group.is_empty() && self.get(&entry.name).is_some() {
                format!("{}.{}", entry.name, group)
            } else {
                entry.name.clone()
            };
            self.insert(fs_name, resource);
        }
    }

    fn get(&self, fs_plural: &str) -> Option<&Resource> {
        self.by_plural.get(fs_plural)
    }

    fn has_group_resource(&self, group: &str, plural: &str) -> bool {
        self.by_plural
            .values()
            .any(|r| r.group == group && r.plural == plural)
    }

    fn sorted_types(&self, namespaced: bool) -> Vec<String> {
        let mut names: Vec<String> = self
            .by_plural
            .iter()
            .filter(|(_, resource)| resource.namespaced == namespaced)
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }
}

fn is_browsable(entry: &K8sApiResource) -> bool {
    !entry.name.contains('/')
        && entry.verbs.iter().any(|verb| verb == "get")
        && entry.verbs.iter().any(|verb| verb == "list")
}

fn group_versions_preferred_first(group: &APIGroup) -> Vec<String> {
    let preferred = group
        .preferred_version
        .as_ref()
        .map(|version| version.group_version.as_str())
        .filter(|version| !version.is_empty());
    let mut versions = preferred.into_iter().map(str::to_owned).collect::<Vec<_>>();
    versions.extend(
        group
            .versions
            .iter()
            .map(|version| version.group_version.clone())
            .filter(|version| !version.is_empty() && Some(version.as_str()) != preferred),
    );
    versions
}

/// Operation-scoped Kubernetes API adapter. It owns URL construction, HTTP
/// status policy, and JSON decoding while still lowering every request through
/// Omnifs host callouts.
pub(crate) struct KubeApi<'a> {
    cx: &'a Cx<State>,
    base: String,
}

impl<'a> KubeApi<'a> {
    pub(crate) fn new(cx: &'a Cx<State>) -> Self {
        Self {
            cx,
            base: cx.state(|state| state.endpoint.clone()),
        }
    }

    fn endpoint(&self) -> KubeEndpoint {
        KubeEndpoint {
            base: self.base.clone(),
        }
    }

    pub(crate) async fn ensure_discovery(&self) -> Result<()> {
        if self.cx.state(|state| state.discovery.borrow().is_some()) {
            return Ok(());
        }
        let discovery = self.fetch_discovery().await?;
        self.cx.state(|state| {
            *state.discovery.borrow_mut() = Some(discovery);
        });
        Ok(())
    }

    pub(crate) async fn resource(&self, fs_plural: &str) -> Result<Resource> {
        self.ensure_discovery().await?;
        self.cx.state(|state| {
            state
                .discovery
                .borrow()
                .as_ref()
                .and_then(|discovery| discovery.get(fs_plural).cloned())
                .ok_or_else(|| {
                    ProviderError::not_found(format!("unknown resource type: {fs_plural}"))
                })
        })
    }

    pub(crate) async fn list_types_for_listing(
        &self,
        namespace: Option<&str>,
    ) -> Result<Vec<String>> {
        let namespaced = namespace.is_some();
        let types = self.list_types(namespaced).await?;
        if !self.cx.state(|state| state.hide_empty_types) {
            return Ok(types);
        }

        let mut names = Vec::new();
        let mut paths = Vec::new();
        self.cx.state(|state| {
            if let Some(discovery) = state.discovery.borrow().as_ref() {
                for plural in &types {
                    if let Some(resource) = discovery.get(plural) {
                        names.push(plural.clone());
                        paths.push(resource.collection_path(namespace));
                    }
                }
            }
        });

        let results = join_all(paths.iter().map(|path| self.collection_non_empty(path))).await;
        Ok(names
            .into_iter()
            .zip(results)
            .filter_map(|(name, result)| result.unwrap_or(true).then_some(name))
            .collect())
    }

    pub(crate) async fn list_names(&self, path: &str) -> Result<Vec<String>> {
        let list: ListResponse = self.get_json(path, &[]).await?;
        Ok(list
            .items
            .into_iter()
            .filter_map(|item| item.metadata.name)
            .collect())
    }

    pub(crate) async fn load_manifest(
        &self,
        fs_plural: &str,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Load<KubeManifest>> {
        let resource = self.resource(fs_plural).await?;
        if resource.namespaced != namespace.is_some() {
            return Ok(Load::NotFound);
        }
        let path = resource.object_path(namespace, name);
        let Some(bytes) = self.get_bytes_opt(&path, &[], ACCEPT_JSON).await? else {
            return Ok(Load::NotFound);
        };
        let manifest = KubeManifest::from_upstream_bytes(&bytes)?;
        let validator = manifest.resource_version().map(Validator::from);
        let canonical = Canonical {
            bytes: manifest.canonical_json_bytes()?,
            validator,
        };
        Ok(Load::fresh(manifest, canonical))
    }

    pub(crate) async fn events_text(
        &self,
        namespace: &str,
        kind: &str,
        name: &str,
        uid: Option<&str>,
    ) -> Result<String> {
        let field_selector = event_field_selector(namespace, kind, name, uid);
        let path = format!("/api/v1/namespaces/{namespace}/events");
        let list: EventList = self
            .get_json(&path, &[("fieldSelector", &field_selector)])
            .await?;
        Ok(list.render())
    }

    /// The container names of one pod, init containers first, in spec order.
    /// These are the leaf names under the pod's `logs/` directory.
    pub(crate) async fn pod_containers(&self, namespace: &str, name: &str) -> Result<Vec<String>> {
        let path = format!("/api/v1/namespaces/{namespace}/pods/{name}");
        let Some(bytes) = self.get_bytes_opt(&path, &[], ACCEPT_JSON).await? else {
            return Err(ProviderError::not_found(format!(
                "pod {name} not found in namespace {namespace}"
            )));
        };
        let pod: Value = serde_json::from_slice(&bytes)
            .map_err(|error| ProviderError::internal(format!("kubernetes: parse pod: {error}")))?;
        Ok(container_names(&pod))
    }

    async fn fetch_discovery(&self) -> Result<Discovery> {
        let mut discovery = Discovery::default();

        if let Ok(core) = self.get_json::<APIResourceList>("/api/v1", &[]).await {
            discovery.add_resources("/api/v1", "", &core.resources);
        }

        if let Ok(groups) = self.get_json::<APIGroupList>("/apis", &[]).await {
            for group in &groups.groups {
                for group_version in group_versions_preferred_first(group) {
                    let api_root = format!("/apis/{group_version}");
                    if let Ok(list) = self.get_json::<APIResourceList>(&api_root, &[]).await {
                        discovery.add_resources(&api_root, &group.name, &list.resources);
                    }
                }
            }
        }

        if discovery.by_plural.is_empty() {
            return Err(ProviderError::internal(
                "kubernetes discovery returned no readable resources; is the API endpoint reachable? \
                 (for `unix://` endpoints, is `kubectl proxy --unix-socket` running?)",
            ));
        }
        Ok(discovery)
    }

    async fn list_types(&self, namespaced: bool) -> Result<Vec<String>> {
        self.ensure_discovery().await?;
        Ok(self.cx.state(|state| {
            state
                .discovery
                .borrow()
                .as_ref()
                .map(|discovery| discovery.sorted_types(namespaced))
                .unwrap_or_default()
        }))
    }

    async fn collection_non_empty(&self, path: &str) -> Result<bool> {
        let list: ListResponse = self.get_json(path, &[("limit", "1")]).await?;
        Ok(!list.items.is_empty())
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        self.cx
            .endpoint(self.endpoint())
            .get(path)
            .header("Accept", ACCEPT_JSON)
            .query_pairs(query.iter().copied())
            .json()
            .await
    }

    async fn get_bytes_opt(
        &self,
        path: &str,
        query: &[(&str, &str)],
        accept: &str,
    ) -> Result<Option<Vec<u8>>> {
        let request = self
            .cx
            .endpoint(self.endpoint())
            .get(path)
            .header("Accept", accept)
            .query_pairs(query.iter().copied());
        match request.send_checked().await {
            Ok(response) => Ok(Some(response.body().to_vec())),
            Err(err) if err.kind() == ProviderErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
}

#[derive(Deserialize)]
struct ListResponse {
    #[serde(default)]
    items: Vec<ListItem>,
}

#[derive(Deserialize)]
struct ListItem {
    #[serde(default)]
    metadata: ListItemMeta,
}

#[derive(Deserialize, Default)]
struct ListItemMeta {
    name: Option<String>,
}

/// Initial window: how many trailing lines to seed before following, so a
/// first read of a long-lived pod does not pull its entire history.
const INITIAL_TAIL_LINES: u32 = 2000;

/// Offset-addressable, append-only reader for one pod container's log. Backs
/// the `Live`/`Ranged` `<container>.log` leaf so the host follow pump and a
/// `tail -f` see the log grow: each read at or past the buffered end
/// delta-fetches new lines (kubectl-style `Accept: */*`, `timestamps=true` for
/// a resume marker, `sinceTime` to bound the fetch) and appends them with the
/// embedded timestamp stripped.
pub(crate) struct PodLogReader {
    base: String,
    namespace: String,
    pod: String,
    container: String,
    buf: RefCell<LogBuf>,
}

#[derive(Default)]
struct LogBuf {
    /// Accumulated log bytes, timestamps stripped: what the file serves.
    content: Vec<u8>,
    /// The last raw `<ts> <line>` appended; the resume marker for the next
    /// fetch, matched verbatim so variable `RFC3339Nano` precision cannot desync.
    last_raw: Option<String>,
    started: bool,
}

impl LogBuf {
    /// Truncate the last raw log timestamp to whole seconds, the precision the
    /// apiserver honors for `sinceTime`.
    fn since_time(&self) -> Option<String> {
        let line = self.last_raw.as_deref()?;
        let timestamp = line
            .split_once(' ')
            .map_or(line, |(timestamp, _)| timestamp);
        let date_time = timestamp.get(..19)?;
        Some(format!("{date_time}Z"))
    }
}

impl PodLogReader {
    pub(crate) fn new(base: String, namespace: &str, pod: &str, container: &str) -> Self {
        Self {
            base,
            namespace: namespace.to_string(),
            pod: pod.to_string(),
            container: container.to_string(),
            buf: RefCell::new(LogBuf::default()),
        }
    }

    async fn fetch_delta(&self, cx: &Cx<()>) -> Result<()> {
        let (since, first) = {
            let buf = self.buf.borrow();
            (buf.since_time(), !buf.started)
        };
        let path = format!(
            "/api/v1/namespaces/{}/pods/{}/log",
            self.namespace, self.pod
        );
        let tail = INITIAL_TAIL_LINES.to_string();
        let mut query: Vec<(&str, &str)> =
            vec![("container", &self.container), ("timestamps", "true")];
        if first {
            query.push(("tailLines", &tail));
        }
        if let Some(since) = &since {
            query.push(("sinceTime", since));
        }
        let request = cx
            .endpoint(KubeEndpoint {
                base: self.base.clone(),
            })
            .get(path)
            .header("Accept", "*/*")
            .query_pairs(query.iter().copied());
        let response = request.send_checked().await?;
        self.append(response.body());
        Ok(())
    }

    fn append(&self, body: &[u8]) {
        let text = String::from_utf8_lossy(body);
        let mut buf = self.buf.borrow_mut();
        buf.started = true;
        // Resume after the marker if the over-fetched window still contains it;
        // otherwise (rotation, or first fetch) take the whole window.
        let start = match &buf.last_raw {
            Some(marker) => text
                .lines()
                .position(|line| line == marker)
                .map_or(0, |i| i + 1),
            None => 0,
        };
        for line in text.lines().skip(start) {
            let content = line.split_once(' ').map_or(line, |(_, rest)| rest);
            buf.content.extend_from_slice(content.as_bytes());
            buf.content.push(b'\n');
            buf.last_raw = Some(line.to_string());
        }
    }
}

impl RangeReader for PodLogReader {
    fn read_chunk<'a>(
        &'a self,
        cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            if offset >= self.buf.borrow().content.len() as u64 {
                self.fetch_delta(cx).await?;
            }
            let buf = self.buf.borrow();
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(buf.content.len());
            let end = start.saturating_add(length as usize).min(buf.content.len());
            Ok(FileChunk::new(
                buf.content[start..end].to_vec(),
                end >= buf.content.len(),
            ))
        })
    }
}

fn container_names(pod: &Value) -> Vec<String> {
    let mut names = Vec::new();
    for field in ["initContainers", "containers"] {
        if let Some(list) = pod
            .pointer(&format!("/spec/{field}"))
            .and_then(Value::as_array)
        {
            names.extend(
                list.iter()
                    .filter_map(|container| container.get("name").and_then(Value::as_str))
                    .map(str::to_string),
            );
        }
    }
    names
}

fn event_field_selector(namespace: &str, kind: &str, name: &str, uid: Option<&str>) -> String {
    let mut terms = vec![
        format!("involvedObject.name={name}"),
        format!("involvedObject.namespace={namespace}"),
    ];
    if !kind.is_empty() {
        terms.push(format!("involvedObject.kind={kind}"));
    }
    if let Some(uid) = uid.filter(|uid| !uid.is_empty()) {
        terms.push(format!("involvedObject.uid={uid}"));
    }
    terms.join(",")
}

#[derive(Deserialize)]
struct EventList {
    #[serde(default)]
    items: Vec<EventItem>,
}

impl EventList {
    fn render(self) -> String {
        if self.items.is_empty() {
            return "No events.\n".to_string();
        }
        let mut out = String::new();
        let _ = writeln!(out, "LAST SEEN\tCOUNT\tTYPE\tREASON\tMESSAGE");
        for event in self.items {
            let timestamp = event
                .last_timestamp
                .or(event.event_time)
                .unwrap_or_else(|| "-".to_string());
            let count = event.count.unwrap_or(1);
            let message = event.message.replace('\n', " ");
            let _ = writeln!(
                out,
                "{timestamp}\t{count}\t{}\t{}\t{message}",
                event.event_type, event.reason
            );
        }
        out
    }
}

#[derive(Deserialize)]
struct EventItem {
    #[serde(rename = "type", default)]
    event_type: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    count: Option<u64>,
    #[serde(rename = "lastTimestamp")]
    last_timestamp: Option<String>,
    #[serde(rename = "eventTime")]
    event_time: Option<String>,
}
