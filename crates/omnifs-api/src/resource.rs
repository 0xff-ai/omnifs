//! Strict resource declarations, normalization, digesting, and pure planning.

use crate::{
    CredentialClientOverrides, CredentialMaterial, FilesystemDefinition,
    ProviderPreparationProgress, ServingProgress,
};
use omnifs_core::{
    FilesystemSpec, MutationId, ProviderId, ResourceDigest, ResourceKey, ResourceKind,
    ResourceName, ResourceRevision, filesystem_pair_supported_on_current_host, validate_account,
    validate_key_part,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The only resource-declaration version accepted by this API.
pub const API_VERSION: &str = "omnifs.dev/v1alpha1";
const DIGEST_DOMAIN: &[u8] = b"omnifs-resource-set-v1\0";

/// The complete desired state submitted by an authoring client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceDeclarations {
    pub api_version: String,
    pub resources: Vec<ResourceDefinition>,
}

impl ResourceDeclarations {
    pub fn normalize(self) -> Result<NormalizedResourceSet, ResourceDefinitionError> {
        if self.api_version != API_VERSION {
            return Err(ResourceDefinitionError::UnsupportedApiVersion(
                self.api_version,
            ));
        }
        NormalizedResourceSet::new(self.resources)
    }
}

/// One strict desired-resource declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "spec",
    rename_all = "PascalCase",
    deny_unknown_fields
)]
pub enum ResourceDefinition {
    Provider(ProviderDefinition),
    Credential(CredentialDefinition),
    Mount(MountResourceDefinition),
    Filesystem(FilesystemDefinition),
}

impl ResourceDefinition {
    #[must_use]
    pub const fn kind(&self) -> ResourceKind {
        match self {
            Self::Provider(_) => ResourceKind::Provider,
            Self::Credential(_) => ResourceKind::Credential,
            Self::Mount(_) => ResourceKind::Mount,
            Self::Filesystem(_) => ResourceKind::Filesystem,
        }
    }

    #[must_use]
    pub fn name(&self) -> &ResourceName {
        match self {
            Self::Provider(value) => value.name(),
            Self::Credential(value) => value.name(),
            Self::Mount(value) => value.name(),
            Self::Filesystem(value) => value.name(),
        }
    }

    #[must_use]
    pub fn key(&self) -> ResourceKey {
        ResourceKey::new(self.kind(), self.name().clone())
    }
}

/// A retained provider artifact used by mounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDefinition {
    pub name: ResourceName,
    pub artifact: ProviderId,
}

impl ProviderDefinition {
    #[must_use]
    pub const fn name(&self) -> &ResourceName {
        &self.name
    }
    #[must_use]
    pub fn key(&self) -> ResourceKey {
        ResourceKey::new(ResourceKind::Provider, self.name.clone())
    }
}

/// Non-secret declaration of a credential slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialDefinition {
    pub name: ResourceName,
    pub provider: ResourceName,
    pub scheme: String,
    pub account: String,
}

impl CredentialDefinition {
    #[must_use]
    pub const fn name(&self) -> &ResourceName {
        &self.name
    }
    #[must_use]
    pub fn key(&self) -> ResourceKey {
        ResourceKey::new(ResourceKind::Credential, self.name.clone())
    }
}

/// Desired provider projection rooted in the shared namespace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MountResourceDefinition {
    pub name: ResourceName,
    pub provider: ResourceName,
    pub credential: Option<ResourceName>,
    pub config: serde_json::Value,
    pub limits: Option<ResourceLimits>,
}

impl MountResourceDefinition {
    #[must_use]
    pub const fn name(&self) -> &ResourceName {
        &self.name
    }
    #[must_use]
    pub fn key(&self) -> ResourceKey {
        ResourceKey::new(ResourceKind::Mount, self.name.clone())
    }
}

/// Host-enforced provider resource limits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceLimits {
    pub max_memory_mb: Option<u32>,
    pub max_fetch_blob_bytes: Option<u64>,
}

/// A validated, sorted, cross-reference-complete desired resource set.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedResourceSet {
    resources: Vec<ResourceDefinition>,
    digest: ResourceDigest,
}

impl NormalizedResourceSet {
    pub fn new(mut resources: Vec<ResourceDefinition>) -> Result<Self, ResourceDefinitionError> {
        let mut keys = BTreeSet::new();
        for resource in &resources {
            let key = resource.key();
            if !keys.insert(key.clone()) {
                return Err(ResourceDefinitionError::DuplicateKey(key));
            }
            validate_resource(resource)?;
        }
        resources.sort_by_key(ResourceDefinition::key);
        validate_references(&resources)?;
        let digest = digest_resources(&resources);
        Ok(Self { resources, digest })
    }

    #[must_use]
    pub fn resources(&self) -> &[ResourceDefinition] {
        &self.resources
    }

    #[must_use]
    pub const fn digest(&self) -> ResourceDigest {
        self.digest
    }

    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new()).expect("empty resource set is valid")
    }
}

fn validate_resource(resource: &ResourceDefinition) -> Result<(), ResourceDefinitionError> {
    match resource {
        ResourceDefinition::Credential(credential) => {
            validate_key_part("credential scheme", &credential.scheme).map_err(|error| {
                ResourceDefinitionError::InvalidCredentialField(error.to_string())
            })?;
            validate_account(&credential.account).map_err(|error| {
                ResourceDefinitionError::InvalidCredentialField(error.to_string())
            })?;
        },
        ResourceDefinition::Mount(mount) if !mount.config.is_object() => {
            return Err(ResourceDefinitionError::MountConfigNotObject(
                mount.name.clone(),
            ));
        },
        ResourceDefinition::Filesystem(filesystem)
            if !filesystem_pair_supported_on_current_host(
                filesystem.spec.protocol(),
                filesystem.spec.runtime(),
            ) =>
        {
            return Err(ResourceDefinitionError::UnsupportedFilesystemPlatform {
                filesystem: filesystem.name.clone(),
                protocol: filesystem.spec.protocol(),
                runtime: filesystem.spec.runtime(),
            });
        },
        _ => {},
    }
    Ok(())
}

fn validate_references(resources: &[ResourceDefinition]) -> Result<(), ResourceDefinitionError> {
    let providers: BTreeMap<_, _> = resources
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Provider(value) => Some((value.name.clone(), value)),
            _ => None,
        })
        .collect();
    let credentials: BTreeMap<_, _> = resources
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Credential(value) => Some((value.name.clone(), value)),
            _ => None,
        })
        .collect();
    for resource in resources {
        if let ResourceDefinition::Credential(credential) = resource
            && !providers.contains_key(&credential.provider)
        {
            return Err(ResourceDefinitionError::MissingCredentialProvider {
                credential: credential.name.clone(),
                provider: credential.provider.clone(),
            });
        }
        let ResourceDefinition::Mount(mount) = resource else {
            continue;
        };
        if !providers.contains_key(&mount.provider) {
            return Err(ResourceDefinitionError::MissingProvider {
                mount: mount.name.clone(),
                provider: mount.provider.clone(),
            });
        }
        if let Some(credential_name) = &mount.credential {
            let Some(credential) = credentials.get(credential_name) else {
                return Err(ResourceDefinitionError::MissingCredential {
                    mount: mount.name.clone(),
                    credential: credential_name.clone(),
                });
            };
            if credential.provider != mount.provider {
                return Err(ResourceDefinitionError::CredentialProviderMismatch {
                    mount: mount.name.clone(),
                    credential: credential.name.clone(),
                    mount_provider: mount.provider.clone(),
                    credential_provider: credential.provider.clone(),
                });
            }
        }
    }
    Ok(())
}

fn digest_resources(resources: &[ResourceDefinition]) -> ResourceDigest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIGEST_DOMAIN);
    write_u64(
        &mut hasher,
        u64::try_from(resources.len()).expect("resource count fits u64"),
    );
    for resource in resources {
        hasher.update(&[resource.kind().tag()]);
        write_string(&mut hasher, resource.name().as_str());
        match resource {
            ResourceDefinition::Provider(value) => {
                hasher.update(value.artifact.as_bytes());
            },
            ResourceDefinition::Credential(value) => {
                write_string(&mut hasher, value.provider.as_str());
                write_string(&mut hasher, &value.scheme);
                write_string(&mut hasher, &value.account);
            },
            ResourceDefinition::Mount(value) => {
                write_string(&mut hasher, value.provider.as_str());
                write_optional_string(
                    &mut hasher,
                    value.credential.as_ref().map(ResourceName::as_str),
                );
                let config =
                    serde_json::to_vec(&value.config).expect("validated JSON value serializes");
                write_bytes(&mut hasher, &config);
                match &value.limits {
                    Some(limits) => {
                        hasher.update(&[1]);
                        write_optional_u32(&mut hasher, limits.max_memory_mb);
                        write_optional_u64(&mut hasher, limits.max_fetch_blob_bytes);
                    },
                    None => {
                        hasher.update(&[0]);
                    },
                }
            },
            ResourceDefinition::Filesystem(value) => write_filesystem(&mut hasher, &value.spec),
        }
    }
    ResourceDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn write_filesystem(hasher: &mut blake3::Hasher, spec: &FilesystemSpec) {
    hasher.update(&[match spec.protocol() {
        omnifs_core::FilesystemProtocol::Fuse => 1,
        omnifs_core::FilesystemProtocol::Nfs => 2,
    }]);
    hasher.update(&[match spec.runtime() {
        omnifs_core::FilesystemRuntime::Host => 1,
        omnifs_core::FilesystemRuntime::Docker => 2,
        omnifs_core::FilesystemRuntime::Libkrun => 3,
    }]);
    write_bytes(hasher, spec.location().as_os_str().as_encoded_bytes());
    write_optional_string(hasher, spec.docker_image());
    write_optional_string(hasher, spec.libkrun_guest_image());
}

fn write_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_be_bytes());
}
fn write_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    write_u64(hasher, u64::try_from(value.len()).expect("length fits u64"));
    hasher.update(value);
}
fn write_string(hasher: &mut blake3::Hasher, value: &str) {
    write_bytes(hasher, value.as_bytes());
}
fn write_optional_string(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            write_string(hasher, value);
        },
        None => {
            hasher.update(&[0]);
        },
    }
}
fn write_optional_u32(hasher: &mut blake3::Hasher, value: Option<u32>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hasher.update(&value.to_be_bytes());
        },
        None => {
            hasher.update(&[0]);
        },
    }
}
fn write_optional_u64(hasher: &mut blake3::Hasher, value: Option<u64>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hasher.update(&value.to_be_bytes());
        },
        None => {
            hasher.update(&[0]);
        },
    }
}

/// One pure desired-set difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceChangeAction {
    Create,
    Update,
    Delete,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceChange {
    pub key: ResourceKey,
    pub action: ResourceChangeAction,
    pub destructive: bool,
    pub secret_impact: bool,
}

/// Compute the full desired-set difference without effects.
#[must_use]
pub fn plan(
    current: &NormalizedResourceSet,
    desired: &NormalizedResourceSet,
) -> Vec<ResourceChange> {
    let current: BTreeMap<_, _> = current
        .resources()
        .iter()
        .map(|resource| (resource.key(), resource))
        .collect();
    let desired: BTreeMap<_, _> = desired
        .resources()
        .iter()
        .map(|resource| (resource.key(), resource))
        .collect();
    let keys: BTreeSet<_> = current.keys().chain(desired.keys()).cloned().collect();
    keys.into_iter()
        .map(|key| {
            let before = current.get(&key);
            let after = desired.get(&key);
            let action = match (before, after) {
                (None, Some(_)) => ResourceChangeAction::Create,
                (Some(_), None) => ResourceChangeAction::Delete,
                (Some(before), Some(after)) if before == after => ResourceChangeAction::Unchanged,
                (Some(_), Some(_)) => ResourceChangeAction::Update,
                (None, None) => unreachable!("union keys have one side"),
            };
            let credential = key.kind == ResourceKind::Credential;
            let filesystem = key.kind == ResourceKind::Filesystem;
            ResourceChange {
                key,
                action,
                destructive: matches!(action, ResourceChangeAction::Delete)
                    && (credential || filesystem),
                secret_impact: credential && !matches!(action, ResourceChangeAction::Unchanged),
            }
        })
        .collect()
}

/// A pure plan against one stored desired revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourcePlan {
    pub base_revision: ResourceRevision,
    pub desired_digest: ResourceDigest,
    pub changes: Vec<ResourceChange>,
}

/// Durable acknowledgement of one desired-set apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApplyReceipt {
    pub mutation_id: MutationId,
    pub revision: ResourceRevision,
    pub desired_digest: ResourceDigest,
    pub created: u32,
    pub updated: u32,
    pub deleted: u32,
    pub changed: bool,
}

/// One complete non-secret desired-state snapshot returned by the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceSnapshot {
    pub revision: ResourceRevision,
    pub desired_digest: ResourceDigest,
    pub resources: Vec<ResourceDefinition>,
    pub resource_statuses: Vec<ResourceStatus>,
    pub serving_revision: Option<ResourceRevision>,
    pub providers: Vec<ProviderPreparationProgress>,
    pub serving: Option<ServingProgress>,
}

/// Request-only secret material paired with one declared credential resource.
///
/// This value must never be returned by a control method, stored in an action
/// receipt, or included in progress output.
pub struct CredentialMaterialSidecar {
    pub credential: ResourceName,
    pub material: CredentialMaterial,
    pub overrides: CredentialClientOverrides,
}

impl std::fmt::Debug for CredentialMaterialSidecar {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialMaterialSidecar")
            .field("credential", &self.credential)
            .field("material", &self.material)
            .field("overrides", &self.overrides)
            .finish()
    }
}

/// One complete desired-set compare-and-swap request.
pub struct ApplyResourcesRequest {
    pub mutation_id: MutationId,
    pub base_revision: ResourceRevision,
    pub expected_desired_digest: ResourceDigest,
    pub declarations: ResourceDeclarations,
    pub credential_material: Vec<CredentialMaterialSidecar>,
}

impl std::fmt::Debug for ApplyResourcesRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApplyResourcesRequest")
            .field("mutation_id", &self.mutation_id)
            .field("base_revision", &self.base_revision)
            .field("expected_desired_digest", &self.expected_desired_digest)
            .field("declarations", &self.declarations)
            .field("credential_material", &self.credential_material)
            .finish()
    }
}

/// Stable observed lifecycle state for one desired resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePhase {
    Pending,
    Preparing,
    Ready,
    Retrying,
    Failed,
    Blocked,
    Deleting,
}

/// Desired and observed status of one resource, without secret material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceStatus {
    pub key: ResourceKey,
    pub desired_revision: ResourceRevision,
    pub observed_revision: Option<ResourceRevision>,
    pub phase: ResourcePhase,
    pub error_code: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResourceDefinitionError {
    #[error("unsupported resource apiVersion `{0}`; expected {API_VERSION}")]
    UnsupportedApiVersion(String),
    #[error("duplicate resource key {0}")]
    DuplicateKey(ResourceKey),
    #[error("invalid credential field: {0}")]
    InvalidCredentialField(String),
    #[error("mount {0} config must be a JSON object")]
    MountConfigNotObject(ResourceName),
    #[error(
        "Filesystem {filesystem} uses {protocol}/{runtime}, which this daemon host cannot launch"
    )]
    UnsupportedFilesystemPlatform {
        filesystem: ResourceName,
        protocol: omnifs_core::FilesystemProtocol,
        runtime: omnifs_core::FilesystemRuntime,
    },
    #[error("mount {mount} references missing provider {provider}")]
    MissingProvider {
        mount: ResourceName,
        provider: ResourceName,
    },
    #[error("credential {credential} references missing provider {provider}")]
    MissingCredentialProvider {
        credential: ResourceName,
        provider: ResourceName,
    },
    #[error("mount {mount} references missing credential {credential}")]
    MissingCredential {
        mount: ResourceName,
        credential: ResourceName,
    },
    #[error(
        "mount {mount} provider {mount_provider} does not match credential {credential} provider {credential_provider}"
    )]
    CredentialProviderMismatch {
        mount: ResourceName,
        credential: ResourceName,
        mount_provider: ResourceName,
        credential_provider: ResourceName,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_core::{FilesystemProtocol as Protocol, FilesystemRuntime as Runtime};
    use std::path::PathBuf;

    fn name(value: &str) -> ResourceName {
        ResourceName::new(value).unwrap()
    }
    fn provider() -> ResourceDefinition {
        ResourceDefinition::Provider(ProviderDefinition {
            name: name("github"),
            artifact: ProviderId::from_wasm_bytes(b"github"),
        })
    }
    fn credential() -> ResourceDefinition {
        ResourceDefinition::Credential(CredentialDefinition {
            name: name("github-default"),
            provider: name("github"),
            scheme: "oauth".into(),
            account: "default".into(),
        })
    }
    fn mount() -> ResourceDefinition {
        ResourceDefinition::Mount(MountResourceDefinition {
            name: name("github"),
            provider: name("github"),
            credential: Some(name("github-default")),
            config: serde_json::json!({"enabled": true}),
            limits: None,
        })
    }

    #[test]
    fn normalizes_order_to_one_pinned_digest() {
        let first = ResourceDeclarations {
            api_version: API_VERSION.into(),
            resources: vec![provider(), credential(), mount()],
        }
        .normalize()
        .unwrap();
        let second = ResourceDeclarations {
            api_version: API_VERSION.into(),
            resources: vec![mount(), provider(), credential()],
        }
        .normalize()
        .unwrap();
        assert_eq!(first.digest(), second.digest());
        assert_eq!(first.resources(), second.resources());
        assert_eq!(
            first.digest().to_string(),
            "d5c5d70cbb3cd8affbf36ddddd9da6dba5aa3ebd6e58e0d97c0ee5689342e640"
        );
    }

    #[test]
    fn rejects_duplicate_references_unknown_fields_and_bad_filesystem_pairs() {
        assert!(
            ResourceDeclarations {
                api_version: API_VERSION.into(),
                resources: vec![provider(), provider()]
            }
            .normalize()
            .is_err()
        );
        assert!(
            ResourceDeclarations {
                api_version: API_VERSION.into(),
                resources: vec![mount()]
            }
            .normalize()
            .is_err()
        );
        assert!(
            ResourceDeclarations {
                api_version: API_VERSION.into(),
                resources: vec![credential()]
            }
            .normalize()
            .is_err()
        );
        let json = r#"{"apiVersion":"omnifs.dev/v1alpha1","resources":[],"unknown":true}"#;
        assert!(serde_json::from_str::<ResourceDeclarations>(json).is_err());
        let json = r#"{"kind":"Provider","spec":{"name":"github","artifact":"8b8efb357747e21316404e52876f7c9c25bd4a1a0ce8f4cdf883fc386a8ef2e5","unknown":true}}"#;
        assert!(serde_json::from_str::<ResourceDefinition>(json).is_err());
        assert!(
            FilesystemSpec::new(
                Protocol::Nfs,
                Runtime::Docker,
                PathBuf::from("/omnifs"),
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn planner_covers_every_action_and_marks_credential_impacts() {
        let empty = NormalizedResourceSet::empty();
        let desired = NormalizedResourceSet::new(vec![provider(), credential(), mount()]).unwrap();
        let creates = plan(&empty, &desired);
        assert!(creates.iter().any(|change| change.action == ResourceChangeAction::Create && change.secret_impact));
        let unchanged = plan(&desired, &desired);
        assert!(
            unchanged
                .iter()
                .all(|change| change.action == ResourceChangeAction::Unchanged)
        );
        let changed = NormalizedResourceSet::new(vec![
            ResourceDefinition::Provider(ProviderDefinition {
                name: name("github"),
                artifact: ProviderId::from_wasm_bytes(b"new"),
            }),
            credential(),
            mount(),
        ])
        .unwrap();
        assert!(
            plan(&desired, &changed)
                .iter()
                .any(|change| change.action == ResourceChangeAction::Update)
        );
        let deletes = plan(&desired, &empty);
        assert!(
            deletes
                .iter()
                .any(|change| change.action == ResourceChangeAction::Delete && change.destructive)
        );
        let filesystem = NormalizedResourceSet::new(vec![ResourceDefinition::Filesystem(
            FilesystemDefinition {
                name: name("local"),
                spec: FilesystemSpec::new(
                    Protocol::Nfs,
                    Runtime::Host,
                    PathBuf::from("/tmp/omnifs"),
                    None,
                    None,
                )
                .unwrap(),
            },
        )])
        .unwrap();
        assert!(
            plan(&filesystem, &empty)
                .iter()
                .all(|change| change.destructive)
        );
    }
}
