//! Daemon-private durable state.

mod batch;
mod blob;
mod credential;
mod db;
mod mount;
mod paths;
mod provider;
mod row;
mod writer;

use anyhow::Context as _;
use omnifs_bootstrap::{Bootstrap, Daemon};
use omnifs_core::{
    AuthRuntimeFingerprint, CredentialGeneration, CredentialVersion, MountName, MountRevision,
    MountVersion, MutationId, ProviderId,
};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnection, SqlitePoolOptions};
use sqlx::{Connection as _, SqlitePool};
use std::ffi::OsStr;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

use credential::{credential_summaries_query, pending_revocations_query, stored_credentials_query};
use db::{Db, RecoveryTransition};
use mount::mounts_query;
use paths::{
    CLONE_CACHE_DIR, DAEMON_LOG_FILE, PROJECTION_CACHE_DIR, StorePaths, WASMTIME_CACHE_DIR,
    ensure_private_dir,
};
use provider::{
    MAX_PROVIDER_BYTES, PROVIDER_CHUNK_BYTES, provider_metadata_query, providers_query,
};
use writer::StateWriter;

pub use batch::{BatchError, OpOutcome, StateOp, StateOpError};
pub use credential::{
    CredentialDocument, CredentialRefreshKind, CredentialRefreshOutcome,
    CredentialRevocationFinish, CredentialState, CredentialSummary, PendingCredentialRevocation,
    SecretMaterial, StoredCredential, next_submitted,
};
pub use mount::{MountDocument, MountLimits, StoredMount};
pub use provider::{
    ProviderImportDisposition, ProviderImportOutcome, ProviderUpload, StoredProvider,
    StoredProviderMetadata, ValidatedProviderUpload,
};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

const READ_CONNECTIONS: u32 = 3;

/// Engine-owned directories nested under daemon state.
#[derive(Debug, Clone)]
pub struct EngineStatePaths {
    projection: PathBuf,
    wasmtime: PathBuf,
    clones: PathBuf,
}

impl EngineStatePaths {
    #[must_use]
    pub fn projection_cache(&self) -> &Path {
        &self.projection
    }

    #[must_use]
    pub fn wasmtime_cache(&self) -> &Path {
        &self.wasmtime
    }

    #[must_use]
    pub fn clone_cache(&self) -> &Path {
        &self.clones
    }
}

#[derive(Debug, Clone)]
pub struct StateStoreOptions {
    pub busy_timeout: Duration,
    pub disk_budget_bytes: u64,
}

impl Default for StateStoreOptions {
    fn default() -> Self {
        Self {
            busy_timeout: Duration::from_secs(5),
            disk_budget_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

pub struct StateStore {
    paths: StorePaths,
    options: StateStoreOptions,
    reads: SqlitePool,
    writer: StateWriter,
    credential_refresh_wakeup: watch::Sender<()>,
    provider_import: Arc<tokio::sync::Semaphore>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlStoreRepairDisposition {
    FreshStoreCreated,
    CorruptStoreArchived,
}

impl StateStore {
    pub async fn open(
        endpoint: &Bootstrap<Daemon>,
        options: StateStoreOptions,
    ) -> anyhow::Result<Self> {
        Self::open_paths(StorePaths::for_endpoint(endpoint), options).await
    }

    /// Archive the authoritative control store as one directory entry and open
    /// a fresh store. Cache, logs, staging, and bootstrap state stay untouched.
    pub async fn recreate_control_store(
        endpoint: &Bootstrap<Daemon>,
        options: StateStoreOptions,
    ) -> anyhow::Result<(Self, ControlStoreRepairDisposition)> {
        let paths = StorePaths::for_endpoint(endpoint);
        ensure_private_dir(paths.root())?;
        let archive = paths.archive_control_store()?;
        let disposition = if archive.is_some() {
            ControlStoreRepairDisposition::CorruptStoreArchived
        } else {
            ControlStoreRepairDisposition::FreshStoreCreated
        };
        match Self::open_paths(paths.clone(), options).await {
            Ok(store) => Ok((store, disposition)),
            Err(open_error) => {
                if let Err(rollback_error) = paths.rollback_control_store(archive.as_deref()) {
                    return Err(anyhow::anyhow!(
                        "{open_error:#}; control-store rollback also failed: {rollback_error:#}"
                    ));
                }
                Err(open_error)
            },
        }
    }

    async fn open_paths(paths: StorePaths, options: StateStoreOptions) -> anyhow::Result<Self> {
        paths.prepare()?;
        paths.cleanup_staging()?;

        let connect_options = db::connect_options(&paths.database(), options.busy_timeout);
        let reads = SqlitePoolOptions::new()
            .max_connections(READ_CONNECTIONS)
            .min_connections(1)
            .connect_with(connect_options.clone())
            .await
            .context("open StateStore read pool")?;
        MIGRATOR.run(&reads).await.context("migrate StateStore")?;
        db::check_integrity(&reads).await?;
        paths.restrict_database_files()?;

        let writer_connection = SqliteConnection::connect_with(&connect_options)
            .await
            .context("open StateStore writer connection")?;
        let (credential_refresh_wakeup, _wakeup_receiver) = watch::channel(());

        Ok(Self {
            paths,
            options,
            reads,
            writer: StateWriter::spawn(writer_connection),
            credential_refresh_wakeup,
            provider_import: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    #[must_use]
    pub fn engine_paths(&self) -> EngineStatePaths {
        let cache = self.paths.cache();
        EngineStatePaths {
            projection: cache.join(PROJECTION_CACHE_DIR),
            wasmtime: cache.join(WASMTIME_CACHE_DIR),
            clones: cache.join(CLONE_CACHE_DIR),
        }
    }

    pub async fn mount_revision(&self) -> anyhow::Result<MountRevision> {
        let revision: i64 =
            sqlx::query_scalar("SELECT revision FROM mount_state WHERE singleton = 1")
                .fetch_one(&self.reads)
                .await
                .context("read mount revision")?;
        Ok(MountRevision::new(
            u64::try_from(revision).context("mount revision is negative")?,
        ))
    }

    /// Read one exact durable head for serving-generation preparation.
    pub async fn serving_snapshot(&self) -> anyhow::Result<DurableServingSnapshot> {
        let mut transaction = self
            .reads
            .begin()
            .await
            .context("begin durable serving snapshot")?;
        let revision: i64 =
            sqlx::query_scalar("SELECT revision FROM mount_state WHERE singleton = 1")
                .fetch_one(&mut *transaction)
                .await
                .context("read snapshot mount revision")?;
        let mounts = sqlx::query_as::<_, StoredMount>(mounts_query!("ORDER BY name"))
            .fetch_all(&mut *transaction)
            .await
            .context("read snapshot mounts")?;
        let credentials = sqlx::query_as::<_, StoredCredential>(stored_credentials_query!(
            "WHERE status <> 'deleted' ORDER BY provider_name, scheme, account"
        ))
        .fetch_all(&mut *transaction)
        .await
        .context("read snapshot credentials")?;
        transaction
            .commit()
            .await
            .context("release durable serving snapshot")?;

        Ok(DurableServingSnapshot {
            revision: MountRevision::new(
                u64::try_from(revision).context("mount revision is negative")?,
            ),
            mounts,
            credentials,
        })
    }

    pub async fn serving_state(&self) -> anyhow::Result<ServingState> {
        let (state, detail, revision, failed_mutation) =
            sqlx::query_as::<_, (String, Option<String>, i64, Option<Vec<u8>>)>(
                "SELECT state, detail, serving_mount_revision, failed_mutation_id \
                 FROM recovery_state WHERE singleton = 1",
            )
            .fetch_one(&self.reads)
            .await
            .context("read recovery state")?;
        Ok(ServingState {
            recovery: RecoveryState::from_row(&state, detail)?,
            revision: MountRevision::new(
                u64::try_from(revision).context("serving revision is negative")?,
            ),
            failed_mutation: failed_mutation
                .as_deref()
                .map(row::decode_mutation_id)
                .transpose()?,
        })
    }

    pub async fn attach_port(&self) -> anyhow::Result<Option<NonZeroU16>> {
        db::read_attach_port(&self.reads).await
    }

    pub async fn persist_attach_port(&self, port: NonZeroU16) -> anyhow::Result<()> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection).write_attach_port(port).await;
                (connection, result)
            })
            .await?
    }

    pub async fn mark_serving(&self, revision: MountRevision) -> anyhow::Result<()> {
        self.transition(RecoveryTransition::Serving { revision })
            .await
    }

    pub async fn mark_recovery_required(
        &self,
        mutation: Option<MutationId>,
        detail: String,
    ) -> anyhow::Result<()> {
        self.transition(RecoveryTransition::RecoveryRequired { mutation, detail })
            .await
    }

    async fn transition(&self, transition: RecoveryTransition) -> anyhow::Result<()> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection)
                    .write_recovery_transition(transition)
                    .await;
                (connection, result)
            })
            .await?
    }

    pub async fn begin_provider_upload(
        &self,
        file_name: impl Into<String>,
        expected_id: ProviderId,
        expected_length: u64,
    ) -> anyhow::Result<ProviderUpload> {
        anyhow::ensure!(
            expected_length <= MAX_PROVIDER_BYTES,
            "provider artifact is {expected_length} bytes; maximum is {MAX_PROVIDER_BYTES}"
        );
        let file_name = validate_provider_file_name(file_name.into())?;
        let permit = Arc::clone(&self.provider_import)
            .acquire_owned()
            .await
            .context("provider import gate closed")?;
        self.paths
            .ensure_provider_disk_budget(&self.options, expected_length)?;
        ProviderUpload::create(
            &self.paths.staging(),
            file_name,
            expected_id,
            expected_length,
            permit,
        )
    }

    /// Stage trusted bytes through the same bounded, hashed, manifest-checked
    /// path used by streamed control uploads. The bundle owner supplies only
    /// bytes; state remains unaware of where they came from.
    pub async fn stage_provider_bytes(
        &self,
        file_name: impl Into<String>,
        expected_id: ProviderId,
        bytes: &[u8],
    ) -> anyhow::Result<ValidatedProviderUpload> {
        let expected_length =
            u64::try_from(bytes.len()).context("provider artifact is too large")?;
        let mut upload = self
            .begin_provider_upload(file_name, expected_id, expected_length)
            .await?;
        for chunk in bytes.chunks(PROVIDER_CHUNK_BYTES) {
            upload.write_chunk(chunk).await?;
        }
        upload.finish().await
    }

    /// Import one validated provider artifact. Content-digest dedup inside
    /// the write (`Inserted`/`Unchanged`/`Repaired`) is the only idempotency
    /// layer; this carries no mutation identity.
    pub async fn import_provider(
        &self,
        upload: ValidatedProviderUpload,
    ) -> anyhow::Result<ProviderImportOutcome> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection).write_provider(upload).await;
                (connection, result)
            })
            .await?
    }

    pub async fn load_provider(&self, id: ProviderId) -> anyhow::Result<Option<StoredProvider>> {
        sqlx::query_as::<_, StoredProvider>(providers_query!("WHERE digest = ?1"))
            .bind(id.as_bytes().as_slice())
            .fetch_optional(&self.reads)
            .await
            .context("load provider")
    }

    pub async fn load_provider_metadata(
        &self,
        id: ProviderId,
    ) -> anyhow::Result<Option<StoredProviderMetadata>> {
        sqlx::query_as::<_, StoredProviderMetadata>(provider_metadata_query!("WHERE digest = ?1"))
            .bind(id.as_bytes().as_slice())
            .fetch_optional(&self.reads)
            .await
            .context("load provider metadata")
    }

    pub async fn list_providers(&self) -> anyhow::Result<Vec<StoredProviderMetadata>> {
        sqlx::query_as::<_, StoredProviderMetadata>(provider_metadata_query!(
            "ORDER BY name, digest"
        ))
        .fetch_all(&self.reads)
        .await
        .context("list providers")
    }

    pub async fn get_mount(&self, name: &MountName) -> anyhow::Result<Option<StoredMount>> {
        sqlx::query_as::<_, StoredMount>(mounts_query!("WHERE name = ?1"))
            .bind(name.as_str())
            .fetch_optional(&self.reads)
            .await
            .context("load mount")
    }

    pub async fn list_mounts(&self) -> anyhow::Result<Vec<StoredMount>> {
        sqlx::query_as::<_, StoredMount>(mounts_query!("ORDER BY name"))
            .fetch_all(&self.reads)
            .await
            .context("list mounts")
    }

    /// Apply every op in `ops`, in order, inside one transaction. The first
    /// failure rolls back the whole batch. Every mounts/credentials row this
    /// batch creates or updates is stamped with `mutation_id`, and the global
    /// mount revision advances at most once, only if `ops` touches a mount.
    ///
    /// This is the sole entry point for the six wire mutation ops (mount
    /// create/update/remove, credential submit/delete/revoke); there is no
    /// standalone method for any of them.
    pub async fn apply_batch(
        &self,
        mutation_id: MutationId,
        ops: Vec<StateOp>,
    ) -> Result<Vec<OpOutcome>, BatchError> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection).apply_batch(mutation_id, ops).await;
                (connection, result)
            })
            .await
            .map_err(BatchError::Store)?
    }

    pub async fn get_credential(
        &self,
        id: &omnifs_auth::CredentialId,
    ) -> anyhow::Result<Option<StoredCredential>> {
        sqlx::query_as::<_, StoredCredential>(stored_credentials_query!(
            "WHERE provider_name = ?1 AND scheme = ?2 AND account = ?3"
        ))
        .bind(id.provider_name())
        .bind(id.scheme())
        .bind(id.account())
        .fetch_optional(&self.reads)
        .await
        .context("load credential")
    }

    pub async fn list_credentials(&self) -> anyhow::Result<Vec<CredentialSummary>> {
        sqlx::query_as::<_, CredentialSummary>(credential_summaries_query!(
            "ORDER BY provider_name, scheme, account"
        ))
        .fetch_all(&self.reads)
        .await
        .context("list credentials")
    }

    pub async fn pending_credential_revocations(
        &self,
    ) -> anyhow::Result<Vec<PendingCredentialRevocation>> {
        sqlx::query_as::<_, PendingCredentialRevocation>(pending_revocations_query!(
            "WHERE status = 'revocation-pending' ORDER BY provider_name, scheme, account"
        ))
        .fetch_all(&self.reads)
        .await
        .context("list pending credential revocations")
    }

    /// Complete a revocation an out-of-band provider call finished, matching
    /// it against the batch id recorded when the revocation began.
    pub async fn finish_credential_revocation(
        &self,
        id: omnifs_auth::CredentialId,
        mutation_id: MutationId,
        finish: CredentialRevocationFinish,
        scopes: Vec<String>,
    ) -> Result<CredentialMutationOutcome, CredentialWriteError> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection)
                    .write_credential_revocation_finish(id, mutation_id, finish, scopes)
                    .await;
                (connection, result)
            })
            .await?
    }

    /// Refresh a credential after auth has validated its opaque material and facts.
    pub async fn refresh_credential(
        &self,
        document: CredentialDocument,
        expected_version: CredentialVersion,
        kind: CredentialRefreshKind,
    ) -> Result<CredentialRefreshOutcome, CredentialWriteError> {
        let wakeup = self.credential_refresh_wakeup.clone();
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection)
                    .write_credential_refresh(document, expected_version, kind)
                    .await;
                // Wake republication before the caller observes the refresh, so
                // a `PendingRepublish` row is never durable-but-unannounced.
                if result
                    .as_ref()
                    .is_ok_and(|outcome| outcome.state == CredentialState::PendingRepublish)
                {
                    wakeup.send_modify(|()| {});
                }
                (connection, result)
            })
            .await?
    }

    /// Subscribe to durable authority-changing credential refreshes.
    ///
    /// The payload is only a signal. Receivers must rescan
    /// [`StateStore::list_credentials`] for `PendingRepublish` rows.
    pub fn subscribe_credential_refreshes(&self) -> watch::Receiver<()> {
        self.credential_refresh_wakeup.subscribe()
    }

    /// Activate one authority-changing refresh after its generation is published.
    pub async fn activate_refreshed_credential(
        &self,
        id: omnifs_auth::CredentialId,
        expected_version: CredentialVersion,
        expected_generation: CredentialGeneration,
    ) -> Result<CredentialRefreshOutcome, CredentialWriteError> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection)
                    .activate_refreshed_credential(id, expected_version, expected_generation)
                    .await;
                (connection, result)
            })
            .await?
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.writer.shutdown().await?;
        self.reads.close().await;
        Ok(())
    }

    /// Return the daemon-owned log path for the control server. The CLI never
    /// receives or opens this path; it is used only by daemon-owned streaming.
    pub fn daemon_log_path(&self) -> PathBuf {
        self.paths.logs().join(DAEMON_LOG_FILE)
    }
}

fn validate_provider_file_name(file_name: String) -> anyhow::Result<String> {
    let path = Path::new(&file_name);
    anyhow::ensure!(
        !file_name.is_empty()
            && file_name.len() <= 255
            && path.file_name() == Some(OsStr::new(&file_name)),
        "provider file name must be one nonempty path component"
    );
    Ok(file_name)
}

/// Open the daemon-owned log for append before the `StateStore` runtime starts.
pub fn open_daemon_log(endpoint: &Bootstrap<Daemon>) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let paths = StorePaths::for_endpoint(endpoint);
    ensure_private_dir(paths.root())?;
    let logs = paths.logs();
    ensure_private_dir(&logs)?;
    let path = logs.join(DAEMON_LOG_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open daemon log {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict daemon log {}", path.display()))?;
    Ok(file)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountMutationOutcome {
    pub name: MountName,
    pub version: Option<MountVersion>,
    pub revision: MountRevision,
}

/// The lease serializes every write, so the CAS conflicts a client-supplied
/// `if_version` used to catch are unreachable except through a bug; only
/// plain existence integrity errors and internal storage failures remain.
#[derive(Debug, thiserror::Error)]
pub enum MountWriteError {
    #[error("mount `{0}` already exists")]
    AlreadyExists(MountName),
    #[error("mount `{0}` was not found")]
    NotFound(MountName),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialMutationOutcome {
    pub provider_name: String,
    pub scheme: String,
    pub account_label: String,
    pub provider: ProviderId,
    pub kind: omnifs_auth::AuthKind,
    pub scopes: Vec<String>,
    pub auth_fingerprint: AuthRuntimeFingerprint,
    pub version: CredentialVersion,
    pub generation: CredentialGeneration,
    pub state: CredentialState,
    /// Provenance: the batch that produced this outcome, echoed back so the
    /// daemon can populate `CredentialStatus.last_mutation_id` without a
    /// second read.
    pub last_mutation_id: MutationId,
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialWriteError {
    #[error("credential `{0}` was not found")]
    NotFound(omnifs_auth::CredentialId),
    /// Real compare-and-swap, reachable only from the background refresh and
    /// activation paths: they race the lease-serialized batch writers rather
    /// than being serialized by them.
    #[error("credential `{id}` changed; expected {expected:?}, found {actual:?}")]
    Conflict {
        id: omnifs_auth::CredentialId,
        expected: CredentialVersion,
        actual: CredentialVersion,
    },
    #[error("credential `{id}` generation changed; expected {expected:?}, found {actual:?}")]
    GenerationConflict {
        id: omnifs_auth::CredentialId,
        expected: CredentialGeneration,
        actual: CredentialGeneration,
    },
    #[error("credential `{id}` facts do not match the stored credential")]
    FactsMismatch { id: omnifs_auth::CredentialId },
    #[error("credential `{id}` is in state {actual:?}; expected {expected}")]
    InvalidState {
        id: omnifs_auth::CredentialId,
        expected: &'static str,
        actual: CredentialState,
    },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// One transactionally consistent durable input for serving preparation.
#[derive(Debug)]
pub struct DurableServingSnapshot {
    pub revision: MountRevision,
    pub mounts: Vec<StoredMount>,
    pub credentials: Vec<StoredCredential>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryState {
    Ready,
    RecoveryRequired { detail: String },
}

impl RecoveryState {
    fn from_row(state: &str, detail: Option<String>) -> anyhow::Result<Self> {
        match state {
            "ready" if detail.is_none() => Ok(Self::Ready),
            "recovery-required" => Ok(Self::RecoveryRequired {
                detail: detail.context("recovery-required state has no detail")?,
            }),
            _ => anyhow::bail!("invalid recovery state `{state}`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServingState {
    pub recovery: RecoveryState,
    pub revision: MountRevision,
    pub failed_mutation: Option<MutationId>,
}

#[cfg(test)]
mod tests;
