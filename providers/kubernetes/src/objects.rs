//! Kubernetes object canonicals and filesystem projections.

use omnifs_sdk::prelude::*;
use omnifs_sdk::repr::{Representable, Yaml};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::State;
use crate::api::KubeApi;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct KubeManifest(Value);

impl KubeManifest {
    pub(crate) fn from_upstream_bytes(bytes: &[u8]) -> Result<Self> {
        let value: Value = decode_json(bytes)?;
        Ok(Self(clean_manifest(value)))
    }

    pub(crate) fn canonical_json_bytes(&self) -> Result<Vec<u8>> {
        pretty_json(&self.0)
    }

    pub(crate) fn resource_version(&self) -> Option<&str> {
        self.0
            .pointer("/metadata/resourceVersion")
            .and_then(Value::as_str)
    }

    pub(crate) fn uid(&self) -> Option<&str> {
        self.0.pointer("/metadata/uid").and_then(Value::as_str)
    }

    fn manifest_yaml_bytes(&self) -> Result<Vec<u8>> {
        yaml_bytes(&self.0)
    }

    pub(crate) fn status_yaml_bytes(&self) -> Result<Vec<u8>> {
        let status = self.0.get("status").cloned().unwrap_or(Value::Null);
        yaml_bytes(&status)
    }
}

/// Namespaced Kubernetes resource (e.g. pods, deployments).
///
/// Canonical = JSON (cleaned manifest); decode round-trips via `serde_json`.
#[omnifs_sdk::object(kind = "kubernetes.namespaced-resource", key = crate::NamespacedResourceKey)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct NamespacedResource(KubeManifest);

impl NamespacedResource {
    pub(crate) fn new(manifest: KubeManifest) -> Self {
        Self(manifest)
    }

    /// Computed: `status.yaml` leaf from the manifest's status stanza.
    pub(crate) fn status_yaml(
        &self,
        _key: &crate::NamespacedResourceKey,
    ) -> Result<FileProjection> {
        Ok(FileProjection::inline(self.0.status_yaml_bytes()?)
            .content_type(ContentType::Custom("application/yaml"))
            .build())
    }

    /// Direct face: `events.txt` — requires two callouts (manifest fetch for UID,
    /// then events fetch). Invoked on every read; not cached as canonical.
    pub(crate) async fn events_txt(
        cx: Cx<State>,
        key: crate::NamespacedResourceKey,
    ) -> Result<FileProjection> {
        let api = KubeApi::new(&cx);
        let resource = api.resource(key.rtype.as_str()).await?;
        if !resource.namespaced {
            return Err(ProviderError::not_found(format!(
                "resource type {} is not namespaced",
                key.rtype.as_str()
            )));
        }
        let loaded = api
            .load_manifest(key.rtype.as_str(), Some(key.ns.as_str()), key.name.as_str())
            .await?;
        let Load::Fresh { value, .. } = loaded else {
            return Err(ProviderError::not_found(format!(
                "{} {} not found in namespace {}",
                key.rtype.as_str(),
                key.name.as_str(),
                key.ns.as_str()
            )));
        };
        let uid = value.uid().map(str::to_string);
        let text = api
            .events_text(
                key.ns.as_str(),
                resource.kind(),
                key.name.as_str(),
                uid.as_deref(),
            )
            .await?;
        Ok(FileProjection::dynamic_body_with_type(
            text.into_bytes(),
            ContentType::Text,
        ))
    }

    /// Inherent load forwarded to by the `#[object]` macro's `Object::load`.
    pub(crate) async fn load(
        cx: &Cx<State>,
        key: &crate::NamespacedResourceKey,
        _since: Option<Validator>,
    ) -> Result<Load<Self>> {
        match KubeApi::new(cx)
            .load_manifest(key.rtype.as_str(), Some(key.ns.as_str()), key.name.as_str())
            .await?
        {
            Load::Fresh {
                value, canonical, ..
            } => Ok(Load::fresh(NamespacedResource::new(value), canonical)),
            Load::Unchanged => Ok(Load::Unchanged),
            Load::NotFound => Ok(Load::NotFound),
        }
    }
}

impl Representable<Yaml> for NamespacedResource {
    fn represent(&self) -> Vec<u8> {
        self.0.manifest_yaml_bytes().unwrap_or_else(error_bytes)
    }
}

/// Cluster-scoped Kubernetes resource (e.g. nodes, namespaces,
/// clusterrolebindings).
///
/// Canonical = JSON (cleaned manifest); decode round-trips via `serde_json`.
#[omnifs_sdk::object(kind = "kubernetes.cluster-resource", key = crate::ClusterResourceKey)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ClusterResource(KubeManifest);

impl ClusterResource {
    pub(crate) fn new(manifest: KubeManifest) -> Self {
        Self(manifest)
    }

    /// Computed: `status.yaml` leaf from the manifest's status stanza.
    pub(crate) fn status_yaml(&self, _key: &crate::ClusterResourceKey) -> Result<FileProjection> {
        Ok(FileProjection::inline(self.0.status_yaml_bytes()?)
            .content_type(ContentType::Custom("application/yaml"))
            .build())
    }

    /// Inherent load forwarded to by the `#[object]` macro's `Object::load`.
    pub(crate) async fn load(
        cx: &Cx<State>,
        key: &crate::ClusterResourceKey,
        _since: Option<Validator>,
    ) -> Result<Load<Self>> {
        match KubeApi::new(cx)
            .load_manifest(key.rtype.as_str(), None, key.name.as_str())
            .await?
        {
            Load::Fresh {
                value, canonical, ..
            } => Ok(Load::fresh(ClusterResource::new(value), canonical)),
            Load::Unchanged => Ok(Load::Unchanged),
            Load::NotFound => Ok(Load::NotFound),
        }
    }
}

impl Representable<Yaml> for ClusterResource {
    fn represent(&self) -> Vec<u8> {
        self.0.manifest_yaml_bytes().unwrap_or_else(error_bytes)
    }
}

fn clean_manifest(mut value: Value) -> Value {
    if let Some(metadata) = value.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.remove("managedFields");
    }
    value
}

fn yaml_bytes(value: &Value) -> Result<Vec<u8>> {
    serde_yaml::to_string(value)
        .map(String::into_bytes)
        .map_err(|error| ProviderError::internal(format!("kubernetes: render yaml: {error}")))
}

fn error_bytes(error: ProviderError) -> Vec<u8> {
    format!("kubernetes: render yaml: {error}\n").into_bytes()
}
