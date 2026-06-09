//! Kubernetes API access: endpoint resolution, discovery, and rendering.
//!
//! The provider reaches the API server through the configured `endpoint`,
//! built into callout URLs by [`HttpEndpoint`]. The recommended endpoint is a
//! local `kubectl proxy --unix-socket` socket: it rides the `unix:` callout
//! transport (the same one the Docker provider uses), and kubectl terminates
//! TLS and injects the active-context credentials, so this provider issues
//! plain HTTP and never handles a token. An `https://` endpoint works only for
//! API servers that accept unauthenticated reads over system-trust TLS (no
//! Authorization header is sent in v1).
//!
//! Resource types are not hard-coded: [`fetch_discovery`] walks `/api/v1` and
//! `/apis` once per instance and caches the catalog in [`crate::State`], so
//! CRDs surface exactly like built-in kinds.

use core::fmt::Write as _;

use hashbrown::HashMap;
use omnifs_sdk::http::{HttpEndpoint, ResponseExt};
use omnifs_sdk::prelude::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::State;

const ACCEPT_JSON: &str = "application/json";
/// For the pod log subresource. The apiserver streams logs as text but its
/// content negotiation validates `Accept` against the structured API media
/// types, so `Accept: text/plain` is rejected with `406 Not Acceptable`
/// (verified against a live server through `kubectl proxy`); `*/*` matches
/// what curl/kubectl effectively send.
const ACCEPT_ANY: &str = "*/*";

// ===========================================================================
// Discovery catalog
// ===========================================================================

/// One API resource discovered from the cluster's discovery documents.
#[derive(Clone, Debug)]
pub(crate) struct ApiResource {
    /// URL root for this resource's group/version: `/api/v1` or `/apis/<group>/<version>`.
    api_root: String,
    /// The resource's plural URL segment: `pods`, `deployments`, ...
    plural: String,
    /// The resource's `Kind` (`Pod`, `Deployment`); used for the event
    /// `involvedObject.kind` selector so events match this exact kind.
    kind: String,
    /// The API group (`""` for the core group), used to dedup the same
    /// resource discovered across multiple versions of one group.
    group: String,
    /// Whether the resource is namespaced (vs cluster-scoped).
    pub(crate) namespaced: bool,
}

impl ApiResource {
    /// Collection URL path, optionally scoped to a namespace.
    pub(crate) fn collection_path(&self, namespace: Option<&str>) -> String {
        match namespace {
            Some(ns) => format!("{}/namespaces/{}/{}", self.api_root, ns, self.plural),
            None => format!("{}/{}", self.api_root, self.plural),
        }
    }

    /// Single-object URL path.
    pub(crate) fn object_path(&self, namespace: Option<&str>, name: &str) -> String {
        format!("{}/{}", self.collection_path(namespace), name)
    }

    /// The resource's `Kind`, for the event `involvedObject.kind` selector.
    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }
}

/// The cluster's resource catalog, keyed by the filesystem-facing plural name.
/// Built once per provider instance. A plural that collides across groups is
/// disambiguated to `<plural>.<group>` (core/built-ins keep the bare name).
#[derive(Debug, Default)]
pub(crate) struct Discovery {
    by_plural: HashMap<String, ApiResource>,
}

impl Discovery {
    fn insert(&mut self, fs_name: String, resource: ApiResource) {
        self.by_plural.entry(fs_name).or_insert(resource);
    }

    fn get(&self, fs_plural: &str) -> Option<&ApiResource> {
        self.by_plural.get(fs_plural)
    }

    /// Has this exact `(group, plural)` resource already been recorded (from a
    /// higher-priority version of the same group)? Used to keep the preferred
    /// version when a resource is served in several versions of one group.
    fn has_group_resource(&self, group: &str, plural: &str) -> bool {
        self.by_plural
            .values()
            .any(|r| r.group == group && r.plural == plural)
    }

    fn sorted_types(&self, namespaced: bool) -> Vec<String> {
        let mut names: Vec<String> = self
            .by_plural
            .iter()
            .filter(|(_, r)| r.namespaced == namespaced)
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }
}

// ===========================================================================
// Discovery wire types
// ===========================================================================

#[derive(Deserialize)]
struct ApiResourceList {
    #[serde(default)]
    resources: Vec<ApiResourceEntry>,
}

#[derive(Deserialize)]
struct ApiResourceEntry {
    name: String,
    #[serde(default)]
    namespaced: bool,
    #[serde(default)]
    kind: String,
}

#[derive(Deserialize)]
struct ApiGroupList {
    #[serde(default)]
    groups: Vec<ApiGroup>,
}

#[derive(Deserialize)]
struct ApiGroup {
    #[serde(default)]
    name: String,
    #[serde(rename = "preferredVersion", default)]
    preferred_version: GroupVersion,
    #[serde(default)]
    versions: Vec<GroupVersion>,
}

#[derive(Deserialize, Default)]
struct GroupVersion {
    #[serde(rename = "groupVersion", default)]
    group_version: String,
}

fn add_resources(
    disc: &mut Discovery,
    api_root: &str,
    group: &str,
    resources: &[ApiResourceEntry],
) {
    for entry in resources {
        // Names with a `/` are subresources (`pods/log`, `deployments/scale`),
        // not browsable resource types.
        if entry.name.contains('/') {
            continue;
        }
        // Already recorded from a higher-priority (preferred) version of the
        // same group — keep that one (client-go `ServerPreferredResources`).
        if disc.has_group_resource(group, &entry.name) {
            continue;
        }
        let resource = ApiResource {
            api_root: api_root.to_string(),
            plural: entry.name.clone(),
            kind: entry.kind.clone(),
            group: group.to_string(),
            namespaced: entry.namespaced,
        };
        // The bare plural is taken by a *different* group → disambiguate as
        // `<plural>.<group>` (matching kubectl's fully-qualified form).
        let fs_name = if !group.is_empty() && disc.get(&entry.name).is_some() {
            format!("{}.{}", entry.name, group)
        } else {
            entry.name.clone()
        };
        disc.insert(fs_name, resource);
    }
}

/// A group's `groupVersion`s with the preferred one first. `add_resources`
/// keeps the first occurrence, so a resource served in several versions
/// resolves to the preferred version (matching client-go
/// `ServerPreferredResources`), while a resource present only in a
/// non-preferred version still surfaces (at its own version).
fn group_versions_preferred_first(group: &ApiGroup) -> Vec<String> {
    let preferred = group.preferred_version.group_version.clone();
    let mut versions = Vec::new();
    if !preferred.is_empty() {
        versions.push(preferred.clone());
    }
    for version in &group.versions {
        if !version.group_version.is_empty() && version.group_version != preferred {
            versions.push(version.group_version.clone());
        }
    }
    versions
}

/// Walk discovery: core (`/api/v1`) plus every named group. Each group's
/// versions are folded preferred-first so a multi-version resource resolves to
/// its preferred version, yet resources present only in a non-preferred version
/// still surface (matching client-go `ServerPreferredResources`). A *group
/// version* whose discovery call fails (e.g. a flaky aggregated API) is skipped
/// rather than failing the whole catalog, but failures of the two root
/// documents (`/api/v1`, `/apis`) propagate: a transient error there must not
/// be cached as a half-empty catalog (which would also misassign bare plural
/// names that core normally claims). All per-group-version requests run in one
/// batched callout round; the fold order stays deterministic (core first, then
/// groups in server priority order).
async fn fetch_discovery(cx: &Cx<State>, ep: &HttpEndpoint) -> Result<Discovery> {
    let groups = get_json::<ApiGroupList>(cx, ep, "/apis").await?;
    let group_roots: Vec<(String, String)> = groups
        .groups
        .iter()
        .flat_map(|group| {
            group_versions_preferred_first(group)
                .into_iter()
                .map(|gv| (group.name.clone(), format!("/apis/{gv}")))
        })
        .collect();

    let mut fetches = vec![get_json::<ApiResourceList>(cx, ep, "/api/v1")];
    fetches.extend(
        group_roots
            .iter()
            .map(|(_, api_root)| get_json::<ApiResourceList>(cx, ep, api_root)),
    );
    let mut results = join_all(fetches).await;

    let mut disc = Discovery::default();
    let core = results.remove(0)?;
    add_resources(&mut disc, "/api/v1", "", &core.resources);
    for ((group, api_root), result) in group_roots.iter().zip(results) {
        if let Ok(list) = result {
            add_resources(&mut disc, api_root, group, &list.resources);
        }
    }

    if disc.by_plural.is_empty() {
        return Err(ProviderError::internal(
            "kubernetes discovery returned no resources; is the API endpoint reachable? \
             (for `unix://` endpoints, is `kubectl proxy --unix-socket` running?)",
        ));
    }
    Ok(disc)
}

/// Populate the per-instance discovery cache on first use. No-op afterwards.
pub(crate) async fn ensure_discovery(cx: &Cx<State>) -> Result<()> {
    if cx.state(|s| s.discovery.is_some()) {
        return Ok(());
    }
    let ep = cx.state(|s| s.endpoint.clone());
    let disc = fetch_discovery(cx, &ep).await?;
    cx.state_mut(|s| {
        s.discovery = Some(disc);
    });
    Ok(())
}

/// Resolve a filesystem plural to its API resource, populating discovery first.
pub(crate) async fn resolve_type(cx: &Cx<State>, fs_plural: &str) -> Result<ApiResource> {
    ensure_discovery(cx).await?;
    cx.state(|s| {
        s.discovery
            .as_ref()
            .and_then(|d| d.get(fs_plural).cloned())
            .ok_or_else(|| ProviderError::not_found(format!("unknown resource type: {fs_plural}")))
    })
}

/// Sorted filesystem plurals for the requested scope.
pub(crate) async fn list_types(cx: &Cx<State>, namespaced: bool) -> Result<Vec<String>> {
    ensure_discovery(cx).await?;
    Ok(cx.state(|s| {
        s.discovery
            .as_ref()
            .map(|d| d.sorted_types(namespaced))
            .unwrap_or_default()
    }))
}

/// Resource types to show in a scope listing. The full discovery catalog by
/// default; when `hide_empty_types` is set, only types with at least one
/// instance in `namespace` (`None` = cluster scope). Existence is probed with
/// `limit=1`, and all probes for the listing run in a single batched round.
/// This filters `readdir` only — `lookup` still resolves any known type, so an
/// empty type stays directly navigable.
pub(crate) async fn list_types_for_listing(
    cx: &Cx<State>,
    ep: &HttpEndpoint,
    namespace: Option<&str>,
) -> Result<Vec<String>> {
    let types = list_types(cx, namespace.is_some()).await?;
    if !cx.state(|s| s.hide_empty_types) {
        return Ok(types);
    }

    // Resolve each type's collection path up front (cheap map lookups), then
    // probe them all concurrently.
    let mut names = Vec::new();
    let mut paths = Vec::new();
    cx.state(|s| {
        if let Some(disc) = s.discovery.as_ref() {
            for plural in &types {
                if let Some(resource) = disc.get(plural) {
                    names.push(plural.clone());
                    paths.push(resource.collection_path(namespace));
                }
            }
        }
    });
    let results = join_all(paths.iter().map(|p| collection_non_empty(cx, ep, p))).await;

    let mut kept = Vec::new();
    for (name, result) in names.into_iter().zip(results) {
        // Keep on probe error: never hide a type we couldn't confirm is empty.
        if result.unwrap_or(true) {
            kept.push(name);
        }
    }
    Ok(kept)
}

/// Does the collection at `path` have any items? Uses `limit=1` so the probe
/// stays tiny regardless of collection size.
async fn collection_non_empty(cx: &Cx<State>, ep: &HttpEndpoint, path: &str) -> Result<bool> {
    let bytes = get_bytes(cx, ep, path, &[("limit", "1")], ACCEPT_JSON).await?;
    let list: ListResponse = serde_json::from_slice(&bytes)
        .map_err(|e| ProviderError::internal(format!("kubernetes: parse list {path}: {e}")))?;
    Ok(!list.items.is_empty())
}

// ===========================================================================
// HTTP helpers
// ===========================================================================

pub(crate) fn endpoint(cx: &Cx<State>) -> HttpEndpoint {
    cx.state(|s| s.endpoint.clone())
}

async fn get_bytes(
    cx: &Cx<State>,
    ep: &HttpEndpoint,
    path: &str,
    query: &[(&str, &str)],
    accept: &str,
) -> Result<Vec<u8>> {
    let url = ep.build_url(path, query);
    let response = cx.http().get(url).header("Accept", accept).send().await?;
    let response = response.error_for_status()?;
    Ok(response.into_body())
}

/// Like [`get_bytes`] but maps `404 Not Found` to `Ok(None)` so callers can do
/// existence checks without treating absence as an error.
pub(crate) async fn get_bytes_opt(
    cx: &Cx<State>,
    ep: &HttpEndpoint,
    path: &str,
    query: &[(&str, &str)],
    accept: &str,
) -> Result<Option<Vec<u8>>> {
    let url = ep.build_url(path, query);
    let response = cx.http().get(url).header("Accept", accept).send().await?;
    if response.status().as_u16() == 404 {
        return Ok(None);
    }
    let response = response.error_for_status()?;
    Ok(Some(response.into_body()))
}

async fn get_json<T: DeserializeOwned>(cx: &Cx<State>, ep: &HttpEndpoint, path: &str) -> Result<T> {
    let bytes = get_bytes(cx, ep, path, &[], ACCEPT_JSON).await?;
    serde_json::from_slice(&bytes)
        .map_err(|e| ProviderError::internal(format!("kubernetes: parse {path}: {e}")))
}

#[derive(Deserialize)]
struct ListResponse {
    #[serde(default)]
    items: Vec<ListItem>,
}

#[derive(Deserialize)]
struct ListItem {
    #[serde(default)]
    metadata: ItemMeta,
}

#[derive(Deserialize, Default)]
struct ItemMeta {
    #[serde(default)]
    name: String,
}

/// `.items[].metadata.name` for a collection `GET`, dropping unnamed entries.
pub(crate) async fn list_names(
    cx: &Cx<State>,
    ep: &HttpEndpoint,
    path: &str,
) -> Result<Vec<String>> {
    let list: ListResponse = get_json(cx, ep, path).await?;
    Ok(list
        .items
        .into_iter()
        .map(|i| i.metadata.name)
        .filter(|name| !name.is_empty())
        .collect())
}

/// Fetch one object as JSON, resolving its type through discovery.
pub(crate) async fn fetch_object(
    cx: &Cx<State>,
    fs_plural: &str,
    namespace: Option<&str>,
    name: &str,
) -> Result<Value> {
    let resource = resolve_type(cx, fs_plural).await?;
    let ep = endpoint(cx);
    let path = resource.object_path(namespace, name);
    let bytes = get_bytes(cx, &ep, &path, &[], ACCEPT_JSON).await?;
    serde_json::from_slice(&bytes)
        .map_err(|e| ProviderError::internal(format!("kubernetes: parse object {path}: {e}")))
}

/// Container names declared by a pod manifest (regular, init, and ephemeral).
pub(crate) fn pod_container_names(pod: &Value) -> Vec<String> {
    let mut names = Vec::new();
    for field in ["containers", "initContainers", "ephemeralContainers"] {
        if let Some(items) = pod
            .pointer(&format!("/spec/{field}"))
            .and_then(Value::as_array)
        {
            for container in items {
                if let Some(name) = container.get("name").and_then(Value::as_str) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// Current logs for one container of a pod.
pub(crate) async fn pod_log(
    cx: &Cx<State>,
    ep: &HttpEndpoint,
    namespace: &str,
    pod: &str,
    container: &str,
) -> Result<Vec<u8>> {
    let path = format!("/api/v1/namespaces/{namespace}/pods/{pod}/log");
    get_bytes(cx, ep, &path, &[("container", container)], ACCEPT_ANY).await
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
    /// New-style (events.k8s.io) recurring events carry their occurrence
    /// count/time here and leave the deprecated `count`/`lastTimestamp` unset.
    #[serde(default)]
    series: Option<EventSeries>,
}

#[derive(Deserialize)]
struct EventSeries {
    #[serde(default)]
    count: Option<u64>,
    #[serde(rename = "lastObservedTime")]
    last_observed_time: Option<String>,
}

/// Tab-separated events for a specific object, matching the involvedObject
/// field selector kubectl builds (`event_expansion.go` `GetFieldSelector`):
/// name + namespace + kind, plus uid when known. Without kind, events from a
/// different-kind object of the same name would leak in; uid additionally
/// excludes events from a prior incarnation of a recreated object.
pub(crate) async fn events_text(
    cx: &Cx<State>,
    ep: &HttpEndpoint,
    namespace: &str,
    kind: &str,
    name: &str,
    uid: Option<&str>,
) -> Result<String> {
    let field_selector = event_field_selector(namespace, kind, name, uid);
    let path = format!("/api/v1/namespaces/{namespace}/events");
    let bytes = get_bytes(
        cx,
        ep,
        &path,
        &[("fieldSelector", &field_selector)],
        ACCEPT_JSON,
    )
    .await?;
    let list: EventList = serde_json::from_slice(&bytes)
        .map_err(|e| ProviderError::internal(format!("kubernetes: parse events: {e}")))?;
    Ok(render_event_list(list))
}

/// Build the comma-separated (logical-AND) involvedObject field selector
/// kubectl uses. Pure so the selector contract is unit-tested directly.
fn event_field_selector(namespace: &str, kind: &str, name: &str, uid: Option<&str>) -> String {
    let mut terms = vec![
        format!("involvedObject.name={}", escape_selector_value(name)),
        format!(
            "involvedObject.namespace={}",
            escape_selector_value(namespace)
        ),
    ];
    if !kind.is_empty() {
        terms.push(format!(
            "involvedObject.kind={}",
            escape_selector_value(kind)
        ));
    }
    if let Some(uid) = uid.filter(|u| !u.is_empty()) {
        terms.push(format!("involvedObject.uid={}", escape_selector_value(uid)));
    }
    terms.join(",")
}

/// Escape a field selector value the way `fields.EscapeValue` does (`\` `,`
/// `=` are the selector syntax characters). Path-segment-named kinds (RBAC)
/// may legally contain `,`/`=`; without escaping the server rejects the whole
/// selector.
fn escape_selector_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | ',' | '=') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Render a parsed event list as a tab-separated table. Pure (no I/O) so the
/// formatting is unit-tested directly. Following kubectl's event printer:
/// a `series` (new-style recurring event) wins for count/last-seen, then the
/// deprecated `count`/`lastTimestamp`, then `eventTime`; `count` defaults to 1;
/// embedded newlines in messages are flattened so one event stays one line.
fn render_event_list(list: EventList) -> String {
    if list.items.is_empty() {
        return "No events.\n".to_string();
    }
    let mut out = String::new();
    let _ = writeln!(out, "LAST SEEN\tCOUNT\tTYPE\tREASON\tMESSAGE");
    for event in list.items {
        let series = event.series.as_ref();
        let timestamp = series
            .and_then(|s| s.last_observed_time.clone())
            .or(event.last_timestamp)
            .or(event.event_time)
            .unwrap_or_else(|| "-".to_string());
        let count = series.and_then(|s| s.count).or(event.count).unwrap_or(1);
        let message = event.message.replace('\n', " ");
        let _ = writeln!(
            out,
            "{timestamp}\t{count}\t{}\t{}\t{message}",
            event.event_type, event.reason
        );
    }
    out
}

// ===========================================================================
// Rendering
// ===========================================================================

/// Strip server-managed `metadata.managedFields` so a manifest reads like
/// `kubectl get -o yaml`, which omits managedFields by default (since v1.21, via
/// cli-runtime's print flags). Everything else — including the
/// `last-applied-configuration` annotation, which `kubectl get` preserves — is
/// kept verbatim.
pub(crate) fn clean_manifest(mut value: Value) -> Value {
    if let Some(metadata) = value.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.remove("managedFields");
    }
    value
}

/// The `.status` subobject, or null if absent.
pub(crate) fn status_of(value: &Value) -> Value {
    value.get("status").cloned().unwrap_or(Value::Null)
}

pub(crate) const YAML: ContentType = ContentType::Custom("application/yaml");
pub(crate) const TEXT: ContentType = ContentType::Custom("text/plain");

pub(crate) fn yaml_bytes(value: &Value) -> Result<Vec<u8>> {
    serde_yaml::to_string(value)
        .map(String::into_bytes)
        .map_err(|e| ProviderError::internal(format!("kubernetes: render yaml: {e}")))
}

pub(crate) fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| ProviderError::internal(format!("kubernetes: render json: {e}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// The full-fidelity projection for the leaf actually being read.
pub(crate) fn body_file(bytes: Vec<u8>, content_type: ContentType) -> FileProjection {
    FileProjection::body(bytes)
        .content_type(content_type)
        .mutable()
        .build()
}

/// A capped inline projection for preloading a sibling leaf derived from the
/// same upstream fetch ("project all data you have already fetched"). Returns
/// `None` when the rendering exceeds the inline preload cap; the sibling is
/// then simply served by its own handler on demand.
pub(crate) fn inline_sibling(bytes: Vec<u8>, content_type: ContentType) -> Option<FileProjection> {
    (bytes.len() <= MAX_PROJECTED_BYTES).then(|| {
        FileProjection::inline(bytes)
            .content_type(content_type)
            .mutable()
            .build()
    })
}

pub(crate) fn text_bytes(bytes: Vec<u8>) -> FileProjection {
    body_file(bytes, TEXT)
}

#[cfg(test)]
mod tests {
    //! Wire-handling correctness against realistic Kubernetes payloads.
    //!
    //! These mirror the surface kubectl's own discovery/RESTMapper tests cover
    //! (scope classification, subresource filtering, group-qualified resource
    //! names) and are version-robust: the fixtures carry the full set of fields
    //! a real server sends so we also prove forward-compatibility (unknown
    //! fields are ignored) and tolerance of omitted optionals.

    use super::*;
    use serde_json::json;

    fn resource_list(body: &str) -> ApiResourceList {
        serde_json::from_str(body).expect("parse APIResourceList")
    }

    /// A realistic core (`/api/v1`) discovery payload: namespaced + cluster
    /// kinds, subresources, and the extra fields (`verbs`, `shortNames`,
    /// `categories`, `storageVersionHash`, `singularName`) a real server emits.
    const CORE_V1: &str = r#"{
      "kind": "APIResourceList",
      "groupVersion": "v1",
      "resources": [
        {"name":"pods","singularName":"pod","namespaced":true,"kind":"Pod","verbs":["get","list","watch"],"shortNames":["po"],"categories":["all"],"storageVersionHash":"abc"},
        {"name":"pods/log","singularName":"","namespaced":true,"kind":"Pod","verbs":["get"]},
        {"name":"pods/status","singularName":"","namespaced":true,"kind":"Pod","verbs":["get","patch","update"]},
        {"name":"nodes","singularName":"node","namespaced":false,"kind":"Node","verbs":["get","list"],"shortNames":["no"]},
        {"name":"namespaces","singularName":"namespace","namespaced":false,"kind":"Namespace","verbs":["get","list"]},
        {"name":"events","singularName":"event","namespaced":true,"kind":"Event","verbs":["get","list"]}
      ]
    }"#;

    #[test]
    fn discovery_classifies_scope_and_filters_subresources() {
        let mut disc = Discovery::default();
        add_resources(&mut disc, "/api/v1", "", &resource_list(CORE_V1).resources);

        // Subresources (names containing '/') are not browsable types.
        assert!(disc.get("pods/log").is_none());
        assert!(disc.get("pods/status").is_none());

        let pods = disc.get("pods").expect("pods present");
        assert!(pods.namespaced);
        assert_eq!(
            pods.collection_path(Some("default")),
            "/api/v1/namespaces/default/pods"
        );
        assert_eq!(
            pods.object_path(Some("default"), "web"),
            "/api/v1/namespaces/default/pods/web"
        );

        let nodes = disc.get("nodes").expect("nodes present");
        assert!(!nodes.namespaced);
        assert_eq!(nodes.collection_path(None), "/api/v1/nodes");
        assert_eq!(nodes.object_path(None, "n1"), "/api/v1/nodes/n1");

        // Scope partitioning is exactly what `/namespaces` vs `/cluster` list.
        let namespaced = disc.sorted_types(true);
        let cluster = disc.sorted_types(false);
        assert!(namespaced.contains(&"pods".to_string()));
        assert!(namespaced.contains(&"events".to_string()));
        assert!(cluster.contains(&"nodes".to_string()));
        assert!(cluster.contains(&"namespaces".to_string()));
        assert!(!namespaced.contains(&"nodes".to_string()));
        // Listings are sorted.
        let mut expected = namespaced.clone();
        expected.sort();
        assert_eq!(namespaced, expected);
    }

    #[test]
    fn group_resources_use_group_version_root() {
        let mut disc = Discovery::default();
        let apps = r#"{"groupVersion":"apps/v1","resources":[
            {"name":"deployments","namespaced":true,"kind":"Deployment","verbs":["get"]},
            {"name":"deployments/scale","namespaced":true,"kind":"Scale","verbs":["get"]}
        ]}"#;
        add_resources(
            &mut disc,
            "/apis/apps/v1",
            "apps",
            &resource_list(apps).resources,
        );
        assert!(disc.get("deployments/scale").is_none());
        assert_eq!(
            disc.get("deployments")
                .expect("deployments present")
                .object_path(Some("default"), "web"),
            "/apis/apps/v1/namespaces/default/deployments/web"
        );
    }

    #[test]
    fn plural_collision_qualifies_by_group_and_core_keeps_bare_name() {
        let mut disc = Discovery::default();
        // Core processed first, so it keeps the bare `events` name.
        add_resources(&mut disc, "/api/v1", "", &resource_list(CORE_V1).resources);
        let group = r#"{"groupVersion":"events.k8s.io/v1","resources":[
            {"name":"events","namespaced":true,"kind":"Event","verbs":["get","list"]}
        ]}"#;
        add_resources(
            &mut disc,
            "/apis/events.k8s.io/v1",
            "events.k8s.io",
            &resource_list(group).resources,
        );

        assert_eq!(
            disc.get("events").unwrap().collection_path(Some("ns")),
            "/api/v1/namespaces/ns/events"
        );
        assert_eq!(
            disc.get("events.events.k8s.io")
                .unwrap()
                .collection_path(Some("ns")),
            "/apis/events.k8s.io/v1/namespaces/ns/events"
        );
    }

    #[test]
    fn api_resource_entry_defaults_namespaced_when_omitted() {
        // Robust against servers/versions that might omit `namespaced`.
        let list = resource_list(r#"{"resources":[{"name":"widgets","kind":"Widget"}]}"#);
        let mut disc = Discovery::default();
        add_resources(
            &mut disc,
            "/apis/example.io/v1",
            "example.io",
            &list.resources,
        );
        assert!(!disc.get("widgets").unwrap().namespaced);
    }

    #[test]
    fn clean_manifest_strips_only_managed_fields() {
        // `kubectl get -o yaml` omits managedFields by default (since v1.21) but
        // PRESERVES the last-applied-configuration annotation; match that.
        let cleaned = clean_manifest(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "web",
                "namespace": "default",
                "managedFields": [{"manager": "kubectl"}],
                "annotations": {
                    "kubectl.kubernetes.io/last-applied-configuration": "{...}",
                    "keep": "me"
                }
            },
            "spec": {"containers": [{"name": "web"}]}
        }));
        let meta = cleaned.get("metadata").unwrap();
        assert!(meta.get("managedFields").is_none());
        let annotations = meta.get("annotations").unwrap();
        // last-applied-configuration is kept, exactly as `kubectl get` shows it.
        assert_eq!(
            annotations.get("kubectl.kubernetes.io/last-applied-configuration"),
            Some(&Value::String("{...}".to_string()))
        );
        assert_eq!(annotations.get("keep").unwrap(), "me");
        assert_eq!(meta.get("name").unwrap(), "web");
        assert!(cleaned.get("spec").is_some());
    }

    #[test]
    fn event_field_selector_matches_kubectl_fields() {
        // kubectl's GetFieldSelector builds name+namespace+kind(+uid), comma-joined.
        assert_eq!(
            event_field_selector("default", "Pod", "web", Some("abc-123")),
            "involvedObject.name=web,involvedObject.namespace=default,\
             involvedObject.kind=Pod,involvedObject.uid=abc-123"
        );
        // uid omitted when unknown/empty; kind omitted only if unknown.
        assert_eq!(
            event_field_selector("default", "Pod", "web", None),
            "involvedObject.name=web,involvedObject.namespace=default,involvedObject.kind=Pod"
        );
        assert_eq!(
            event_field_selector("ns", "Deployment", "app", Some("")),
            "involvedObject.name=app,involvedObject.namespace=ns,involvedObject.kind=Deployment"
        );
    }

    #[test]
    fn event_field_selector_escapes_selector_syntax_like_kubectl() {
        // fields.EscapeValue escapes `\` `,` `=`; RBAC path-segment names may
        // legally contain them and must not break (or smuggle) selector terms.
        assert_eq!(escape_selector_value(r"a=b,c\d"), r"a\=b\,c\\d");
        assert_eq!(
            event_field_selector("default", "Role", "edit=true,really", None),
            "involvedObject.name=edit\\=true\\,really,involvedObject.namespace=default,\
             involvedObject.kind=Role"
        );
    }

    #[test]
    fn discovery_prefers_preferred_version_and_keeps_non_preferred_only_resources() {
        // A group with two versions: v2 is preferred. `bars` exists in both;
        // `legacies` only in the non-preferred v1. fetch_discovery queries
        // preferred-first, so `bars` resolves to v2 while `legacies` still
        // surfaces (at v1) — matching client-go ServerPreferredResources.
        let mut disc = Discovery::default();
        let v2 = r#"{"groupVersion":"example.io/v2","resources":[
            {"name":"bars","namespaced":true,"kind":"Bar"}
        ]}"#;
        let v1 = r#"{"groupVersion":"example.io/v1","resources":[
            {"name":"bars","namespaced":true,"kind":"Bar"},
            {"name":"legacies","namespaced":true,"kind":"Legacy"}
        ]}"#;
        // preferred (v2) first, then v1
        add_resources(
            &mut disc,
            "/apis/example.io/v2",
            "example.io",
            &resource_list(v2).resources,
        );
        add_resources(
            &mut disc,
            "/apis/example.io/v1",
            "example.io",
            &resource_list(v1).resources,
        );

        assert_eq!(
            disc.get("bars").unwrap().collection_path(Some("ns")),
            "/apis/example.io/v2/namespaces/ns/bars",
            "multi-version resource resolves to the preferred version"
        );
        assert_eq!(
            disc.get("legacies").unwrap().collection_path(Some("ns")),
            "/apis/example.io/v1/namespaces/ns/legacies",
            "a resource only in a non-preferred version still surfaces"
        );
        // No spurious group-qualified duplicate from the second version.
        assert!(disc.get("bars.example.io").is_none());
    }

    #[test]
    fn group_versions_preferred_first_orders_and_dedups() {
        let group: ApiGroup = serde_json::from_str(
            r#"{"name":"example.io",
                "preferredVersion":{"groupVersion":"example.io/v2"},
                "versions":[{"groupVersion":"example.io/v1"},{"groupVersion":"example.io/v2"}]}"#,
        )
        .unwrap();
        assert_eq!(
            group_versions_preferred_first(&group),
            vec!["example.io/v2".to_string(), "example.io/v1".to_string()]
        );
    }

    #[test]
    fn clean_manifest_tolerates_unexpected_shapes() {
        // Must never panic on shapes a CRD or malformed object might present.
        let _ = clean_manifest(json!({"kind": "X"}));
        let _ = clean_manifest(json!({"metadata": "not-an-object"}));
        let _ = clean_manifest(json!({"metadata": {"annotations": "nope"}}));
        let _ = clean_manifest(json!(42));
    }

    #[test]
    fn status_of_extracts_or_nulls() {
        assert_eq!(
            status_of(&json!({"status": {"phase": "Running"}})),
            json!({"phase": "Running"})
        );
        assert_eq!(status_of(&json!({"spec": {}})), Value::Null);
    }

    #[test]
    fn pod_container_names_covers_all_container_kinds_in_order() {
        let pod = json!({"spec": {
            "initContainers": [{"name": "init"}],
            "containers": [{"name": "app"}, {"name": "sidecar"}],
            "ephemeralContainers": [{"name": "debug"}]
        }});
        // Order: containers, then initContainers, then ephemeralContainers.
        assert_eq!(
            pod_container_names(&pod),
            vec![
                "app".to_string(),
                "sidecar".to_string(),
                "init".to_string(),
                "debug".to_string()
            ]
        );
        // No spec / unexpected shape -> empty, no panic.
        assert!(pod_container_names(&json!({"kind": "Pod"})).is_empty());
        assert!(pod_container_names(&json!({"spec": {"containers": "nope"}})).is_empty());
    }

    #[test]
    fn render_event_list_formats_rows_and_handles_empty() {
        let empty: EventList = serde_json::from_str(r#"{"items":[]}"#).unwrap();
        assert_eq!(render_event_list(empty), "No events.\n");

        // Carries the full real-server field set (involvedObject, source, etc.)
        // to prove unknown fields are ignored.
        let list: EventList = serde_json::from_str(
            r#"{"apiVersion":"v1","kind":"EventList","items":[
                {"involvedObject":{"kind":"Pod","name":"web","namespace":"default"},
                 "type":"Warning","reason":"BackOff","message":"Back-off restarting\nfailed container",
                 "count":5,"source":{"component":"kubelet"},
                 "firstTimestamp":"2024-01-01T00:00:00Z","lastTimestamp":"2024-01-01T00:05:00Z",
                 "reportingComponent":"kubelet"},
                {"type":"Normal","reason":"Scheduled","message":"assigned","eventTime":"2024-01-02T00:00:00Z"}
            ]}"#,
        )
        .unwrap();
        let out = render_event_list(list);
        assert!(out.starts_with("LAST SEEN\tCOUNT\tTYPE\tREASON\tMESSAGE\n"));
        // lastTimestamp preferred; embedded newline flattened to a space.
        assert!(out.contains(
            "2024-01-01T00:05:00Z\t5\tWarning\tBackOff\tBack-off restarting failed container\n"
        ));
        // eventTime fallback when lastTimestamp absent; count defaults to 1.
        assert!(out.contains("2024-01-02T00:00:00Z\t1\tNormal\tScheduled\tassigned\n"));
    }

    #[test]
    fn render_event_list_prefers_series_for_recurring_new_style_events() {
        // events.k8s.io recurring events carry series{count,lastObservedTime}
        // and leave the deprecated count/lastTimestamp unset; kubectl's printer
        // reads the series. Without this an hourly-repeating event shows as
        // COUNT 1 at its first occurrence time.
        let list: EventList = serde_json::from_str(
            r#"{"items":[
                {"type":"Warning","reason":"FailedScheduling","message":"0/3 nodes available",
                 "eventTime":"2024-01-01T00:00:00Z",
                 "series":{"count":17,"lastObservedTime":"2024-01-01T04:00:00Z"}}
            ]}"#,
        )
        .unwrap();
        assert!(render_event_list(list).contains(
            "2024-01-01T04:00:00Z\t17\tWarning\tFailedScheduling\t0/3 nodes available\n"
        ));
    }
}
