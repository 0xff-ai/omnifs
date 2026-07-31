//! Daemon-private durable state.

mod action;
mod attachment;
mod blob;
mod credential;
mod db;
mod mount;
mod paths;
mod provider;
mod resource;
mod row;
mod writer;

use anyhow::Context as _;
use omnifs_core::{
    ActionId, AuthRuntimeFingerprint, CredentialGeneration, CredentialVersion, MountRevision,
    ProviderId,
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

use credential::{credential_summaries_query, stored_credentials_query};
use db::{Db, RecoveryTransition};
use paths::{DAEMON_LOG_FILE, ensure_private_dir};
use provider::{
    MAX_PROVIDER_BYTES, PROVIDER_CHUNK_BYTES, provider_metadata_query, providers_query,
};
use writer::StateWriter;

pub use action::{
    ActionWriteError, AttachmentActionRequest, CredentialActionOperation, CredentialActionRequest,
};
pub use attachment::{AttachmentInstance, AttachmentObservation, AttachmentPhase};
pub use credential::{
    CredentialDocument, CredentialRefreshKind, CredentialRefreshOutcome,
    CredentialRevocationFinish, CredentialState, CredentialSummary, SecretMaterial,
    StoredCredential, next_submitted,
};
pub use mount::{MountDocument, MountLimits, StoredMount};
pub use paths::DaemonStatePaths;
pub use provider::{
    ProviderImportDisposition, ProviderImportOutcome, ProviderUpload, StoredProvider,
    StoredProviderMetadata, ValidatedProviderUpload,
};
pub use resource::{
    CredentialSecretSidecar, DesiredAttachment, ResourceApplyError, ResourceApplyRequest,
    ResourceSnapshot,
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
    paths: DaemonStatePaths,
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
    pub async fn open(paths: DaemonStatePaths, options: StateStoreOptions) -> anyhow::Result<Self> {
        Self::open_paths(paths, options).await
    }

    /// Archive the authoritative control store as one directory entry and open
    /// a fresh store. Cache, logs, staging, and bootstrap state stay untouched.
    pub async fn recreate_control_store(
        paths: DaemonStatePaths,
        options: StateStoreOptions,
    ) -> anyhow::Result<(Self, ControlStoreRepairDisposition)> {
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

    pub async fn open_paths(
        paths: DaemonStatePaths,
        options: StateStoreOptions,
    ) -> anyhow::Result<Self> {
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

        let mut writer_connection = SqliteConnection::connect_with(&connect_options)
            .await
            .context("open StateStore writer connection")?;
        Db::new(&mut writer_connection)
            .initialize_resources()
            .await
            .context("initialize desired resources")?;
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
        self.paths.engine_paths()
    }

    /// Read one exact, non-secret desired-resource head.
    pub async fn resource_snapshot(&self) -> anyhow::Result<ResourceSnapshot> {
        resource::snapshot(&self.reads).await
    }

    /// Atomically replace the complete desired resource set.
    pub async fn apply_resources(
        &self,
        request: ResourceApplyRequest,
    ) -> Result<omnifs_api::ApplyReceipt, ResourceApplyError> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection).apply_resources(request).await;
                (connection, result)
            })
            .await
            .map_err(ResourceApplyError::Store)?
    }

    /// Accept one credential action and its secret input in the same writer
    /// transaction as the durable non-secret receipt.
    pub async fn accept_credential_action(
        &self,
        request: CredentialActionRequest,
    ) -> Result<omnifs_api::ActionReceipt, ActionWriteError> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection)
                    .accept_credential_action(request)
                    .await;
                (connection, result)
            })
            .await?
    }

    /// Accept one non-secret attachment restart action and its durable receipt
    /// in one writer transaction.
    pub async fn accept_attachment_action(
        &self,
        request: AttachmentActionRequest,
    ) -> Result<omnifs_api::ActionReceipt, ActionWriteError> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection)
                    .accept_attachment_action(request)
                    .await;
                (connection, result)
            })
            .await?
    }

    pub async fn action_receipt(
        &self,
        action_id: omnifs_core::ActionId,
    ) -> anyhow::Result<Option<omnifs_api::ActionReceipt>> {
        let mut connection = self
            .reads
            .acquire()
            .await
            .context("acquire action receipt reader")?;
        action::action_receipt(&mut connection, action_id).await
    }

    pub async fn pending_actions(&self) -> anyhow::Result<Vec<omnifs_api::ActionReceipt>> {
        let mut connection = self
            .reads
            .acquire()
            .await
            .context("acquire pending action reader")?;
        action::pending_actions(&mut connection).await
    }

    pub async fn action_receipts(&self) -> anyhow::Result<Vec<omnifs_api::ActionReceipt>> {
        let mut connection = self
            .reads
            .acquire()
            .await
            .context("acquire action receipt reader")?;
        action::action_receipts(&mut connection).await
    }

    /// Read one exact observed attachment instance, including a deleting
    /// tombstone that no longer has a desired resource.
    pub async fn attachment_instance(
        &self,
        name: &omnifs_core::ResourceName,
    ) -> anyhow::Result<Option<AttachmentInstance>> {
        let mut connection = self
            .reads
            .acquire()
            .await
            .context("acquire attachment instance reader")?;
        attachment::load_instance(&mut connection, name).await
    }

    /// Read all observed attachment instances in stable name order.
    pub async fn attachment_instances(&self) -> anyhow::Result<Vec<AttachmentInstance>> {
        attachment::list_instances(&self.reads).await
    }

    /// Read all desired attachment definitions with their durable versions.
    pub async fn desired_attachments(&self) -> anyhow::Result<Vec<DesiredAttachment>> {
        resource::desired_attachments(&self.reads).await
    }

    /// Record one observed attachment phase after checking the exact desired,
    /// action, and runtime identity observed before the supervisor effect.
    ///
    /// A stale write returns `None` and cannot change desired state, deletion
    /// state, or action generation.
    pub async fn write_attachment_observation(
        &self,
        observation: AttachmentObservation,
    ) -> anyhow::Result<Option<AttachmentInstance>> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection)
                    .write_attachment_observation(observation)
                    .await;
                (connection, result)
            })
            .await?
    }

    /// Clear a deletion tombstone only if its desired row and exact runtime
    /// identity have not changed since the teardown proof.
    pub async fn clear_attachment_instance_if_deleting(
        &self,
        name: omnifs_core::ResourceName,
        runtime_instance: Option<String>,
    ) -> anyhow::Result<bool> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection)
                    .delete_attachment_instance_if_deleting(name, runtime_instance)
                    .await;
                (connection, result)
            })
            .await?
    }

    pub async fn transition_action(
        &self,
        action_id: omnifs_core::ActionId,
        phase: omnifs_api::ActionPhase,
        error_code: Option<String>,
        detail: Option<String>,
    ) -> Result<omnifs_api::ActionReceipt, ActionWriteError> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection)
                    .transition_action(action_id, phase, error_code, detail)
                    .await;
                (connection, result)
            })
            .await?
    }

    pub async fn serving_state(&self) -> anyhow::Result<ServingState> {
        let (state, detail, revision) = sqlx::query_as::<_, (String, Option<String>, i64)>(
            "SELECT state, detail, serving_mount_revision \
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

    pub async fn mark_recovery_required(&self, detail: String) -> anyhow::Result<()> {
        self.transition(RecoveryTransition::RecoveryRequired { detail })
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

    /// Complete a revocation an out-of-band provider call finished, matching
    /// it against the durable action id recorded when revocation began.
    pub async fn finish_credential_revocation(
        &self,
        id: omnifs_auth::CredentialId,
        action_id: ActionId,
        finish: CredentialRevocationFinish,
        scopes: Vec<String>,
    ) -> Result<CredentialMutationOutcome, CredentialWriteError> {
        self.writer
            .call(move |mut connection| async move {
                let result = Db::new(&mut connection)
                    .write_credential_revocation_finish(id, action_id, finish, scopes)
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
pub fn open_daemon_log(paths: &DaemonStatePaths) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

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
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialWriteError {
    #[error("credential `{0}` was not found")]
    NotFound(omnifs_auth::CredentialId),
    /// Real compare-and-swap, reachable only from the background refresh and
    /// activation paths: they can race daemon action and reconcile writers.
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
}

#[cfg(test)]
mod tests;
