//! Kubernetes object canonicals and filesystem projections.

use omnifs_sdk::browse::FileContent;
use omnifs_sdk::prelude::*;
use omnifs_sdk::repr::{Representable, Yaml};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct KubeManifest(Value);

impl KubeManifest {
    pub(crate) fn from_upstream_bytes(bytes: &[u8]) -> Result<Self> {
        let value: Value = serde_json::from_slice(bytes).map_err(|error| {
            ProviderError::internal(format!("kubernetes: parse object: {error}"))
        })?;
        Ok(Self(clean_manifest(value)))
    }

    pub(crate) fn canonical_json_bytes(&self) -> Result<Vec<u8>> {
        json_bytes(&self.0)
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

    fn status_yaml(&self) -> Result<FileContent> {
        let status = self.0.get("status").cloned().unwrap_or(Value::Null);
        Ok(FileContent::new(yaml_bytes(&status)?)
            .with_content_type(ContentType::Custom("application/yaml")))
    }
}

#[omnifs_sdk::object(kind = "kubernetes.namespaced-resource", key = crate::NamespacedResourceKey)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct NamespacedResource(KubeManifest);

impl NamespacedResource {
    pub(crate) fn new(manifest: KubeManifest) -> Self {
        Self(manifest)
    }

    pub(crate) fn status_yaml(&self) -> Result<FileContent> {
        self.0.status_yaml()
    }

    pub(crate) fn uid(&self) -> Option<&str> {
        self.0.uid()
    }
}

impl Representable<Yaml> for NamespacedResource {
    fn represent(&self) -> Vec<u8> {
        self.0.manifest_yaml_bytes().unwrap_or_else(error_bytes)
    }
}

#[omnifs_sdk::object(kind = "kubernetes.cluster-resource", key = crate::ClusterResourceKey)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ClusterResource(KubeManifest);

impl ClusterResource {
    pub(crate) fn new(manifest: KubeManifest) -> Self {
        Self(manifest)
    }

    pub(crate) fn status_yaml(&self) -> Result<FileContent> {
        self.0.status_yaml()
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

fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| ProviderError::internal(format!("kubernetes: render json: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn yaml_bytes(value: &Value) -> Result<Vec<u8>> {
    serde_yaml::to_string(value)
        .map(String::into_bytes)
        .map_err(|error| ProviderError::internal(format!("kubernetes: render yaml: {error}")))
}

fn error_bytes(error: ProviderError) -> Vec<u8> {
    format!("kubernetes: render yaml: {error}\n").into_bytes()
}
