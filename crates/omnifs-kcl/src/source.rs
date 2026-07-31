use omnifs_api::{
    AttachmentDefinition, CredentialDefinition, MountResourceDefinition, ProviderDefinition,
    ResourceDeclarations, ResourceDefinition,
};
use omnifs_core::{
    AttachmentProtocol, AttachmentRuntime, AttachmentSpec, ProviderId, ResourceName,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use thiserror::Error;

/// Strict KCL authoring root. It is an in-memory interchange type only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthoringConfig {
    pub api_version: String,
    pub resources: Vec<AuthoringResource>,
}

/// A resource before client-only provider source resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "spec",
    rename_all = "PascalCase",
    deny_unknown_fields
)]
pub enum AuthoringResource {
    Provider(ProviderAuthoring),
    Credential(CredentialDefinition),
    Mount(MountResourceDefinition),
    Attachment(AttachmentAuthoring),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderAuthoring {
    pub name: ResourceName,
    pub source: ProviderSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentAuthoring {
    pub name: ResourceName,
    pub protocol: AttachmentProtocol,
    pub runtime: AttachmentRuntime,
    pub location: PathBuf,
    pub docker_image: Option<String>,
    pub libkrun_guest_image: Option<String>,
}

/// Client-only source selector. Paths and selectors never cross the daemon
/// boundary; they must be resolved to a content-addressed provider id first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ProviderSource {
    Embedded { embedded: String },
    Local { local: LocalProviderSource },
    Digest { digest: ProviderId },
}

impl<'de> Deserialize<'de> for ProviderSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("provider source must be an object"))?;
        if object.len() != 1 {
            return Err(serde::de::Error::custom(
                "provider source must contain exactly one of `embedded`, `local`, or `digest`",
            ));
        }
        let (kind, value) = object
            .iter()
            .next()
            .expect("checked that provider source has exactly one field");
        match kind.as_str() {
            "embedded" => {
                let embedded = serde_json::from_value::<String>(value.clone())
                    .map_err(serde::de::Error::custom)?;
                if embedded.is_empty() {
                    return Err(serde::de::Error::custom(
                        "embedded provider source must not be empty",
                    ));
                }
                Ok(Self::Embedded { embedded })
            },
            "local" => serde_json::from_value::<LocalProviderSource>(value.clone())
                .map(|local| Self::Local { local })
                .map_err(serde::de::Error::custom),
            "digest" => serde_json::from_value::<ProviderId>(value.clone())
                .map(|digest| Self::Digest { digest })
                .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(
                "provider source must contain one of `embedded`, `local`, or `digest`",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalProviderSource {
    pub path: PathBuf,
    pub expected_digest: ProviderId,
}

impl AuthoringConfig {
    /// Convert authoring declarations after every provider source has been
    /// resolved to an exact artifact digest.
    pub fn into_declarations(
        self,
        resolved: &BTreeMap<ResourceName, ProviderId>,
    ) -> Result<ResourceDeclarations, SourceResolutionError> {
        let resources = self
            .resources
            .into_iter()
            .map(|resource| match resource {
                AuthoringResource::Provider(provider) => {
                    let artifact = match provider.source {
                        ProviderSource::Digest { digest } => digest,
                        ProviderSource::Embedded { .. } | ProviderSource::Local { .. } => {
                            resolved.get(&provider.name).copied().ok_or_else(|| {
                                SourceResolutionError::Unresolved(provider.name.clone())
                            })?
                        },
                    };
                    Ok(ResourceDefinition::Provider(ProviderDefinition {
                        name: provider.name,
                        artifact,
                    }))
                },
                AuthoringResource::Credential(value) => Ok(ResourceDefinition::Credential(value)),
                AuthoringResource::Mount(value) => Ok(ResourceDefinition::Mount(value)),
                AuthoringResource::Attachment(value) => {
                    let spec = AttachmentSpec::new(
                        value.protocol,
                        value.runtime,
                        value.location,
                        value.docker_image,
                        value.libkrun_guest_image,
                    )
                    .map_err(|error| SourceResolutionError::Attachment(error.to_string()))?;
                    Ok(ResourceDefinition::Attachment(AttachmentDefinition {
                        name: value.name,
                        spec,
                    }))
                },
            })
            .collect::<Result<Vec<_>, SourceResolutionError>>()?;
        Ok(ResourceDeclarations {
            api_version: self.api_version,
            resources,
        })
    }
}

/// Resolve a local provider source relative to the KCL file when needed.
///
/// KCL authoring runs with the user's authority, so an explicit local artifact
/// path is not restricted to the KCL package. Only the resolved digest crosses
/// the control boundary.
pub fn resolve_local_source(
    source: &LocalProviderSource,
    config_dir: &std::path::Path,
) -> Result<(PathBuf, ProviderId), SourceResolutionError> {
    let path = if source.path.is_absolute() {
        source.path.clone()
    } else {
        config_dir.join(&source.path)
    };
    let path = path.canonicalize().map_err(SourceResolutionError::Io)?;
    let bytes = std::fs::read(&path).map_err(SourceResolutionError::Io)?;
    let digest = ProviderId::from_wasm_bytes(&bytes);
    if digest != source.expected_digest {
        return Err(SourceResolutionError::DigestMismatch {
            path,
            expected: source.expected_digest,
            actual: digest,
        });
    }
    Ok((path, digest))
}

#[derive(Debug, Error)]
pub enum SourceResolutionError {
    #[error("provider source for `{0}` has not been resolved")]
    Unresolved(ResourceName),
    #[error("local provider source I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("local provider digest mismatch for {path}: expected {expected}, got {actual}")]
    DigestMismatch {
        path: PathBuf,
        expected: ProviderId,
        actual: ProviderId,
    },
    #[error("invalid attachment source: {0}")]
    Attachment(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn local_source_resolves_relative_to_config_and_checks_digest() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("provider.wasm");
        fs::write(&path, b"wasm").unwrap();
        let source = LocalProviderSource {
            path: PathBuf::from("provider.wasm"),
            expected_digest: ProviderId::from_wasm_bytes(b"wasm"),
        };
        let (_, digest) = resolve_local_source(&source, dir.path()).unwrap();
        assert_eq!(digest, source.expected_digest);
    }

    #[test]
    fn local_source_rejects_digest_mismatch() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("provider.wasm"), b"wasm").unwrap();
        let source = LocalProviderSource {
            path: PathBuf::from("provider.wasm"),
            expected_digest: ProviderId::from_wasm_bytes(b"other"),
        };
        assert!(matches!(
            resolve_local_source(&source, dir.path()),
            Err(SourceResolutionError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn local_source_resolves_an_explicit_path_outside_config_directory() {
        let dir = tempdir().unwrap();
        let artifact_dir = tempdir().unwrap();
        let artifact = artifact_dir.path().join("provider.wasm");
        fs::write(&artifact, b"wasm").unwrap();
        let source = LocalProviderSource {
            path: artifact,
            expected_digest: ProviderId::from_wasm_bytes(b"wasm"),
        };
        assert!(resolve_local_source(&source, dir.path()).is_ok());
    }

    #[test]
    fn provider_source_requires_exactly_one_selector() {
        let digest = ProviderId::from_wasm_bytes(b"provider");
        let accepted = serde_json::from_value::<ProviderSource>(serde_json::json!({
            "digest": digest,
        }))
        .unwrap();
        assert_eq!(accepted, ProviderSource::Digest { digest });

        for source in [
            serde_json::json!({}),
            serde_json::json!({"embedded": ""}),
            serde_json::json!({"embedded": "demo", "digest": digest}),
            serde_json::json!({"unknown": "demo"}),
        ] {
            assert!(serde_json::from_value::<ProviderSource>(source).is_err());
        }
    }
}
