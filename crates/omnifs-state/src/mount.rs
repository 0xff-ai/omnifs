//! Mount documents, their canonical encoding, and durable mount mutations.

use anyhow::Context as _;
use omnifs_auth::CredentialId;
use omnifs_core::{
    MountName, MountRevision, MountVersion, ProviderId, ProviderMeta, ProviderName, ProviderRef,
    ProviderVersion,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use sqlx::sqlite::SqliteRow;

use crate::row::{RowExt as _, decode_error};

const CANONICAL_MOUNT_V1_PREFIX: &[u8] = b"omnifs.mount.v1\0";
const MOUNT_VERSION_DOMAIN: &str = "omnifs mount version v1";

#[derive(Debug, Clone, PartialEq)]
pub struct MountDocument {
    pub name: MountName,
    pub provider: ProviderRef,
    pub credential: Option<CredentialId>,
    pub limits: Option<MountLimits>,
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountLimits {
    pub max_memory_mb: Option<u32>,
    pub max_fetch_blob_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredMount {
    pub document: MountDocument,
    pub canonical: Vec<u8>,
    pub version: MountVersion,
    pub revision: MountRevision,
}

/// One mount SELECT with its column list stated once. `concat!` keeps the
/// result a `&'static str`, which is what `sqlx::query_as` accepts.
macro_rules! mounts_query {
    ($tail:literal) => {
        concat!("SELECT canonical, version, revision FROM mounts ", $tail)
    };
}
pub(crate) use mounts_query;

impl StoredMount {
    pub fn prepare(document: MountDocument, revision: MountRevision) -> anyhow::Result<Self> {
        let (canonical, version) = encode(&document)?;
        Ok(Self {
            document,
            canonical,
            version,
            revision,
        })
    }

    fn decode_row(row: &SqliteRow) -> anyhow::Result<Self> {
        decode(
            row.bytes("canonical")?,
            MountVersion::from_digest(row.digest("version")?),
            MountRevision::new(row.unsigned("revision")?),
        )
    }
}

impl FromRow<'_, SqliteRow> for StoredMount {
    fn from_row(row: &SqliteRow) -> sqlx::Result<Self> {
        Self::decode_row(row).map_err(decode_error)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CanonicalMountV1 {
    name: String,
    provider_id: [u8; 32],
    provider_name: String,
    provider_version: Option<String>,
    credential: Option<CanonicalCredentialV1>,
    limits: Option<CanonicalLimitsV1>,
    /// Config as canonical JSON text. `serde_json::Map` is a `BTreeMap` while
    /// the `preserve_order` feature is off, so keys serialize in byte order and
    /// integers stay distinct from floats. Turning that feature on anywhere in
    /// the workspace would silently change these bytes and invalidate every
    /// stored mount version.
    config: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CanonicalCredentialV1 {
    provider_name: String,
    scheme: String,
    account_label: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CanonicalLimitsV1 {
    max_memory_mb: Option<u32>,
    max_fetch_blob_bytes: Option<u64>,
}

pub(crate) fn encode(document: &MountDocument) -> anyhow::Result<(Vec<u8>, MountVersion)> {
    let dto = CanonicalMountV1::from_document(document)?;
    let payload = postcard::to_allocvec(&dto).context("encode canonical mount")?;
    let mut bytes = Vec::with_capacity(CANONICAL_MOUNT_V1_PREFIX.len() + payload.len());
    bytes.extend_from_slice(CANONICAL_MOUNT_V1_PREFIX);
    bytes.extend_from_slice(&payload);
    let mut hasher = blake3::Hasher::new_derive_key(MOUNT_VERSION_DOMAIN);
    hasher.update(&bytes);
    let version = MountVersion::from_digest(*hasher.finalize().as_bytes());
    Ok((bytes, version))
}

pub(crate) fn decode(
    canonical: Vec<u8>,
    stored_version: MountVersion,
    revision: MountRevision,
) -> anyhow::Result<StoredMount> {
    let payload = canonical
        .strip_prefix(CANONICAL_MOUNT_V1_PREFIX)
        .context("mount canonical bytes have an unknown version")?;
    let dto: CanonicalMountV1 = postcard::from_bytes(payload).context("decode canonical mount")?;
    let document = dto.into_document()?;
    let (_, actual_version) = encode(&document)?;
    anyhow::ensure!(
        actual_version == stored_version,
        "stored mount version does not match canonical bytes"
    );
    Ok(StoredMount {
        document,
        canonical,
        version: stored_version,
        revision,
    })
}

impl CanonicalMountV1 {
    fn from_document(document: &MountDocument) -> anyhow::Result<Self> {
        Ok(Self {
            name: document.name.to_string(),
            provider_id: *document.provider.id.as_bytes(),
            provider_name: document.provider.meta.name.to_string(),
            provider_version: document
                .provider
                .meta
                .version
                .as_ref()
                .map(ToString::to_string),
            credential: document
                .credential
                .as_ref()
                .map(|credential| CanonicalCredentialV1 {
                    provider_name: credential.provider_name().to_owned(),
                    scheme: credential.scheme().to_owned(),
                    account_label: credential.account().to_owned(),
                }),
            limits: document.limits.as_ref().map(|limits| CanonicalLimitsV1 {
                max_memory_mb: limits.max_memory_mb,
                max_fetch_blob_bytes: limits.max_fetch_blob_bytes,
            }),
            config: serde_json::to_string(&document.config).context("encode mount config")?,
        })
    }

    fn into_document(self) -> anyhow::Result<MountDocument> {
        let provider_name =
            ProviderName::new(self.provider_name).context("invalid stored provider name")?;
        let provider = ProviderRef {
            id: ProviderId::from_digest(self.provider_id),
            meta: ProviderMeta {
                name: provider_name,
                version: self.provider_version.map(ProviderVersion::new),
            },
        };
        let credential = self
            .credential
            .map(|credential| {
                CredentialId::new(
                    credential.provider_name,
                    credential.scheme,
                    credential.account_label,
                )
            })
            .transpose()
            .context("invalid stored credential identity")?;
        Ok(MountDocument {
            name: MountName::new(self.name).context("invalid stored mount name")?,
            provider,
            credential,
            limits: self.limits.map(|limits| MountLimits {
                max_memory_mb: limits.max_memory_mb,
                max_fetch_blob_bytes: limits.max_fetch_blob_bytes,
            }),
            config: serde_json::from_str(&self.config).context("decode stored mount config")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(config: serde_json::Value) -> MountDocument {
        MountDocument {
            name: MountName::new("demo").unwrap(),
            provider: ProviderRef {
                id: ProviderId::from_wasm_bytes(b"demo"),
                meta: ProviderMeta {
                    name: ProviderName::new("demo").unwrap(),
                    version: Some(ProviderVersion::new("1")),
                },
            },
            credential: None,
            limits: None,
            config,
        }
    }

    #[test]
    fn canonical_json_sorts_objects_and_preserves_number_kinds() {
        let first = document(serde_json::json!({"z": 1.0, "a": 1}));
        let second = document(serde_json::json!({"a": 1, "z": 1.0}));
        let (first_bytes, first_version) = encode(&first).unwrap();
        let (second_bytes, second_version) = encode(&second).unwrap();
        assert_eq!(first_bytes, second_bytes);
        assert_eq!(first_version, second_version);

        let integer = encode(&document(serde_json::json!({"a": 1}))).unwrap();
        let float = encode(&document(serde_json::json!({"a": 1.0}))).unwrap();
        assert_ne!(integer, float);
    }

    /// Canonical bytes are a persisted hash input, and the config half of them
    /// is whatever `serde_json` emits. Pin the exact text so enabling
    /// `serde_json/preserve_order`, or any change to number formatting, fails
    /// here instead of silently invalidating every stored mount version.
    #[test]
    fn canonical_config_text_is_pinned() {
        const EXPECTED: &str = r#"{"a":1,"nested":{"a":null,"b":true},"z":1.0}"#;

        let (bytes, _) = encode(&document(
            serde_json::json!({"z": 1.0, "a": 1, "nested": {"b": true, "a": null}}),
        ))
        .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains(EXPECTED),
            "canonical config text changed; expected to find {EXPECTED} in {text}"
        );
    }

    #[test]
    fn canonical_mount_round_trips() {
        let document = document(serde_json::json!({"nested": [true, null, -2, 3.5]}));
        let (canonical, version) = encode(&document).unwrap();
        let stored = decode(canonical.clone(), version, MountRevision::new(4)).unwrap();
        assert_eq!(stored.document, document);
        assert_eq!(stored.canonical, canonical);
        assert_eq!(stored.version, version);
        assert_eq!(stored.revision, MountRevision::new(4));
    }
}
