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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clean_manifest_strips_only_managed_fields() {
        let manifest = KubeManifest::from_upstream_bytes(
            serde_json::to_vec(&json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": "web",
                    "resourceVersion": "12",
                    "uid": "abc",
                    "managedFields": [{"manager": "kubectl"}],
                    "annotations": {
                        "kubectl.kubernetes.io/last-applied-configuration": "{...}",
                        "keep": "me"
                    }
                },
                "spec": {"containers": [{"name": "web"}]}
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
        assert_eq!(manifest.resource_version(), Some("12"));
        assert_eq!(manifest.uid(), Some("abc"));

        let canonical: Value = serde_json::from_slice(&manifest.canonical_json_bytes().unwrap())
            .expect("canonical json");
        let meta = canonical.get("metadata").unwrap();
        assert!(meta.get("managedFields").is_none());
        assert_eq!(
            meta.pointer("/annotations/kubectl.kubernetes.io~1last-applied-configuration"),
            Some(&Value::String("{...}".to_string()))
        );
        assert_eq!(meta.pointer("/annotations/keep"), Some(&json!("me")));
    }

    #[test]
    fn status_yaml_extracts_or_nulls() {
        let manifest =
            KubeManifest::from_upstream_bytes(br#"{"status":{"phase":"Running"}}"#).unwrap();
        let status = manifest.status_yaml().unwrap();
        assert_eq!(status.content().unwrap(), b"phase: Running\n");

        let manifest = KubeManifest::from_upstream_bytes(br#"{"spec":{}}"#).unwrap();
        let status = manifest.status_yaml().unwrap();
        assert_eq!(status.content().unwrap(), b"null\n");
    }
}
