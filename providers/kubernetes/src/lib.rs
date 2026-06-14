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

mod api;
mod objects;

use crate::api::{Discovery, KubeApi, PodLogReader, text_file};
use crate::objects::{ClusterResource, NamespacedResource};

#[derive(Clone)]
#[omnifs_sdk::config]
pub struct Config {
    /// API endpoint. A `unix://` socket served by `kubectl proxy --unix-socket`
    /// is the supported transport.
    #[serde(default = "default_endpoint")]
    endpoint: String,
    /// When true, type listings show only resource types with at least one
    /// current instance. Empty types remain directly navigable by lookup.
    #[serde(default)]
    hide_empty_types: bool,
}

fn default_endpoint() -> String {
    "unix:///run/omnifs/k8s.sock".to_string()
}

pub(crate) struct State {
    pub(crate) endpoint: HttpEndpoint,
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
    ns: Namespace,
    rtype: ResourceType,
    name: ResourceName,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ClusterResourceKey {
    rtype: ResourceType,
    name: ResourceName,
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

fn register_routes(r: &mut Router<State>) -> Result<()> {
    r.dir("/namespaces").handler(namespaces_dir)?;
    r.dir("/namespaces/{ns}").handler(ns_types_dir)?;
    r.dir("/namespaces/{ns}/{rtype}")
        .handler(ns_resources_dir)?;
    r.object::<NamespacedResource>("/namespaces/{ns}/{rtype}/{name}", |o| {
        o.representations("manifest", (Yaml,))?;
        o.file("status.yaml")
            .project(NamespacedResource::status_yaml)?;
        Ok(())
    })?;
    r.file("/namespaces/{ns}/{rtype}/{name}/events.txt")
        .handler(namespaced_events_txt)?;
    r.dir("/namespaces/{ns}/{rtype}/{name}/logs")
        .handler(pod_logs_dir)?;
    r.file("/namespaces/{ns}/{rtype}/{name}/logs/{logfile}")
        .handler(pod_log_read)?;

    r.dir("/cluster").handler(cluster_types_dir)?;
    r.dir("/cluster/{rtype}").handler(cluster_resources_dir)?;
    r.object::<ClusterResource>("/cluster/{rtype}/{name}", |o| {
        o.representations("manifest", (Yaml,))?;
        o.file("status.yaml")
            .project(ClusterResource::status_yaml)?;
        Ok(())
    })?;

    Ok(())
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

impl NamespacedResourceKey {
    async fn events(self, cx: Cx<State>) -> Result<FileProjection> {
        let api = KubeApi::new(&cx);
        let resource = api.resource(self.rtype.as_str()).await?;
        if !resource.namespaced {
            return Err(ProviderError::not_found(format!(
                "resource type {} is not namespaced",
                self.rtype.as_str()
            )));
        }
        let loaded = api
            .load_manifest(
                self.rtype.as_str(),
                Some(self.ns.as_str()),
                self.name.as_str(),
            )
            .await?;
        let Load::Fresh { value, .. } = loaded else {
            return Err(ProviderError::not_found(format!(
                "{} {} not found in namespace {}",
                self.rtype.as_str(),
                self.name.as_str(),
                self.ns.as_str()
            )));
        };
        let uid = NamespacedResource::new(value).uid().map(str::to_string);
        let text = api
            .events_text(
                self.ns.as_str(),
                resource.kind(),
                self.name.as_str(),
                uid.as_deref(),
            )
            .await?;
        Ok(text_file(text.into_bytes()))
    }
}

async fn namespaced_events_txt(
    cx: Cx<State>,
    key: NamespacedResourceKey,
) -> Result<FileProjection> {
    key.events(cx).await
}

async fn pod_logs_dir(cx: DirCx<State>, key: PodLogsKey) -> Result<DirProjection> {
    let containers = KubeApi::new(&cx)
        .pod_containers(key.ns.as_str(), key.name.as_str())
        .await?;
    Ok(DirProjection::exhaustive(
        containers.into_iter().map(|c| Entry::file(format!("{c}.log"))),
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
        .volatile()
        .build())
}

impl Key for NamespacedResourceKey {
    type Object = NamespacedResource;
    type State = State;

    async fn load(&self, cx: &Cx<State>, _since: Option<Validator>) -> Result<Load<Self::Object>> {
        match KubeApi::new(cx)
            .load_manifest(
                self.rtype.as_str(),
                Some(self.ns.as_str()),
                self.name.as_str(),
            )
            .await?
        {
            Load::Fresh { value, canonical } => Ok(Load::Fresh {
                value: NamespacedResource::new(value),
                canonical,
            }),
            Load::Unchanged => Ok(Load::Unchanged),
            Load::NotFound => Ok(Load::NotFound),
        }
    }
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

impl Key for ClusterResourceKey {
    type Object = ClusterResource;
    type State = State;

    async fn load(&self, cx: &Cx<State>, _since: Option<Validator>) -> Result<Load<Self::Object>> {
        match KubeApi::new(cx)
            .load_manifest(self.rtype.as_str(), None, self.name.as_str())
            .await?
        {
            Load::Fresh { value, canonical } => Ok(Load::Fresh {
                value: ClusterResource::new(value),
                canonical,
            }),
            Load::Unchanged => Ok(Load::Unchanged),
            Load::NotFound => Ok(Load::NotFound),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_seal_without_ambiguity() {
        let mut router = Router::<State>::new();
        register_routes(&mut router).expect("routes register");
        router.seal().expect("route set must seal without overlap");
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
