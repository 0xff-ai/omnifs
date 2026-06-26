#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

//! `omnifs-provider-kubernetes`: a read-only projected filesystem over a
//! Kubernetes cluster.
//!
//! One mount targets one cluster/context through `config.endpoint`. The
//! supported endpoint shape is a local `kubectl proxy --unix-socket` socket:
//! kubectl terminates TLS and injects the active-context credentials, so this
//! provider issues plain read-only HTTP over the socket and never handles a
//! Kubernetes token.
//!
//! ```text
//! /namespaces/<ns>/<type>/<name>/{manifest.json,manifest.yaml,status.yaml,events.txt}
//! /namespaces/<ns>/pods/<name>/logs/<container>.log
//! /cluster/<type>/<name>/{manifest.json,manifest.yaml,status.yaml}
//! ```

use core::fmt;
use core::str::FromStr;
use std::cell::RefCell;
use std::rc::Rc;

use omnifs_sdk::prelude::*;

mod api;
mod objects;

use crate::api::{Discovery, KubeApi, PodLogReader};
use crate::objects::{ClusterResource, NamespacedResource};

#[derive(Clone)]
#[omnifs_sdk::config]
pub struct Config {
    /// API endpoint. A `unix://` socket served by `kubectl proxy --unix-socket`
    /// is the supported transport.
    #[serde(default = "default_endpoint")]
    endpoint: omnifs_sdk::HostSocket,
    /// When true, type listings show only resource types with at least one
    /// current instance. Empty types remain directly navigable by lookup.
    #[serde(default)]
    hide_empty_types: bool,
}

fn default_endpoint() -> omnifs_sdk::HostSocket {
    omnifs_sdk::HostSocket("unix:///run/omnifs/k8s.sock".to_string())
}

pub(crate) struct State {
    /// The in-cluster API server base URL; re-parsed per request by the
    /// endpoint URL builder.
    pub(crate) endpoint: String,
    pub(crate) hide_empty_types: bool,
    pub(crate) discovery: Rc<RefCell<Option<Discovery>>>,
}

fn valid_segment(s: &str) -> bool {
    // Reject path traversal, separators, and URL metacharacters: a captured
    // segment is interpolated into the apiserver URL path, so `%`/`?`/`#` or a
    // control byte could otherwise smuggle a query or alter the request. `%` is
    // already forbidden by Kubernetes' own ValidatePathSegmentName, and `:`
    // (RBAC names) stays legal.
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s
            .bytes()
            .any(|b| b < 0x20 || matches!(b, b'%' | b'?' | b'#'))
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
                valid_segment(s).then(|| Self(s.to_string())).ok_or(())
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
    /// A resource type's filesystem plural.
    ResourceType
);
string_segment!(
    /// A resource object name.
    ResourceName
);
string_segment!(
    /// A `<container>.log` leaf name under a pod's `logs/` directory.
    LogFile
);

/// The `pods` resource type, used to gate the `logs/` subtree to pods only.
/// As a capture (not a literal route segment) it stays invisible in the
/// namespace type listing, so it neither advertises `pods` unconditionally nor
/// breaks the `hide_empty_types` contract; its parser rejects every other type.
#[derive(Clone, Debug)]
pub(crate) struct PodType;

impl FromStr for PodType {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        (s == "pods").then_some(Self).ok_or(())
    }
}

impl fmt::Display for PodType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("pods")
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
pub(crate) struct NamespacedResourceKey {
    pub(crate) ns: Namespace,
    pub(crate) rtype: ResourceType,
    pub(crate) name: ResourceName,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ClusterResourceKey {
    pub(crate) rtype: ResourceType,
    pub(crate) name: ResourceName,
}

#[omnifs_sdk::path_captures]
struct ClusterTypeKey {
    rtype: ResourceType,
}

#[omnifs_sdk::path_captures]
struct PodLogsKey {
    ns: Namespace,
    rtype: PodType,
    name: ResourceName,
}

#[omnifs_sdk::path_captures]
struct PodLogKey {
    ns: Namespace,
    rtype: PodType,
    name: ResourceName,
    logfile: LogFile,
}

#[omnifs_sdk::provider(
    id = "kubernetes",
    display_name = "Kubernetes",
    mount = "k8s",
    capabilities(
        unix_socket(
            dynamic,
            "Talk to the Kubernetes API server through a local `kubectl proxy --unix-socket` endpoint. kubectl terminates TLS and injects the active-context credentials, so the provider issues plain HTTP over the socket and never handles a token."
        ),
        memory_mb(
            256,
            "Leave room for API discovery across all groups plus large list and manifest payloads."
        ),
    )
)]
impl KubernetesProvider {
    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        r.dir("/namespaces").handler(namespaces_dir)?;
        r.dir("/namespaces/{ns}").handler(ns_types_dir)?;
        r.dir("/namespaces/{ns}/{rtype}")
            .handler(ns_resources_dir)?;
        r.object::<NamespacedResource>("/namespaces/{ns}/{rtype}/{name}", |o| {
            o.dynamic();
            o.file("manifest.json").canonical::<Json>()?;
            o.file("manifest.yaml").representation::<Yaml>()?;
            o.file("status.yaml")
                .derive(NamespacedResource::status_yaml)?;
            o.file("events.txt")
                .direct(NamespacedResource::events_txt)?;
            Ok(())
        })?;
        // Pod logs stay on raw routes: `logs/` is a dynamic directory
        // (enumerates containers from a pod fetch) and `logs/{logfile}` is a
        // live-ranged stream, both requiring the `{rtype}` == "pods" gate that
        // `PodType`'s parser enforces on capture.
        r.dir("/namespaces/{ns}/{rtype}/{name}/logs")
            .handler(pod_logs_dir)?;
        r.file("/namespaces/{ns}/{rtype}/{name}/logs/{logfile}")
            .ranged()
            .handler(pod_log_read)?;

        r.dir("/cluster").handler(cluster_types_dir)?;
        r.dir("/cluster/{rtype}").handler(cluster_resources_dir)?;
        r.object::<ClusterResource>("/cluster/{rtype}/{name}", |o| {
            o.dynamic();
            o.file("manifest.json").canonical::<Json>()?;
            o.file("manifest.yaml").representation::<Yaml>()?;
            o.file("status.yaml").derive(ClusterResource::status_yaml)?;
            Ok(())
        })?;

        Ok(State {
            endpoint: config.endpoint.into(),
            hide_empty_types: config.hide_empty_types,
            discovery: Rc::new(RefCell::new(None)),
        })
    }
}

fn empty_dir() -> DirProjection {
    DirProjection::exhaustive(core::iter::empty::<Entry>())
}

fn dir_listing(names: Vec<String>) -> DirProjection {
    DirProjection::exhaustive(names.into_iter().map(Entry::dir))
}

async fn namespaces_dir(cx: DirCx<State>) -> Result<DirProjection> {
    let api = KubeApi::new(&cx);
    Ok(dir_listing(api.list_names("/api/v1/namespaces").await?))
}

impl NamespaceKey {
    async fn types(self, cx: DirCx<State>) -> Result<DirProjection> {
        let api = KubeApi::new(&cx);
        Ok(dir_listing(
            api.list_types_for_listing(Some(self.ns.as_str())).await?,
        ))
    }
}

async fn ns_types_dir(cx: DirCx<State>, key: NamespaceKey) -> Result<DirProjection> {
    key.types(cx).await
}

impl NsTypeKey {
    async fn resources(self, cx: DirCx<State>) -> Result<DirProjection> {
        let api = KubeApi::new(&cx);
        let resource = api.resource(self.rtype.as_str()).await?;
        if !resource.namespaced {
            return Ok(empty_dir());
        }
        Ok(dir_listing(
            api.list_names(&resource.collection_path(Some(self.ns.as_str())))
                .await?,
        ))
    }
}

async fn ns_resources_dir(cx: DirCx<State>, key: NsTypeKey) -> Result<DirProjection> {
    key.resources(cx).await
}

async fn pod_logs_dir(cx: DirCx<State>, key: PodLogsKey) -> Result<DirProjection> {
    let containers = KubeApi::new(&cx)
        .pod_containers(key.ns.as_str(), key.name.as_str())
        .await?;
    Ok(DirProjection::exhaustive(
        containers
            .into_iter()
            .map(|c| Entry::file(format!("{c}.log"))),
    ))
}

async fn pod_log_read(cx: Cx<State>, key: PodLogKey) -> Result<FileProjection> {
    let Some(container) = key.logfile.as_str().strip_suffix(".log") else {
        return Err(ProviderError::not_found(
            "pod log files are named <container>.log",
        ));
    };
    let endpoint = cx.state(|state| state.endpoint.clone());
    let reader = PodLogReader::new(endpoint, key.ns.as_str(), key.name.as_str(), container);
    Ok(FileProjection::ranged(reader)
        .size(Size::Unknown)
        .live()
        .build())
}

async fn cluster_types_dir(cx: DirCx<State>) -> Result<DirProjection> {
    let api = KubeApi::new(&cx);
    Ok(dir_listing(api.list_types_for_listing(None).await?))
}

impl ClusterTypeKey {
    async fn resources(self, cx: DirCx<State>) -> Result<DirProjection> {
        let api = KubeApi::new(&cx);
        let resource = api.resource(self.rtype.as_str()).await?;
        if resource.namespaced {
            return Ok(empty_dir());
        }
        Ok(dir_listing(
            api.list_names(&resource.collection_path(None)).await?,
        ))
    }
}

async fn cluster_resources_dir(cx: DirCx<State>, key: ClusterTypeKey) -> Result<DirProjection> {
    key.resources(cx).await
}
