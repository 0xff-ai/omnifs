//! Provider artifacts: staged upload, validation, and the durable BLOB.

use anyhow::Context as _;
use omnifs_core::{ProviderId, ProviderMeta, ProviderName, ProviderRef, ProviderVersion};
use omnifs_provider::{Artifact, ProviderManifest};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use sqlx::sqlite::SqliteRow;
use std::io::Read as _;
use std::path::Path;
use tokio::io::AsyncWriteExt as _;

use crate::blob::BlobHandle;
use crate::db::Db;
use crate::row::{RowExt as _, decode_error};

/// Maximum accepted provider artifact size.
pub(crate) const MAX_PROVIDER_BYTES: u64 = 128 * 1024 * 1024;

pub(crate) const PROVIDER_CHUNK_BYTES: usize = 1024 * 1024;

/// Above this size a committed import truncates the WAL instead of letting it
/// keep the whole artifact resident.
const LARGE_IMPORT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderImportDisposition {
    Inserted,
    Unchanged,
    Repaired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderImportOutcome {
    pub reference: ProviderRef,
    pub disposition: ProviderImportDisposition,
}

#[derive(Debug, Clone)]
pub struct StoredProviderMetadata {
    pub reference: ProviderRef,
    pub manifest: ProviderManifest,
    pub document: Vec<u8>,
}

/// Metadata-only reads must never pull the wasm BLOB, so the two provider
/// SELECT shapes stay separate. `concat!` keeps the result a `&'static str`,
/// which is what `sqlx::query_as` accepts.
macro_rules! provider_metadata_query {
    ($tail:literal) => {
        concat!(
            "SELECT digest, name, version, metadata FROM providers ",
            $tail
        )
    };
}
pub(crate) use provider_metadata_query;

macro_rules! providers_query {
    ($tail:literal) => {
        concat!(
            "SELECT digest, name, version, metadata, wasm, wasm_length FROM providers ",
            $tail
        )
    };
}
pub(crate) use providers_query;

impl StoredProviderMetadata {
    /// Prove the indexed columns still agree with the compiled manifest.
    fn decode_parts(
        digest: Vec<u8>,
        name: String,
        version: Option<String>,
        metadata: Vec<u8>,
    ) -> anyhow::Result<Self> {
        let length = digest.len();
        let digest: [u8; 32] = digest
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored provider digest has {length} bytes"))?;
        let name = ProviderName::new(name).context("stored provider has invalid name")?;
        let version = version.map(ProviderVersion::new);
        let manifest = ProviderManifest::from_bytes(&metadata)
            .context("stored provider metadata is invalid")?;
        anyhow::ensure!(
            manifest.id == name.as_str(),
            "stored provider name does not match metadata"
        );
        anyhow::ensure!(
            manifest.version.as_deref() == version.as_ref().map(ProviderVersion::as_str),
            "stored provider version does not match metadata"
        );
        Ok(Self {
            reference: ProviderRef {
                id: ProviderId::from_digest(digest),
                meta: ProviderMeta { name, version },
            },
            manifest,
            document: metadata,
        })
    }

    fn decode_row(row: &SqliteRow) -> anyhow::Result<Self> {
        Self::decode_parts(
            row.bytes("digest")?,
            row.text("name")?,
            row.optional_text("version")?,
            row.bytes("metadata")?,
        )
    }
}

impl FromRow<'_, SqliteRow> for StoredProviderMetadata {
    fn from_row(row: &SqliteRow) -> sqlx::Result<Self> {
        Self::decode_row(row).map_err(decode_error)
    }
}

#[derive(Debug)]
pub struct StoredProvider {
    pub reference: ProviderRef,
    pub manifest: ProviderManifest,
    pub bytes: Vec<u8>,
}

impl StoredProvider {
    fn decode_row(row: &SqliteRow) -> anyhow::Result<Self> {
        let metadata = StoredProviderMetadata::decode_row(row)?;
        let wasm = row.bytes("wasm")?;
        let wasm_length = row.unsigned("wasm_length")?;
        anyhow::ensure!(
            wasm_length <= MAX_PROVIDER_BYTES && usize::try_from(wasm_length)? == wasm.len(),
            "stored provider length is invalid"
        );
        anyhow::ensure!(
            ProviderId::from_wasm_bytes(&wasm) == metadata.reference.id,
            "stored provider digest does not match its key"
        );
        Ok(Self {
            reference: metadata.reference,
            manifest: metadata.manifest,
            bytes: wasm,
        })
    }
}

impl FromRow<'_, SqliteRow> for StoredProvider {
    fn from_row(row: &SqliteRow) -> sqlx::Result<Self> {
        Self::decode_row(row).map_err(decode_error)
    }
}

/// Storage-shaped: `rowid` addresses the incremental BLOB and has no domain
/// counterpart, and the metadata columns are read leniently so a corrupt row
/// can be repaired rather than failing the query.
#[derive(FromRow)]
struct RetainedProviderRow {
    rowid: i64,
    digest: Vec<u8>,
    name: String,
    version: Option<String>,
    metadata: Vec<u8>,
    wasm_length: i64,
}

pub struct ProviderUpload {
    path: tempfile::TempPath,
    file_name: String,
    file: Option<tokio::fs::File>,
    expected_id: ProviderId,
    expected_length: u64,
    written: u64,
    hasher: blake3::Hasher,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl ProviderUpload {
    pub(crate) fn create(
        staging: &Path,
        file_name: String,
        expected_id: ProviderId,
        expected_length: u64,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> anyhow::Result<Self> {
        let file = tempfile::Builder::new()
            .prefix("provider-")
            .suffix(".upload")
            .tempfile_in(staging)
            .context("create provider staging file")?;
        let (file, path) = file.into_parts();
        Ok(Self {
            path,
            file_name,
            file: Some(tokio::fs::File::from_std(file)),
            expected_id,
            expected_length,
            written: 0,
            hasher: blake3::Hasher::new(),
            permit: Some(permit),
        })
    }

    pub async fn write_chunk(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            bytes.len() <= PROVIDER_CHUNK_BYTES,
            "provider upload chunk is {} bytes; maximum is {PROVIDER_CHUNK_BYTES}",
            bytes.len()
        );
        let next = self
            .written
            .checked_add(u64::try_from(bytes.len())?)
            .context("provider upload length overflow")?;
        anyhow::ensure!(
            next <= self.expected_length,
            "provider upload exceeds declared length {}",
            self.expected_length
        );
        self.file
            .as_mut()
            .context("provider upload is already finished")?
            .write_all(bytes)
            .await
            .context("write provider staging file")?;
        self.hasher.update(bytes);
        self.written = next;
        Ok(())
    }

    pub async fn finish(mut self) -> anyhow::Result<ValidatedProviderUpload> {
        anyhow::ensure!(
            self.written == self.expected_length,
            "provider upload is truncated: expected {}, received {} bytes",
            self.expected_length,
            self.written
        );
        let mut file = self
            .file
            .take()
            .context("provider upload is already finished")?;
        file.flush().await.context("flush provider staging file")?;
        drop(file);

        let digest = self.hasher.finalize();
        anyhow::ensure!(
            digest.as_bytes() == self.expected_id.as_bytes(),
            "provider upload digest does not match declared digest"
        );
        let actual_id = self.expected_id;
        let path = self.path.to_path_buf();
        let file_name = self.file_name.clone();
        let (reference, metadata) = tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("read staged provider {}", path.display()))?;
            let (artifact, manifest) = Artifact::from_bytes_with_manifest(file_name, bytes)
                .context("validate staged provider")?;
            anyhow::ensure!(
                artifact.id() == actual_id,
                "staged provider changed during validation"
            );
            let metadata =
                serde_json::to_vec(&manifest).context("encode validated provider metadata")?;
            Ok::<_, anyhow::Error>((artifact.reference(), metadata))
        })
        .await
        .context("join provider validation task")??;

        Ok(ValidatedProviderUpload {
            path: self.path,
            id: actual_id,
            reference,
            metadata,
            length: self.expected_length,
            _permit: self
                .permit
                .take()
                .context("provider import permit missing")?,
        })
    }
}

pub struct ValidatedProviderUpload {
    path: tempfile::TempPath,
    id: ProviderId,
    reference: ProviderRef,
    metadata: Vec<u8>,
    length: u64,
    /// Held so the single-import gate stays closed until the import commits.
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Db<'_> {
    /// Import one validated provider artifact. Content-digest dedup inside
    /// `write_provider_blob` (`Inserted`/`Unchanged`/`Repaired`) is the only
    /// idempotency layer; there is no mutation identity to admit or record.
    pub(crate) async fn write_provider(
        &mut self,
        upload: ValidatedProviderUpload,
    ) -> anyhow::Result<ProviderImportOutcome> {
        let length = upload.length;
        let outcome = self
            .transact("provider import", async move |db| {
                db.write_provider_blob(&upload).await
            })
            .await?;
        if length >= LARGE_IMPORT_BYTES {
            self.request_truncating_checkpoint().await;
        }
        Ok(outcome)
    }

    async fn write_provider_blob(
        &mut self,
        upload: &ValidatedProviderUpload,
    ) -> anyhow::Result<ProviderImportOutcome> {
        let existing = sqlx::query_as::<_, RetainedProviderRow>(
            "SELECT rowid, digest, name, version, metadata, wasm_length \
             FROM providers WHERE digest = ?1",
        )
        .bind(upload.id.as_bytes().as_slice())
        .fetch_optional(self.raw())
        .await
        .context("inspect retained provider")?;
        let disposition = if let Some(row) = existing {
            let intact = StoredProviderMetadata::decode_parts(
                row.digest,
                row.name,
                row.version,
                row.metadata,
            )
            .is_ok_and(|stored| {
                stored.reference == upload.reference && stored.document == upload.metadata
            }) && self
                .retained_blob_is_valid(row.rowid, row.wasm_length, upload.id)
                .await?;
            if intact {
                return Ok(ProviderImportOutcome {
                    reference: upload.reference.clone(),
                    disposition: ProviderImportDisposition::Unchanged,
                });
            }
            self.allocate_provider_blob(upload, true).await?;
            ProviderImportDisposition::Repaired
        } else {
            self.allocate_provider_blob(upload, false).await?;
            ProviderImportDisposition::Inserted
        };

        let row_id: i64 = sqlx::query_scalar("SELECT rowid FROM providers WHERE digest = ?1")
            .bind(upload.id.as_bytes().as_slice())
            .fetch_one(self.raw())
            .await
            .context("resolve provider BLOB row")?;
        self.copy_provider_blob(upload, row_id).await?;
        Ok(ProviderImportOutcome {
            reference: upload.reference.clone(),
            disposition,
        })
    }

    /// Run one incremental BLOB session under the locked `SQLite` handle.
    async fn with_blob<T>(
        &mut self,
        row_id: i64,
        writable: bool,
        body: impl FnOnce(&mut BlobHandle) -> anyhow::Result<T> + Send,
    ) -> anyhow::Result<T>
    where
        T: Send,
    {
        let mut locked = self
            .raw()
            .lock_handle()
            .await
            .context("lock SQLite handle for provider BLOB")?;
        let value = tokio::task::block_in_place(|| {
            let mut blob = if writable {
                BlobHandle::open_write(locked.as_raw_handle(), c"providers", c"wasm", row_id)
            } else {
                BlobHandle::open_read(locked.as_raw_handle(), c"providers", c"wasm", row_id)
            }
            .context("open provider BLOB")?;
            let value = body(&mut blob)?;
            blob.close().context("close provider BLOB")?;
            Ok::<_, anyhow::Error>(value)
        })?;
        drop(locked);
        Ok(value)
    }

    async fn retained_blob_is_valid(
        &mut self,
        row_id: i64,
        stored_length: i64,
        expected: ProviderId,
    ) -> anyhow::Result<bool> {
        let Ok(length) = usize::try_from(stored_length) else {
            return Ok(false);
        };
        if u64::try_from(length)? > MAX_PROVIDER_BYTES {
            return Ok(false);
        }
        self.with_blob(row_id, false, move |blob| {
            if blob.len()? != length {
                return Ok(false);
            }
            let mut hasher = blake3::Hasher::new();
            let mut chunk = vec![0_u8; PROVIDER_CHUNK_BYTES];
            let mut offset = 0_usize;
            while offset < length {
                let chunk_length = (length - offset).min(chunk.len());
                blob.read(offset, &mut chunk[..chunk_length])
                    .context("read retained provider BLOB chunk")?;
                hasher.update(&chunk[..chunk_length]);
                offset += chunk_length;
            }
            Ok(ProviderId::from_digest(*hasher.finalize().as_bytes()) == expected)
        })
        .await
    }

    async fn allocate_provider_blob(
        &mut self,
        upload: &ValidatedProviderUpload,
        replace: bool,
    ) -> anyhow::Result<()> {
        let version = upload
            .reference
            .meta
            .version
            .as_ref()
            .map(ProviderVersion::as_str);
        let statement = if replace {
            "UPDATE providers SET name = ?2, version = ?3, metadata = ?4, \
             wasm = zeroblob(?5), wasm_length = ?5, created_at = unixepoch() \
             WHERE digest = ?1"
        } else {
            "INSERT INTO providers(\
                 digest, name, version, metadata, wasm, wasm_length, created_at\
             ) VALUES (?1, ?2, ?3, ?4, zeroblob(?5), ?5, unixepoch())"
        };
        sqlx::query(statement)
            .bind(upload.id.as_bytes().as_slice())
            .bind(upload.reference.meta.name.as_str())
            .bind(version)
            .bind(&upload.metadata)
            .bind(i64::try_from(upload.length)?)
            .execute(self.raw())
            .await
            .context("allocate provider BLOB")?;
        Ok(())
    }

    async fn copy_provider_blob(
        &mut self,
        upload: &ValidatedProviderUpload,
        row_id: i64,
    ) -> anyhow::Result<()> {
        let path: &Path = upload.path.as_ref();
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("open staged provider {}", path.display()))?;
        let expected = usize::try_from(upload.length)?;
        self.with_blob(row_id, true, move |blob| {
            anyhow::ensure!(
                blob.len()? == expected,
                "allocated provider BLOB has wrong length"
            );
            let mut chunk = vec![0_u8; PROVIDER_CHUNK_BYTES];
            let mut offset = 0_usize;
            loop {
                let read = file
                    .read(&mut chunk)
                    .context("read staged provider chunk")?;
                if read == 0 {
                    break;
                }
                blob.write(offset, &chunk[..read])
                    .context("write provider BLOB chunk")?;
                offset = offset
                    .checked_add(read)
                    .context("provider offset overflow")?;
            }
            anyhow::ensure!(
                offset == expected,
                "staged provider changed during BLOB copy"
            );
            Ok(())
        })
        .await
    }
}
