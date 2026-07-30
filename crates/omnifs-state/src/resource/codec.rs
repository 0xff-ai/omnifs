//! Versioned stored encodings for resource rows with structured payloads.

use anyhow::Context as _;
use omnifs_api::{AttachmentDefinition, MountResourceDefinition, ResourceLimits};
use omnifs_core::fs::{Protocol, Runtime};
use omnifs_core::{AttachmentSpec, AttachmentVersion, ResourceName};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::PathBuf;

const MOUNT_PREFIX: &[u8] = b"omnifs.resource.mount.v1\0";
const MOUNT_VERSION_DOMAIN: &str = "omnifs resource mount version v1";
const ATTACHMENT_PREFIX: &[u8] = b"omnifs.resource.attachment.v1\0";
const ATTACHMENT_VERSION_DOMAIN: &str = "omnifs resource attachment version v1";

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

pub(super) fn encode_mount(
    definition: &MountResourceDefinition,
) -> anyhow::Result<(Vec<u8>, [u8; 32])> {
    let payload = postcard::to_allocvec(&StoredMountV1 {
        name: definition.name.to_string(),
        provider: definition.provider.to_string(),
        credential: definition.credential.as_ref().map(ToString::to_string),
        config: serde_json::to_string(&definition.config)
            .context("encode mount resource config")?,
        limits: definition.limits.as_ref().map(|limits| StoredLimitsV1 {
            max_memory_mb: limits.max_memory_mb,
            max_fetch_blob_bytes: limits.max_fetch_blob_bytes,
        }),
    })
    .context("encode mount resource")?;
    let mut canonical = Vec::with_capacity(MOUNT_PREFIX.len() + payload.len());
    canonical.extend_from_slice(MOUNT_PREFIX);
    canonical.extend_from_slice(&payload);
    let mut hasher = blake3::Hasher::new_derive_key(MOUNT_VERSION_DOMAIN);
    hasher.update(&canonical);
    Ok((canonical, *hasher.finalize().as_bytes()))
}

pub(super) fn decode_mount(
    canonical: &[u8],
    stored_version: [u8; 32],
) -> anyhow::Result<MountResourceDefinition> {
    let payload = canonical
        .strip_prefix(MOUNT_PREFIX)
        .context("mount resource canonical bytes have an unknown version")?;
    let stored: StoredMountV1 = postcard::from_bytes(payload).context("decode mount resource")?;
    let definition = MountResourceDefinition {
        name: ResourceName::new(stored.name).context("invalid stored mount resource name")?,
        provider: ResourceName::new(stored.provider)
            .context("invalid stored mount provider name")?,
        credential: stored
            .credential
            .map(ResourceName::new)
            .transpose()
            .context("invalid stored mount credential name")?,
        config: serde_json::from_str(&stored.config)
            .context("decode stored mount resource config")?,
        limits: stored.limits.map(|limits| ResourceLimits {
            max_memory_mb: limits.max_memory_mb,
            max_fetch_blob_bytes: limits.max_fetch_blob_bytes,
        }),
    };
    let (_, actual_version) = encode_mount(&definition)?;
    anyhow::ensure!(
        actual_version == stored_version,
        "stored mount resource version does not match canonical bytes"
    );
    Ok(definition)
}

pub(super) fn encode_attachment(
    definition: &AttachmentDefinition,
) -> anyhow::Result<(Vec<u8>, AttachmentVersion)> {
    let payload = postcard::to_allocvec(&StoredAttachmentV1 {
        name: definition.name.to_string(),
        protocol: definition.spec.protocol().to_string(),
        runtime: definition.spec.runtime().to_string(),
        location: definition.spec.location().as_os_str().as_bytes().to_vec(),
        docker_image: definition.spec.docker_image().map(ToOwned::to_owned),
        libkrun_guest_image: definition.spec.libkrun_guest_image().map(ToOwned::to_owned),
    })
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

pub(super) fn decode_attachment(
    canonical: &[u8],
    stored_version: AttachmentVersion,
) -> anyhow::Result<AttachmentDefinition> {
    let payload = canonical
        .strip_prefix(ATTACHMENT_PREFIX)
        .context("attachment resource canonical bytes have an unknown version")?;
    let stored: StoredAttachmentV1 =
        postcard::from_bytes(payload).context("decode attachment resource")?;
    let runtime = stored
        .runtime
        .parse::<Runtime>()
        .context("invalid stored attachment runtime")?;
    let protocol = stored
        .protocol
        .parse::<Protocol>()
        .context("invalid stored attachment protocol")?;
    let definition = AttachmentDefinition {
        name: ResourceName::new(stored.name).context("invalid stored attachment resource name")?,
        spec: AttachmentSpec::new(
            protocol,
            runtime,
            PathBuf::from(OsString::from_vec(stored.location)),
            stored.docker_image,
            stored.libkrun_guest_image,
        )
        .context("invalid stored attachment spec")?,
    };
    let (_, actual_version) = encode_attachment(&definition)?;
    anyhow::ensure!(
        actual_version == stored_version,
        "stored attachment resource version does not match canonical bytes"
    );
    Ok(definition)
}
