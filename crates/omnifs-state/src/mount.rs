//! Mount documents, their canonical encoding, and durable mount mutations.

use anyhow::Context as _;
use omnifs_auth::CredentialId;
use omnifs_core::{
    MountName, MountRevision, MountVersion, MutationId, ProviderId, ProviderMeta, ProviderName,
    ProviderRef, ProviderVersion,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use sqlx::sqlite::SqliteRow;

use crate::db::Db;
use crate::row::{RowExt as _, decode_error, sql_int};
use crate::{MountMutationOutcome, MountWriteError};

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
    /// Provenance: the batch that last created or updated this row. Row
    /// metadata only; it never feeds the canonical bytes or version hash.
    pub last_mutation_id: MutationId,
}

/// One mount SELECT with its column list stated once. `concat!` keeps the
/// result a `&'static str`, which is what `sqlx::query_as` accepts.
macro_rules! mounts_query {
    ($tail:literal) => {
        concat!(
            "SELECT canonical, version, revision, last_mutation_id FROM mounts ",
            $tail
        )
    };
}
pub(crate) use mounts_query;

impl StoredMount {
    pub fn prepare(
        document: MountDocument,
        revision: MountRevision,
        last_mutation_id: MutationId,
    ) -> anyhow::Result<Self> {
        let (canonical, version) = encode(&document)?;
        Ok(Self {
            document,
            canonical,
            version,
            revision,
            last_mutation_id,
        })
    }

    fn decode_row(row: &SqliteRow) -> anyhow::Result<Self> {
        decode(
            row.bytes("canonical")?,
            MountVersion::from_digest(row.digest("version")?),
            MountRevision::new(row.unsigned("revision")?),
            row.mutation_id("last_mutation_id")?,
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
    last_mutation_id: MutationId,
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
        last_mutation_id,
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

impl Db<'_> {
    /// Read the mount revision a batch would commit if it wrote a mount row,
    /// without advancing it. The caller writes it back only once, after every
    /// op in the batch has succeeded.
    pub(crate) async fn next_mount_revision(&mut self) -> anyhow::Result<MountRevision> {
        let current: i64 =
            sqlx::query_scalar("SELECT revision FROM mount_state WHERE singleton = 1")
                .fetch_one(self.raw())
                .await
                .context("read mount revision")?;
        let current = MountRevision::new(
            u64::try_from(current).context("stored mount revision is negative")?,
        );
        current.next().context("mount revision exhausted")
    }

    /// Persist a batch's computed mount revision. Called at most once per
    /// batch, after every op has applied.
    pub(crate) async fn advance_mount_revision(
        &mut self,
        revision: MountRevision,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE mount_state SET revision = ?1 WHERE singleton = 1")
            .bind(sql_int(revision.get(), "mount revision")?)
            .execute(self.raw())
            .await
            .context("advance mount revision")?;
        Ok(())
    }

    pub(crate) async fn create_mount_row(
        &mut self,
        document: MountDocument,
        revision: MountRevision,
        mutation_id: MutationId,
    ) -> Result<MountMutationOutcome, MountWriteError> {
        if self.mount_version(&document.name).await?.is_some() {
            return Err(MountWriteError::AlreadyExists(document.name));
        }
        self.insert_or_replace_mount(&document, revision, mutation_id, false)
            .await
    }

    pub(crate) async fn update_mount_row(
        &mut self,
        document: MountDocument,
        revision: MountRevision,
        mutation_id: MutationId,
    ) -> Result<MountMutationOutcome, MountWriteError> {
        self.require_mount_exists(&document.name).await?;
        self.insert_or_replace_mount(&document, revision, mutation_id, true)
            .await
    }

    pub(crate) async fn remove_mount_row(
        &mut self,
        name: MountName,
        revision: MountRevision,
    ) -> Result<MountMutationOutcome, MountWriteError> {
        self.require_mount_exists(&name).await?;
        sqlx::query("DELETE FROM mounts WHERE name = ?1")
            .bind(name.as_str())
            .execute(self.raw())
            .await
            .context("remove mount")?;
        Ok(MountMutationOutcome {
            name,
            version: None,
            revision,
        })
    }

    async fn mount_version(
        &mut self,
        name: &MountName,
    ) -> Result<Option<MountVersion>, MountWriteError> {
        let version: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT version FROM mounts WHERE name = ?1")
                .bind(name.as_str())
                .fetch_optional(self.raw())
                .await
                .context("read mount version")?;
        let Some(version) = version else {
            return Ok(None);
        };
        let length = version.len();
        let version: [u8; 32] = version
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored mount version has {length} bytes"))?;
        Ok(Some(MountVersion::from_digest(version)))
    }

    /// The lease serializes every batch, so a mount vanishing between a
    /// caller's read and this write is a bug, not a race; the single-writer
    /// invariant makes this an existence probe rather than a CAS check.
    async fn require_mount_exists(&mut self, name: &MountName) -> Result<(), MountWriteError> {
        if self.mount_version(name).await?.is_none() {
            return Err(MountWriteError::NotFound(name.clone()));
        }
        Ok(())
    }

    async fn insert_or_replace_mount(
        &mut self,
        document: &MountDocument,
        revision: MountRevision,
        mutation_id: MutationId,
        replace: bool,
    ) -> Result<MountMutationOutcome, MountWriteError> {
        self.verify_mount_provider(&document.provider).await?;
        if let Some(credential) = document.credential.as_ref()
            && credential.provider_name() != document.provider.meta.name.as_str()
        {
            return Err(anyhow::anyhow!(
                "mount credential provider does not match pinned provider"
            )
            .into());
        }
        let (canonical, version) = encode(document)?;
        let credential_provider = document
            .credential
            .as_ref()
            .map(CredentialId::provider_name);
        let credential_scheme = document.credential.as_ref().map(CredentialId::scheme);
        let credential_account = document.credential.as_ref().map(CredentialId::account);
        let statement = if replace {
            "UPDATE mounts SET \
                 canonical = ?2, version = ?3, revision = ?4, provider_digest = ?5, \
                 credential_provider_name = ?6, credential_scheme = ?7, \
                 credential_account = ?8, last_mutation_id = ?9, updated_at = unixepoch() \
             WHERE name = ?1"
        } else {
            "INSERT INTO mounts(\
                 name, canonical, version, revision, provider_digest, \
                 credential_provider_name, credential_scheme, credential_account, \
                 last_mutation_id, updated_at\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, unixepoch())"
        };
        sqlx::query(statement)
            .bind(document.name.as_str())
            .bind(canonical)
            .bind(version.as_bytes().as_slice())
            .bind(sql_int(revision.get(), "mount revision")?)
            .bind(document.provider.id.as_bytes().as_slice())
            .bind(credential_provider)
            .bind(credential_scheme)
            .bind(credential_account)
            .bind(mutation_id.as_bytes().as_slice())
            .execute(self.raw())
            .await
            .context("write mount")?;
        Ok(MountMutationOutcome {
            name: document.name.clone(),
            version: Some(version),
            revision,
        })
    }

    async fn verify_mount_provider(
        &mut self,
        reference: &ProviderRef,
    ) -> Result<(), MountWriteError> {
        let (name, version) = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT name, version FROM providers WHERE digest = ?1",
        )
        .bind(reference.id.as_bytes().as_slice())
        .fetch_optional(self.raw())
        .await
        .context("load mount provider")?
        .with_context(|| format!("provider {} is not retained", reference.id))?;
        if name != reference.meta.name.as_str()
            || version.as_deref() != reference.meta.version.as_ref().map(ProviderVersion::as_str)
        {
            return Err(anyhow::anyhow!(
                "retained provider metadata does not match mount reference"
            )
            .into());
        }
        Ok(())
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
        let mutation_id = MutationId::from_bytes([0x99; 16]);
        let stored = decode(
            canonical.clone(),
            version,
            MountRevision::new(4),
            mutation_id,
        )
        .unwrap();
        assert_eq!(stored.document, document);
        assert_eq!(stored.canonical, canonical);
        assert_eq!(stored.version, version);
        assert_eq!(stored.revision, MountRevision::new(4));
        assert_eq!(stored.last_mutation_id, mutation_id);
    }
}
