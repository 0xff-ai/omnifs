#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

//! `omnifs-provider-kubernetes`: a read-only projected filesystem over a
//! Kubernetes cluster.
//!
//! One mount targets one cluster/context, fixed by the mount config's
//! `endpoint` (the FS does not change when you `kubectl config use-context`;
//! browse another cluster by adding another mount). The recommended endpoint
//! is a local `kubectl proxy --unix-socket`, which terminates TLS and injects
//! the active-context credentials — so this provider never handles a token and
//! works against any cluster `kubectl` can reach (mTLS, EKS/GKE exec plugins,
//! OIDC, custom CA all handled upstream by kubectl).
//!
//! Layout (resource-as-directory; types incl. CRDs come from live discovery):
//!
//! ```text
//! /namespaces/<ns>/<type>/<name>/{manifest.yaml,manifest.json,status.yaml,events.txt}
//! /namespaces/<ns>/pods/<name>/logs/<container>.log
//! /cluster/<type>/<name>/{manifest.yaml,manifest.json,status.yaml}
//! ```

use core::fmt;
use core::str::FromStr;
use std::cell::RefCell;
use std::rc::Rc;

use omnifs_sdk::http::HttpEndpoint;
use omnifs_sdk::prelude::*;
use serde_json::Value;

mod api;

use crate::api::{
    Discovery, clean_manifest, endpoint, events_text, fetch_object, get_bytes_opt, list_names,
    list_types_for_listing, path_exists, pod_container_names, pod_log, render_json, render_yaml,
    resolve_type, status_of, text_bytes, text_file, type_is,
};

/// Core `v1` plural for pods — the only type that gets a `logs/` subtree.
const POD_PLURAL: &str = "pods";

// ===========================================================================
// Config & state
// ===========================================================================

#[derive(Clone)]
#[omnifs_sdk::config]
pub struct Config {
    /// API endpoint. A `unix://` socket served by `kubectl proxy --unix-socket`
    /// (recommended), or an `https://` API server reachable with system-trust
    /// TLS. The host grants this socket automatically from the endpoint.
    #[serde(default = "default_endpoint")]
    endpoint: String,
    /// When true, listing a namespace (or `/cluster`) shows only resource types
    /// that currently have at least one instance, instead of the full discovery
    /// catalog. Costs one batched `limit=1` probe per type per listing; empty
    /// types stay directly navigable (lookup is unaffected). Default false.
    #[serde(default)]
    hide_empty_types: bool,
}

fn default_endpoint() -> String {
    "unix:///run/omnifs/k8s.sock".to_string()
}

/// Per-instance state: the resolved endpoint plus a lazily-populated discovery
/// cache (filled on first browse, since discovery requires async callouts that
/// cannot run during synchronous `start`).
pub(crate) struct State {
    pub(crate) endpoint: HttpEndpoint,
    pub(crate) hide_empty_types: bool,
    pub(crate) discovery: Rc<RefCell<Option<Discovery>>>,
}

// ===========================================================================
// Path-segment capture types
// ===========================================================================

/// Charset gate for a path segment. Existence is proven by the API on lookup,
/// so this only rejects clearly-invalid segments and lets the catalog/API be
/// the authoritative name oracle.
fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains('/') && !s.contains('\0')
}

macro_rules! string_segment {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = ();

            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                if valid_segment(s) {
                    Ok(Self(s.to_string()))
                } else {
                    Err(())
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_segment!(
    /// A Kubernetes namespace name.
    Namespace
);
string_segment!(
    /// A resource type's filesystem plural (`pods`, `deployments`, or a
    /// group-qualified `<plural>.<group>` for collisions).
    ResourceType
);
string_segment!(
    /// A resource object name.
    ResourceName
);

/// A pod log filename: `<container>.log`.
#[derive(Clone, Debug)]
pub(crate) struct LogFile(String);

impl LogFile {
    /// The container name (the filename without its `.log` suffix).
    fn container(&self) -> &str {
        self.0.strip_suffix(".log").unwrap_or(&self.0)
    }
}

impl FromStr for LogFile {
    type Err = ();

    // We mint these filenames ourselves as lowercase `<container>.log`, so an
    // exact suffix match is the intended contract.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if valid_segment(s) && s.len() > 4 && s.ends_with(".log") {
            Ok(Self(s.to_string()))
        } else {
            Err(())
        }
    }
}

impl fmt::Display for LogFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[omnifs_sdk::path_captures]
struct NamespaceKey {
    ns: Namespace,
}

#[omnifs_sdk::path_captures]
struct NsTypeKey {
    ns: Namespace,
    rtype: ResourceType,
}

#[omnifs_sdk::path_captures]
struct NsResourceKey {
    ns: Namespace,
    rtype: ResourceType,
    name: ResourceName,
}

#[omnifs_sdk::path_captures]
struct PodLogsKey {
    ns: Namespace,
    name: ResourceName,
}

#[omnifs_sdk::path_captures]
struct PodLogKey {
    ns: Namespace,
    name: ResourceName,
    logfile: LogFile,
}

#[omnifs_sdk::path_captures]
struct ClusterTypeKey {
    rtype: ResourceType,
}

#[omnifs_sdk::path_captures]
struct ClusterResourceKey {
    rtype: ResourceType,
    name: ResourceName,
}

// ===========================================================================
// Provider
// ===========================================================================

#[omnifs_sdk::provider(metadata = "omnifs.provider.json")]
impl KubernetesProvider {
    type Config = Config;
    type State = State;

    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        register_routes(r)?;
        Ok(State {
            endpoint: HttpEndpoint::parse(&config.endpoint),
            hide_empty_types: config.hide_empty_types,
            discovery: Rc::new(RefCell::new(None)),
        })
    }
}

/// Register the full route tree. Factored out of `start` so the route set can
/// be sealed in a host-runnable test (route ambiguity is otherwise only caught
/// at runtime, by the generated `router.seal()` on first use).
fn register_routes(r: &mut Router<State>) -> Result<()> {
    // Namespaced browse: /namespaces/<ns>/<type>/<name>/...
    r.dir("/namespaces").handler(namespaces_dir)?;
    r.dir("/namespaces/{ns}").handler(ns_types_dir)?;
    r.dir("/namespaces/{ns}/{rtype}")
        .handler(ns_resources_dir)?;
    r.dir("/namespaces/{ns}/{rtype}/{name}")
        .handler(ns_resource_dir)?;
    r.file("/namespaces/{ns}/{rtype}/{name}/manifest.yaml")
        .handler(ns_manifest_yaml)?;
    r.file("/namespaces/{ns}/{rtype}/{name}/manifest.json")
        .handler(ns_manifest_json)?;
    r.file("/namespaces/{ns}/{rtype}/{name}/status.yaml")
        .handler(ns_status_yaml)?;
    r.file("/namespaces/{ns}/{rtype}/{name}/events.txt")
        .handler(ns_events_txt)?;

    // Pod logs live under the literal `pods` type so only pods grow a `logs/`
    // subtree; other namespaced types share the leaf set above.
    r.dir("/namespaces/{ns}/pods/{name}/logs")
        .handler(pod_logs_dir)?;
    r.file("/namespaces/{ns}/pods/{name}/logs/{logfile}")
        .handler(pod_log_file)?;

    // Cluster-scoped browse: /cluster/<type>/<name>/...
    r.dir("/cluster").handler(cluster_types_dir)?;
    r.dir("/cluster/{rtype}").handler(cluster_resources_dir)?;
    r.dir("/cluster/{rtype}/{name}")
        .handler(cluster_resource_dir)?;
    r.file("/cluster/{rtype}/{name}/manifest.yaml")
        .handler(cluster_manifest_yaml)?;
    r.file("/cluster/{rtype}/{name}/manifest.json")
        .handler(cluster_manifest_json)?;
    r.file("/cluster/{rtype}/{name}/status.yaml")
        .handler(cluster_status_yaml)?;

    Ok(())
}

// ===========================================================================
// Listing helpers
// ===========================================================================

fn empty_dir() -> DirProjection {
    DirProjection::exhaustive(core::iter::empty::<Entry>())
}

fn dir_listing(names: Vec<String>) -> DirProjection {
    DirProjection::exhaustive(names.into_iter().map(Entry::dir))
}

/// A single-entry (or empty) listing for a `Lookup` existence check.
fn lookup_dir(exists: bool, name: &str) -> DirProjection {
    if exists {
        DirProjection::exhaustive([Entry::dir(name)])
    } else {
        empty_dir()
    }
}

// ===========================================================================
// Namespaced handlers
// ===========================================================================

async fn namespaces_dir(cx: DirCx<State>) -> Result<DirProjection> {
    let ep = endpoint(&cx);
    if let DirIntent::Lookup { child } = cx.intent() {
        let exists = path_exists(&cx, &ep, &format!("/api/v1/namespaces/{child}")).await?;
        return Ok(lookup_dir(exists, child));
    }
    Ok(dir_listing(
        list_names(&cx, &ep, "/api/v1/namespaces").await?,
    ))
}

async fn ns_types_dir(cx: DirCx<State>, key: NamespaceKey) -> Result<DirProjection> {
    if let DirIntent::Lookup { child } = cx.intent() {
        return Ok(lookup_dir(type_is(&cx, child, true).await?, child));
    }
    let ep = endpoint(&cx);
    Ok(dir_listing(
        list_types_for_listing(&cx, &ep, Some(key.ns.as_str())).await?,
    ))
}

async fn ns_resources_dir(cx: DirCx<State>, key: NsTypeKey) -> Result<DirProjection> {
    let resource = resolve_type(&cx, key.rtype.as_str()).await?;
    if !resource.namespaced {
        return Ok(empty_dir());
    }
    let ep = endpoint(&cx);
    if let DirIntent::Lookup { child } = cx.intent() {
        let path = resource.object_path(Some(key.ns.as_str()), child);
        return Ok(lookup_dir(path_exists(&cx, &ep, &path).await?, child));
    }
    let path = resource.collection_path(Some(key.ns.as_str()));
    Ok(dir_listing(list_names(&cx, &ep, &path).await?))
}

async fn ns_resource_dir(_cx: DirCx<State>, key: NsResourceKey) -> Result<DirProjection> {
    let mut entries = vec![
        Entry::file("manifest.yaml"),
        Entry::file("manifest.json"),
        Entry::file("status.yaml"),
        Entry::file("events.txt"),
    ];
    if key.rtype.as_str() == POD_PLURAL {
        entries.push(Entry::dir("logs"));
    }
    Ok(DirProjection::exhaustive(entries))
}

async fn ns_manifest_yaml(cx: Cx<State>, key: NsResourceKey) -> Result<FileProjection> {
    let value = fetch_object(
        &cx,
        key.rtype.as_str(),
        Some(key.ns.as_str()),
        key.name.as_str(),
    )
    .await?;
    render_yaml(&clean_manifest(value))
}

async fn ns_manifest_json(cx: Cx<State>, key: NsResourceKey) -> Result<FileProjection> {
    let value = fetch_object(
        &cx,
        key.rtype.as_str(),
        Some(key.ns.as_str()),
        key.name.as_str(),
    )
    .await?;
    render_json(&clean_manifest(value))
}

async fn ns_status_yaml(cx: Cx<State>, key: NsResourceKey) -> Result<FileProjection> {
    let value = fetch_object(
        &cx,
        key.rtype.as_str(),
        Some(key.ns.as_str()),
        key.name.as_str(),
    )
    .await?;
    render_yaml(&status_of(&value))
}

async fn ns_events_txt(cx: Cx<State>, key: NsResourceKey) -> Result<FileProjection> {
    let ep = endpoint(&cx);
    // Match kubectl's event search: filter by the object's kind (from discovery)
    // and uid (from the object) as well as name/namespace, so events of a
    // same-named object of another kind — or a prior incarnation — don't leak in.
    let resource = resolve_type(&cx, key.rtype.as_str()).await?;
    let object = fetch_object(
        &cx,
        key.rtype.as_str(),
        Some(key.ns.as_str()),
        key.name.as_str(),
    )
    .await?;
    let uid = object.pointer("/metadata/uid").and_then(Value::as_str);
    let text = events_text(
        &cx,
        &ep,
        key.ns.as_str(),
        resource.kind(),
        key.name.as_str(),
        uid,
    )
    .await?;
    Ok(text_file(text))
}

// ===========================================================================
// Pod logs
// ===========================================================================

async fn pod_logs_dir(cx: DirCx<State>, key: PodLogsKey) -> Result<DirProjection> {
    let ep = endpoint(&cx);
    let path = format!(
        "/api/v1/namespaces/{}/pods/{}",
        key.ns.as_str(),
        key.name.as_str()
    );
    let Some(bytes) = get_bytes_opt(&cx, &ep, &path, &[], "application/json").await? else {
        return Ok(empty_dir());
    };
    let pod: Value = serde_json::from_slice(&bytes)
        .map_err(|e| ProviderError::internal(format!("kubernetes: parse pod: {e}")))?;
    let entries = pod_container_names(&pod)
        .into_iter()
        .map(|container| Entry::file(format!("{container}.log")));
    Ok(DirProjection::exhaustive(entries))
}

async fn pod_log_file(cx: Cx<State>, key: PodLogKey) -> Result<FileProjection> {
    let ep = endpoint(&cx);
    let bytes = pod_log(
        &cx,
        &ep,
        key.ns.as_str(),
        key.name.as_str(),
        key.logfile.container(),
    )
    .await?;
    Ok(text_bytes(bytes))
}

// ===========================================================================
// Cluster-scoped handlers
// ===========================================================================

async fn cluster_types_dir(cx: DirCx<State>) -> Result<DirProjection> {
    if let DirIntent::Lookup { child } = cx.intent() {
        return Ok(lookup_dir(type_is(&cx, child, false).await?, child));
    }
    let ep = endpoint(&cx);
    Ok(dir_listing(list_types_for_listing(&cx, &ep, None).await?))
}

async fn cluster_resources_dir(cx: DirCx<State>, key: ClusterTypeKey) -> Result<DirProjection> {
    let resource = resolve_type(&cx, key.rtype.as_str()).await?;
    if resource.namespaced {
        return Ok(empty_dir());
    }
    let ep = endpoint(&cx);
    if let DirIntent::Lookup { child } = cx.intent() {
        let path = resource.object_path(None, child);
        return Ok(lookup_dir(path_exists(&cx, &ep, &path).await?, child));
    }
    Ok(dir_listing(
        list_names(&cx, &ep, &resource.collection_path(None)).await?,
    ))
}

async fn cluster_resource_dir(
    _cx: DirCx<State>,
    _key: ClusterResourceKey,
) -> Result<DirProjection> {
    Ok(DirProjection::exhaustive([
        Entry::file("manifest.yaml"),
        Entry::file("manifest.json"),
        Entry::file("status.yaml"),
    ]))
}

async fn cluster_manifest_yaml(cx: Cx<State>, key: ClusterResourceKey) -> Result<FileProjection> {
    let value = fetch_object(&cx, key.rtype.as_str(), None, key.name.as_str()).await?;
    render_yaml(&clean_manifest(value))
}

async fn cluster_manifest_json(cx: Cx<State>, key: ClusterResourceKey) -> Result<FileProjection> {
    let value = fetch_object(&cx, key.rtype.as_str(), None, key.name.as_str()).await?;
    render_json(&clean_manifest(value))
}

async fn cluster_status_yaml(cx: Cx<State>, key: ClusterResourceKey) -> Result<FileProjection> {
    let value = fetch_object(&cx, key.rtype.as_str(), None, key.name.as_str()).await?;
    render_yaml(&status_of(&value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole route set must seal without overlap. The generated provider
    /// calls `router.seal()` at first use, so an ambiguous route would fail the
    /// provider at runtime rather than at compile time; this guards it. The
    /// literal `pods` logs routes coexisting with the `{rtype}` capture is the
    /// case most at risk.
    #[test]
    fn routes_seal_without_ambiguity() {
        let mut router = Router::<State>::new();
        register_routes(&mut router).expect("routes register");
        router.seal().expect("route set must seal without overlap");
    }

    #[test]
    fn log_file_parses_container_stem() {
        assert_eq!("web.log".parse::<LogFile>().unwrap().container(), "web");
        assert_eq!(
            "istio-proxy.log".parse::<LogFile>().unwrap().container(),
            "istio-proxy"
        );
        assert!("web".parse::<LogFile>().is_err()); // missing .log
        assert!(".log".parse::<LogFile>().is_err()); // empty container
        assert!("a/b.log".parse::<LogFile>().is_err()); // path separator
    }

    #[test]
    fn segment_validation_rejects_traversal_and_separators() {
        assert!("default".parse::<Namespace>().is_ok());
        assert!("cert-manager".parse::<ResourceType>().is_ok());
        assert!(
            "certificates.cert-manager.io"
                .parse::<ResourceType>()
                .is_ok()
        );
        assert!("".parse::<Namespace>().is_err());
        assert!(".".parse::<ResourceName>().is_err());
        assert!("..".parse::<ResourceName>().is_err());
        assert!("a/b".parse::<ResourceName>().is_err());
    }
}
