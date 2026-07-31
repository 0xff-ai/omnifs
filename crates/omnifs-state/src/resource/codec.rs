//! Versioned storage encodings for desired resources and Attachment specs.

use anyhow::Context as _;
use omnifs_api::{
    AttachmentDefinition, CredentialDefinition, MountResourceDefinition, ProviderDefinition,
    ResourceDefinition, ResourceLimits,
};
use omnifs_core::{
    AttachmentProtocol as Protocol, AttachmentRuntime as Runtime, AttachmentSpec,
    AttachmentVersion, ProviderId, ResourceName, ResourceRevision,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::PathBuf;

const RESOURCES_PREFIX: &[u8] = b"omnifs.resources.v1\0";
const ATTACHMENT_PREFIX: &[u8] = b"omnifs.resource.attachment.v1\0";
const ATTACHMENT_VERSION_DOMAIN: &str = "omnifs resource attachment version v1";

#[derive(Debug, Serialize, Deserialize)]
struct StoredResourceSetV1 {
    resources: Vec<StoredResourceV1>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredResourceV1 {
    revision: u64,
    definition: StoredDefinitionV1,
}

#[derive(Debug, Serialize, Deserialize)]
enum StoredDefinitionV1 {
    Provider {
        name: String,
        artifact: [u8; 32],
    },
    Credential {
        name: String,
        provider: String,
        scheme: String,
        account: String,
    },
    Mount(StoredMountV1),
    Attachment(StoredAttachmentV1),
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredMountV1 {
    name: String,
    provider: String,
    credential: Option<String>,
    config: String,
    limits: Option<StoredLimitsV1>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredLimitsV1 {
    max_memory_mb: Option<u32>,
    max_fetch_blob_bytes: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredAttachmentV1 {
    name: String,
    protocol: String,
    runtime: String,
    location: Vec<u8>,
    docker_image: Option<String>,
    libkrun_guest_image: Option<String>,
}

pub(crate) fn encode_resources(
    resources: impl IntoIterator<Item = (ResourceDefinition, ResourceRevision)>,
) -> anyhow::Result<Vec<u8>> {
    let resources = resources
        .into_iter()
        .map(|(definition, revision)| {
            Ok(StoredResourceV1 {
                revision: revision.get(),
                definition: StoredDefinitionV1::from_definition(definition)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let payload = postcard::to_allocvec(&StoredResourceSetV1 { resources })
        .context("encode desired resources")?;
    let mut canonical = Vec::with_capacity(RESOURCES_PREFIX.len() + payload.len());
    canonical.extend_from_slice(RESOURCES_PREFIX);
    canonical.extend_from_slice(&payload);
    Ok(canonical)
}

pub(crate) fn decode_resources(
    canonical: &[u8],
) -> anyhow::Result<Vec<(ResourceDefinition, ResourceRevision)>> {
    let payload = canonical
        .strip_prefix(RESOURCES_PREFIX)
        .context("desired resource bytes have an unknown version")?;
    let stored: StoredResourceSetV1 =
        postcard::from_bytes(payload).context("decode desired resources")?;
    stored
        .resources
        .into_iter()
        .map(|resource| {
            Ok((
                resource.definition.into_definition()?,
                ResourceRevision::new(resource.revision),
            ))
        })
        .collect()
}

impl StoredDefinitionV1 {
    fn from_definition(definition: ResourceDefinition) -> anyhow::Result<Self> {
        Ok(match definition {
            ResourceDefinition::Provider(definition) => Self::Provider {
                name: definition.name.to_string(),
                artifact: *definition.artifact.as_bytes(),
            },
            ResourceDefinition::Credential(definition) => Self::Credential {
                name: definition.name.to_string(),
                provider: definition.provider.to_string(),
                scheme: definition.scheme,
                account: definition.account,
            },
            ResourceDefinition::Mount(definition) => Self::Mount(StoredMountV1 {
                name: definition.name.to_string(),
                provider: definition.provider.to_string(),
                credential: definition.credential.map(|name| name.to_string()),
                config: serde_json::to_string(&definition.config)
                    .context("encode mount resource config")?,
                limits: definition.limits.map(StoredLimitsV1::from),
            }),
            ResourceDefinition::Attachment(definition) => {
                Self::Attachment(StoredAttachmentV1::from_definition(definition))
            },
        })
    }

    fn into_definition(self) -> anyhow::Result<ResourceDefinition> {
        Ok(match self {
            Self::Provider { name, artifact } => ResourceDefinition::Provider(ProviderDefinition {
                name: ResourceName::new(name).context("invalid stored provider name")?,
                artifact: ProviderId::from_digest(artifact),
            }),
            Self::Credential {
                name,
                provider,
                scheme,
                account,
            } => ResourceDefinition::Credential(CredentialDefinition {
                name: ResourceName::new(name).context("invalid stored credential name")?,
                provider: ResourceName::new(provider)
                    .context("invalid stored credential provider")?,
                scheme,
                account,
            }),
            Self::Mount(definition) => ResourceDefinition::Mount(MountResourceDefinition {
                name: ResourceName::new(definition.name).context("invalid stored mount name")?,
                provider: ResourceName::new(definition.provider)
                    .context("invalid stored mount provider")?,
                credential: definition
                    .credential
                    .map(ResourceName::new)
                    .transpose()
                    .context("invalid stored mount credential")?,
                config: serde_json::from_str(&definition.config)
                    .context("decode stored mount config")?,
                limits: definition.limits.map(ResourceLimits::from),
            }),
            Self::Attachment(definition) => {
                ResourceDefinition::Attachment(definition.into_definition()?)
            },
        })
    }
}

impl From<ResourceLimits> for StoredLimitsV1 {
    fn from(limits: ResourceLimits) -> Self {
        Self {
            max_memory_mb: limits.max_memory_mb,
            max_fetch_blob_bytes: limits.max_fetch_blob_bytes,
        }
    }
}

impl From<StoredLimitsV1> for ResourceLimits {
    fn from(limits: StoredLimitsV1) -> Self {
        Self {
            max_memory_mb: limits.max_memory_mb,
            max_fetch_blob_bytes: limits.max_fetch_blob_bytes,
        }
    }
}

impl StoredAttachmentV1 {
    fn from_definition(definition: AttachmentDefinition) -> Self {
        Self {
            name: definition.name.to_string(),
            protocol: definition.spec.protocol().to_string(),
            runtime: definition.spec.runtime().to_string(),
            location: definition.spec.location().as_os_str().as_bytes().to_vec(),
            docker_image: definition.spec.docker_image().map(ToOwned::to_owned),
            libkrun_guest_image: definition.spec.libkrun_guest_image().map(ToOwned::to_owned),
        }
    }

    fn into_definition(self) -> anyhow::Result<AttachmentDefinition> {
        let runtime = self
            .runtime
            .parse::<Runtime>()
            .context("invalid stored attachment runtime")?;
        let protocol = self
            .protocol
            .parse::<Protocol>()
            .context("invalid stored attachment protocol")?;
        Ok(AttachmentDefinition {
            name: ResourceName::new(self.name).context("invalid stored attachment name")?,
            spec: AttachmentSpec::new(
                protocol,
                runtime,
                PathBuf::from(OsString::from_vec(self.location)),
                self.docker_image,
                self.libkrun_guest_image,
            )
            .context("invalid stored attachment spec")?,
        })
    }
}

pub(crate) fn encode_attachment(
    definition: &AttachmentDefinition,
) -> anyhow::Result<(Vec<u8>, AttachmentVersion)> {
    let payload = postcard::to_allocvec(&StoredAttachmentV1::from_definition(definition.clone()))
        .context("encode attachment resource")?;
    let mut canonical = Vec::with_capacity(ATTACHMENT_PREFIX.len() + payload.len());
    canonical.extend_from_slice(ATTACHMENT_PREFIX);
    canonical.extend_from_slice(&payload);
    let mut hasher = blake3::Hasher::new_derive_key(ATTACHMENT_VERSION_DOMAIN);
    hasher.update(&canonical);
    Ok((
        canonical,
        AttachmentVersion::from_digest(*hasher.finalize().as_bytes()),
    ))
}

pub(crate) fn decode_attachment(
    canonical: &[u8],
    stored_version: AttachmentVersion,
) -> anyhow::Result<AttachmentDefinition> {
    let payload = canonical
        .strip_prefix(ATTACHMENT_PREFIX)
        .context("attachment resource bytes have an unknown version")?;
    let stored: StoredAttachmentV1 =
        postcard::from_bytes(payload).context("decode attachment resource")?;
    let definition = stored.into_definition()?;
    let (_, actual_version) = encode_attachment(&definition)?;
    anyhow::ensure!(
        actual_version == stored_version,
        "stored attachment resource version does not match canonical bytes"
    );
    Ok(definition)
}
