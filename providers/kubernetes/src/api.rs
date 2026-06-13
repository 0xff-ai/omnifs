//! Kubernetes API access, discovery cataloging, and related-view rendering.

use core::fmt::Write as _;

use hashbrown::HashMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
    APIGroup, APIGroupList, APIResource as K8sApiResource, APIResourceList,
};
use omnifs_sdk::http::{HttpEndpoint, ResponseExt};
use omnifs_sdk::prelude::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::State;
use crate::objects::KubeManifest;

const ACCEPT_JSON: &str = "application/json";

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

fn add_resources(
    discovery: &mut Discovery,
    api_root: &str,
    group: &str,
    resources: &[K8sApiResource],
) {
    for entry in resources {
        if !is_browsable(entry) || discovery.has_group_resource(group, &entry.name) {
            continue;
        }
        let resource = Resource {
            api_root: api_root.to_string(),
            plural: entry.name.clone(),
            kind: entry.kind.clone(),
            group: group.to_string(),
            namespaced: entry.namespaced,
        };
        let fs_name = if !group.is_empty() && discovery.get(&entry.name).is_some() {
            format!("{}.{}", entry.name, group)
        } else {
            entry.name.clone()
        };
        discovery.insert(fs_name, resource);
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
        .map(|version| version.group_version.clone())
        .unwrap_or_default();
    let mut versions = Vec::new();
    if !preferred.is_empty() {
        versions.push(preferred.clone());
    }
    versions.extend(
        group
            .versions
            .iter()
            .map(|version| version.group_version.clone())
            .filter(|version| !version.is_empty() && version != &preferred),
    );
    versions
}

/// Operation-scoped Kubernetes API adapter. It owns URL construction, HTTP
/// status policy, and JSON decoding while still lowering every request through
/// Omnifs host callouts.
pub(crate) struct KubeApi<'a> {
    cx: &'a Cx<State>,
    endpoint: HttpEndpoint,
}

impl<'a> KubeApi<'a> {
    pub(crate) fn new(cx: &'a Cx<State>) -> Self {
        Self {
            cx,
            endpoint: cx.state(|state| state.endpoint.clone()),
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

    pub(crate) async fn type_is(&self, fs_plural: &str, namespaced: bool) -> Result<bool> {
        self.ensure_discovery().await?;
        Ok(self.cx.state(|state| {
            state
                .discovery
                .borrow()
                .as_ref()
                .and_then(|discovery| discovery.get(fs_plural))
                .is_some_and(|resource| resource.namespaced == namespaced)
        }))
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

    pub(crate) async fn path_exists(&self, path: &str) -> Result<bool> {
        Ok(self.get_bytes_opt(path, &[], ACCEPT_JSON).await?.is_some())
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
        Ok(Load::Fresh {
            value: manifest,
            canonical,
        })
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
        Ok(render_event_list(list))
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

    /// The current log buffer for one container, read whole. The apiserver's
    /// content negotiation rejects `Accept: text/plain` on the `log`
    /// subresource even though it streams text, so send `*/*` like kubectl.
    pub(crate) async fn pod_log(
        &self,
        namespace: &str,
        name: &str,
        container: &str,
    ) -> Result<Vec<u8>> {
        let path = format!("/api/v1/namespaces/{namespace}/pods/{name}/log");
        self.get_bytes(&path, &[("container", container)], "*/*").await
    }

    async fn fetch_discovery(&self) -> Result<Discovery> {
        let mut discovery = Discovery::default();

        if let Ok(core) = self.get_json::<APIResourceList>("/api/v1", &[]).await {
            add_resources(&mut discovery, "/api/v1", "", &core.resources);
        }

        if let Ok(groups) = self.get_json::<APIGroupList>("/apis", &[]).await {
            for group in &groups.groups {
                for group_version in group_versions_preferred_first(group) {
                    let api_root = format!("/apis/{group_version}");
                    if let Ok(list) = self.get_json::<APIResourceList>(&api_root, &[]).await {
                        add_resources(&mut discovery, &api_root, &group.name, &list.resources);
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
        let bytes = self.get_bytes(path, query, ACCEPT_JSON).await?;
        serde_json::from_slice(&bytes)
            .map_err(|error| ProviderError::internal(format!("kubernetes: parse {path}: {error}")))
    }

    async fn get_bytes(&self, path: &str, query: &[(&str, &str)], accept: &str) -> Result<Vec<u8>> {
        let url = self.endpoint.build_url(path, query);
        let response = self
            .cx
            .http()
            .get(url)
            .header("Accept", accept)
            .send()
            .await?;
        Ok(response.error_for_status()?.into_body())
    }

    async fn get_bytes_opt(
        &self,
        path: &str,
        query: &[(&str, &str)],
        accept: &str,
    ) -> Result<Option<Vec<u8>>> {
        let url = self.endpoint.build_url(path, query);
        let response = self
            .cx
            .http()
            .get(url)
            .header("Accept", accept)
            .send()
            .await?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        Ok(Some(response.error_for_status()?.into_body()))
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

fn container_names(pod: &Value) -> Vec<String> {
    let mut names = Vec::new();
    for field in ["initContainers", "containers"] {
        if let Some(list) = pod.pointer(&format!("/spec/{field}")).and_then(Value::as_array) {
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

fn render_event_list(list: EventList) -> String {
    if list.items.is_empty() {
        return "No events.\n".to_string();
    }
    let mut out = String::new();
    let _ = writeln!(out, "LAST SEEN\tCOUNT\tTYPE\tREASON\tMESSAGE");
    for event in list.items {
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

pub(crate) fn text_file(bytes: Vec<u8>) -> FileProjection {
    FileProjection::body(bytes)
        .content_type(ContentType::Custom("text/plain"))
        .mutable()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn resource_list(body: &str) -> APIResourceList {
        serde_json::from_str(body).expect("parse APIResourceList")
    }

    const CORE_V1: &str = r#"{
      "kind": "APIResourceList",
      "groupVersion": "v1",
      "resources": [
        {"name":"bindings","singularName":"binding","namespaced":true,"kind":"Binding","verbs":["create"]},
        {"name":"pods","singularName":"pod","namespaced":true,"kind":"Pod","verbs":["get","list","watch"],"shortNames":["po"],"categories":["all"],"storageVersionHash":"abc"},
        {"name":"pods/log","singularName":"","namespaced":true,"kind":"Pod","verbs":["get"]},
        {"name":"pods/status","singularName":"","namespaced":true,"kind":"Pod","verbs":["get","patch","update"]},
        {"name":"nodes","singularName":"node","namespaced":false,"kind":"Node","verbs":["get","list"],"shortNames":["no"]},
        {"name":"namespaces","singularName":"namespace","namespaced":false,"kind":"Namespace","verbs":["get","list"]},
        {"name":"events","singularName":"event","namespaced":true,"kind":"Event","verbs":["get","list"]}
      ]
    }"#;

    #[test]
    fn discovery_classifies_scope_and_filters_unreadable_resources() {
        let mut discovery = Discovery::default();
        add_resources(
            &mut discovery,
            "/api/v1",
            "",
            &resource_list(CORE_V1).resources,
        );

        assert!(discovery.get("bindings").is_none());
        assert!(discovery.get("pods/log").is_none());
        assert!(discovery.get("pods/status").is_none());

        let pods = discovery.get("pods").expect("pods present");
        assert!(pods.namespaced);
        assert_eq!(
            pods.collection_path(Some("default")),
            "/api/v1/namespaces/default/pods"
        );
        assert_eq!(
            pods.object_path(Some("default"), "web"),
            "/api/v1/namespaces/default/pods/web"
        );

        let nodes = discovery.get("nodes").expect("nodes present");
        assert!(!nodes.namespaced);
        assert_eq!(nodes.collection_path(None), "/api/v1/nodes");
        assert_eq!(nodes.object_path(None, "n1"), "/api/v1/nodes/n1");

        let namespaced = discovery.sorted_types(true);
        let cluster = discovery.sorted_types(false);
        assert!(namespaced.contains(&"pods".to_string()));
        assert!(namespaced.contains(&"events".to_string()));
        assert!(cluster.contains(&"nodes".to_string()));
        assert!(cluster.contains(&"namespaces".to_string()));
        assert!(!namespaced.contains(&"nodes".to_string()));
        assert!(!cluster.contains(&"pods".to_string()));
    }

    #[test]
    fn group_resources_use_group_version_root() {
        let mut discovery = Discovery::default();
        let apps = r#"{"groupVersion":"apps/v1","resources":[
            {"name":"deployments","singularName":"deployment","namespaced":true,"kind":"Deployment","verbs":["get","list"]},
            {"name":"deployments/scale","singularName":"","namespaced":true,"kind":"Scale","verbs":["get"]}
        ]}"#;
        add_resources(
            &mut discovery,
            "/apis/apps/v1",
            "apps",
            &resource_list(apps).resources,
        );
        assert!(discovery.get("deployments/scale").is_none());
        assert_eq!(
            discovery
                .get("deployments")
                .expect("deployments present")
                .object_path(Some("default"), "web"),
            "/apis/apps/v1/namespaces/default/deployments/web"
        );
    }

    #[test]
    fn plural_collision_qualifies_by_group_and_core_keeps_bare_name() {
        let mut discovery = Discovery::default();
        add_resources(
            &mut discovery,
            "/api/v1",
            "",
            &resource_list(CORE_V1).resources,
        );
        let group = r#"{"groupVersion":"events.k8s.io/v1","resources":[
            {"name":"events","singularName":"event","namespaced":true,"kind":"Event","verbs":["get","list"]}
        ]}"#;
        add_resources(
            &mut discovery,
            "/apis/events.k8s.io/v1",
            "events.k8s.io",
            &resource_list(group).resources,
        );

        assert_eq!(
            discovery.get("events").unwrap().collection_path(Some("ns")),
            "/api/v1/namespaces/ns/events"
        );
        assert_eq!(
            discovery
                .get("events.events.k8s.io")
                .unwrap()
                .collection_path(Some("ns")),
            "/apis/events.k8s.io/v1/namespaces/ns/events"
        );
    }

    #[test]
    fn discovery_prefers_preferred_version_and_keeps_non_preferred_only_resources() {
        let mut discovery = Discovery::default();
        let v2 = r#"{"groupVersion":"example.io/v2","resources":[
            {"name":"bars","singularName":"bar","namespaced":true,"kind":"Bar","verbs":["get","list"]}
        ]}"#;
        let v1 = r#"{"groupVersion":"example.io/v1","resources":[
            {"name":"bars","singularName":"bar","namespaced":true,"kind":"Bar","verbs":["get","list"]},
            {"name":"legacies","singularName":"legacy","namespaced":true,"kind":"Legacy","verbs":["get","list"]}
        ]}"#;
        add_resources(
            &mut discovery,
            "/apis/example.io/v2",
            "example.io",
            &resource_list(v2).resources,
        );
        add_resources(
            &mut discovery,
            "/apis/example.io/v1",
            "example.io",
            &resource_list(v1).resources,
        );

        assert_eq!(
            discovery.get("bars").unwrap().collection_path(Some("ns")),
            "/apis/example.io/v2/namespaces/ns/bars"
        );
        assert_eq!(
            discovery
                .get("legacies")
                .unwrap()
                .collection_path(Some("ns")),
            "/apis/example.io/v1/namespaces/ns/legacies"
        );
        assert!(discovery.get("bars.example.io").is_none());
    }

    #[test]
    fn group_versions_preferred_first_orders_and_dedups() {
        let group: APIGroup = serde_json::from_str(
            r#"{"name":"example.io",
                "preferredVersion":{"groupVersion":"example.io/v2","version":"v2"},
                "versions":[
                    {"groupVersion":"example.io/v1","version":"v1"},
                    {"groupVersion":"example.io/v2","version":"v2"}
                ]}"#,
        )
        .unwrap();
        assert_eq!(
            group_versions_preferred_first(&group),
            vec!["example.io/v2".to_string(), "example.io/v1".to_string()]
        );
    }

    #[test]
    fn event_field_selector_matches_kubectl_fields() {
        assert_eq!(
            event_field_selector("default", "Pod", "web", Some("abc-123")),
            "involvedObject.name=web,involvedObject.namespace=default,\
             involvedObject.kind=Pod,involvedObject.uid=abc-123"
        );
        assert_eq!(
            event_field_selector("default", "Pod", "web", None),
            "involvedObject.name=web,involvedObject.namespace=default,involvedObject.kind=Pod"
        );
        assert_eq!(
            event_field_selector("ns", "Deployment", "app", Some("")),
            "involvedObject.name=app,involvedObject.namespace=ns,involvedObject.kind=Deployment"
        );
    }
}
