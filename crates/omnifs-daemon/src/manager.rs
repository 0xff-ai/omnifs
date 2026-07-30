//! The daemon's single mutation lease and the one batch-apply path.
//!
//! `MutationManager` is a thin async facade over a single-writer background
//! task ([`run_manager`]) that owns [`ManagerState`], including the one
//! mutation slot. `begin`/`apply`/`drop_mutation` all go through that same
//! task alongside every other durable state change, so the lease only has to
//! police the `Begin` → `Apply` gap: once a batch is admitted, nothing else
//! can run concurrently, so it always runs to completion regardless of how
//! long generation rebuild and activation take.

use crate::generation_builder::{
    GenerationBuild, GenerationDraft, GenerationParts, PreparedCredentialRevocation,
    credential_scopes, prepare_credential_document, prepare_credential_revocation,
};
use anyhow::Context as _;
use omnifs_api::{
    ActiveMutation, CredentialKey, MountDefinition, MountField, MountPatch, MutationOp,
    MutationOpResult, ServingOutcome,
};
use omnifs_auth::CredentialId;
use omnifs_core::{MutationId, ProviderId};
use omnifs_engine::{DrainOutcome, HostOnline, ServingCell};
use omnifs_state::{
    BatchError, CredentialRevocationFinish, CredentialWriteError, MountDocument, MountLimits,
    MountWriteError, StateOp, StateStore, StoredCredential,
};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

const MUTATION_QUEUE_CAPACITY: usize = 16;
const GENERATION_DRAIN_GRACE: Duration = Duration::from_secs(10);
const CREDENTIAL_REVOCATION_TIMEOUT: Duration = Duration::from_secs(15);
/// Fixed lease length: no renewal, and an expired lease is stealable by the
/// next `begin`. This only gates the `BeginMutation` → `ApplyMutation` gap;
/// the single-writer task serializes everything else.
const MUTATION_LEASE: Duration = Duration::from_secs(30);

/// A live view of the held slot, mirrored outside the single-writer task so
/// synchronous status reporting can read it without a channel round trip.
#[derive(Clone, Copy)]
struct SlotView {
    id: MutationId,
    deadline_unix_ms: u64,
}

pub(crate) struct MutationManager {
    sender: mpsc::Sender<ManagerCommand>,
    slot_view: Arc<std::sync::Mutex<Option<SlotView>>>,
    stopping: tokio::sync::Mutex<bool>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl MutationManager {
    pub(crate) fn spawn(
        state: Arc<StateStore>,
        host: Arc<HostOnline>,
        serving: Arc<ServingCell>,
    ) -> Arc<Self> {
        Self::spawn_with_lease(state, host, serving, MUTATION_LEASE)
    }

    /// Test-only entry point so lease-expiry tests can inject a short lease
    /// instead of waiting out the real 30-second one. Also the production
    /// path (via `spawn`), which always passes the fixed [`MUTATION_LEASE`].
    pub(crate) fn spawn_with_lease(
        state: Arc<StateStore>,
        host: Arc<HostOnline>,
        serving: Arc<ServingCell>,
        lease: Duration,
    ) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(MUTATION_QUEUE_CAPACITY);
        let refreshes = state.subscribe_credential_refreshes();
        let slot_view = Arc::new(std::sync::Mutex::new(None));
        let task = tokio::spawn(run_manager(
            state,
            host,
            serving,
            lease,
            Arc::clone(&slot_view),
            refreshes,
            receiver,
        ));
        Arc::new(Self {
            sender,
            slot_view,
            stopping: tokio::sync::Mutex::new(false),
            task: tokio::sync::Mutex::new(Some(task)),
        })
    }

    /// The daemon's single mutation lease, when currently held and unexpired.
    /// Reads a mirror updated by the manager task rather than round-tripping
    /// through it, so daemon status can stay a synchronous call.
    pub(crate) fn active_mutation(&self) -> Option<ActiveMutation> {
        let slot = (*self
            .slot_view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))?;
        (unix_millis_now() < slot.deadline_unix_ms).then_some(ActiveMutation {
            mutation_id: slot.id,
            lease_deadline_unix_ms: slot.deadline_unix_ms,
        })
    }

    pub(crate) async fn begin(&self, id: MutationId) -> Result<u64, ManagerError> {
        self.call(|reply| ManagerCommand::Begin { id, reply }).await
    }

    pub(crate) async fn apply(
        &self,
        id: MutationId,
        ops: Vec<MutationOp>,
    ) -> Result<ApplyOutcome, ManagerError> {
        self.call(|reply| ManagerCommand::Apply { id, ops, reply })
            .await
    }

    /// Release the lease if `id` holds it. Idempotent: an id that does not
    /// hold the lease (already dropped, stolen, or never begun) is a no-op.
    pub(crate) async fn drop_mutation(&self, id: MutationId) -> Result<(), ManagerError> {
        self.call(|reply| ManagerCommand::Drop { id, reply }).await
    }

    /// Drive one generation rebuild/activation for a provider artifact a
    /// re-import just repaired, so a mount pinning it recovers immediately
    /// instead of staying degraded until an unrelated mutation or a daemon
    /// restart. Provider import carries no mutation identity of its own, so
    /// this runs through the same single-writer task as `apply` without
    /// requiring the mutation lease.
    pub(crate) async fn rebuild_for_provider_repair(
        &self,
        provider: ProviderId,
    ) -> Result<(), ManagerError> {
        self.call(|reply| ManagerCommand::RebuildForProviderRepair { provider, reply })
            .await
    }

    /// Queue one command and wait for its reply.
    async fn call<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, ManagerError>>) -> ManagerCommand,
    ) -> Result<T, ManagerError> {
        let (reply, receive) = oneshot::channel();
        self.send(command(reply)).await?;
        receive.await.map_err(|_| ManagerError::Stopped)?
    }

    async fn send(&self, command: ManagerCommand) -> Result<(), ManagerError> {
        let stopping = self.stopping.lock().await;
        if *stopping {
            return Err(ManagerError::Stopped);
        }
        self.sender.try_send(command).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => ManagerError::Busy,
            mpsc::error::TrySendError::Closed(_) => ManagerError::Stopped,
        })
    }

    pub(crate) async fn shutdown(&self) -> Result<(), ManagerError> {
        let shutdown = {
            let mut stopping = self.stopping.lock().await;
            if *stopping {
                None
            } else {
                *stopping = true;
                let (reply, receive) = oneshot::channel();
                Some((
                    self.sender.send(ManagerCommand::Shutdown { reply }).await,
                    receive,
                ))
            }
        };
        let command_result = match shutdown {
            Some((Ok(()), receive)) => receive.await.map_err(|_| ManagerError::Stopped),
            Some((Err(_), _)) => Err(ManagerError::Stopped),
            None => Ok(()),
        };
        // Daemon teardown has one shutdown owner. Hold this lock while joining
        // so a concurrent caller cannot consume the handle and return before
        // the owned task outcome is known.
        let mut task = self.task.lock().await;
        let task_result = if let Some(task) = task.take() {
            task.await.map_err(ManagerError::Task)
        } else {
            Ok(())
        };
        command_result?;
        task_result
    }
}

/// Result of a successfully applied batch: one op result per submitted op,
/// in order, plus whether the daemon's serving generation now reflects it.
#[derive(Debug)]
pub(crate) struct ApplyOutcome {
    pub(crate) results: Vec<MutationOpResult>,
    pub(crate) serving: ServingOutcome,
}

enum ManagerCommand {
    Begin {
        id: MutationId,
        reply: oneshot::Sender<Result<u64, ManagerError>>,
    },
    Apply {
        id: MutationId,
        ops: Vec<MutationOp>,
        reply: oneshot::Sender<Result<ApplyOutcome, ManagerError>>,
    },
    Drop {
        id: MutationId,
        reply: oneshot::Sender<Result<(), ManagerError>>,
    },
    RebuildForProviderRepair {
        provider: ProviderId,
        reply: oneshot::Sender<Result<(), ManagerError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

struct MutationSlot {
    id: MutationId,
    deadline: Instant,
    deadline_unix_ms: u64,
}

struct ManagerState {
    state: Arc<StateStore>,
    host: Arc<HostOnline>,
    serving: Arc<ServingCell>,
    stuck: Option<omnifs_engine::RetiredGeneration>,
    lease: Duration,
    slot: Option<MutationSlot>,
    slot_view: Arc<std::sync::Mutex<Option<SlotView>>>,
}

async fn run_manager(
    state: Arc<StateStore>,
    host: Arc<HostOnline>,
    serving: Arc<ServingCell>,
    lease: Duration,
    slot_view: Arc<std::sync::Mutex<Option<SlotView>>>,
    mut refreshes: tokio::sync::watch::Receiver<()>,
    mut receiver: mpsc::Receiver<ManagerCommand>,
) {
    let mut manager = ManagerState {
        state,
        host,
        serving,
        stuck: None,
        lease,
        slot: None,
        slot_view,
    };
    manager.resume_pending_revocations().await;
    loop {
        let command = tokio::select! {
            command = receiver.recv() => {
                let Some(command) = command else {
                    break;
                };
                command
            },
            changed = refreshes.changed() => {
                if changed.is_err() {
                    break;
                }
                if let Err(error) = manager.republish_pending_refreshes().await {
                    tracing::error!(%error, "failed to publish refreshed credential");
                }
                continue;
            },
        };
        if manager.handle_command(command).await {
            break;
        }
    }
}

impl ManagerState {
    async fn handle_command(&mut self, command: ManagerCommand) -> bool {
        match command {
            ManagerCommand::Begin { id, reply } => {
                let _ = reply.send(self.begin(id));
            },
            ManagerCommand::Apply { id, ops, reply } => {
                let result = self.apply(id, ops).await;
                let _ = reply.send(result);
            },
            ManagerCommand::Drop { id, reply } => {
                if self.slot.as_ref().is_some_and(|slot| slot.id == id) {
                    self.set_slot(None);
                }
                let _ = reply.send(Ok(()));
            },
            ManagerCommand::RebuildForProviderRepair { provider, reply } => {
                let result = self.rebuild_for_provider_repair(provider).await;
                let _ = reply.send(result);
            },
            ManagerCommand::Shutdown { reply } => {
                let _ = reply.send(());
                return true;
            },
        }
        false
    }

    /// Publish the slot mirror after every change: begin, release on apply
    /// (success or failure), explicit drop, or steal by a later begin.
    fn set_slot(&mut self, slot: Option<MutationSlot>) {
        *self
            .slot_view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            slot.as_ref().map(|slot| SlotView {
                id: slot.id,
                deadline_unix_ms: slot.deadline_unix_ms,
            });
        self.slot = slot;
    }

    /// Acquire the lease for `id`, or steal it if the current holder's lease
    /// already expired. A held, unexpired lease with a different id rejects.
    fn begin(&mut self, id: MutationId) -> Result<u64, ManagerError> {
        if let Some(slot) = &self.slot
            && Instant::now() < slot.deadline
        {
            return Err(ManagerError::MutationInProgress {
                holder: slot.id,
                deadline_unix_ms: slot.deadline_unix_ms,
            });
        }
        let deadline_unix_ms = unix_millis_now() + duration_millis(self.lease);
        self.set_slot(Some(MutationSlot {
            id,
            deadline: Instant::now() + self.lease,
            deadline_unix_ms,
        }));
        Ok(deadline_unix_ms)
    }

    async fn apply(
        &mut self,
        id: MutationId,
        ops: Vec<MutationOp>,
    ) -> Result<ApplyOutcome, ManagerError> {
        match &self.slot {
            Some(slot) if slot.id == id => {
                if Instant::now() >= slot.deadline {
                    self.set_slot(None);
                    return Err(ManagerError::LeaseExpired(id));
                }
            },
            _ => return Err(ManagerError::LeaseNotHeld(id)),
        }
        // The lease is admitted; the single-writer task means nothing else
        // can run until this returns, so the batch always runs to
        // completion regardless of how long it takes.
        let result = self.apply_admitted(id, ops).await;
        self.set_slot(None);
        result
    }

    async fn apply_admitted(
        &mut self,
        id: MutationId,
        ops: Vec<MutationOp>,
    ) -> Result<ApplyOutcome, ManagerError> {
        self.retry_stuck().await?;
        let mut state_ops = Vec::with_capacity(ops.len());
        for op in ops {
            state_ops.push(self.resolve_op(op).await?);
        }
        let revocations: Vec<CredentialId> = state_ops
            .iter()
            .filter_map(|op| match op {
                StateOp::RevokeCredential { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        let outcomes = self.state.apply_batch(id, state_ops).await?;
        let results = outcomes
            .iter()
            .map(crate::control::mapping::api_mutation_op_result)
            .collect();
        let serving = match self.prepare_and_activate(id).await {
            Ok(()) => ServingOutcome {
                serving: true,
                recovery_detail: None,
            },
            Err(ManagerError::RecoveryRequired(detail)) => ServingOutcome {
                serving: false,
                recovery_detail: Some(detail),
            },
            Err(other) => return Err(other),
        };
        if serving.serving {
            // The generation just rebuilt from durable state, which already
            // excludes credentials this batch moved to `RevocationPending`,
            // so completing the upstream revocation now cannot race a live
            // request still holding the old credential.
            for revoked in revocations {
                if let Err(error) = self
                    .complete_credential_revocation_by_id(revoked.clone(), id)
                    .await
                {
                    tracing::warn!(
                        credential = %revoked,
                        %error,
                        "could not complete credential revocation after batch commit"
                    );
                }
            }
        }
        Ok(ApplyOutcome { results, serving })
    }

    async fn resolve_op(&self, op: MutationOp) -> Result<StateOp, ManagerError> {
        match op {
            MutationOp::CreateMount(definition) => {
                Ok(StateOp::CreateMount(self.mount_document(definition).await?))
            },
            MutationOp::UpdateMount { name, patch } => {
                let current =
                    self.state.get_mount(&name).await?.ok_or_else(|| {
                        ManagerError::Mount(MountWriteError::NotFound(name.clone()))
                    })?;
                let document = patch_mount(&self.state, current.document, patch).await?;
                Ok(StateOp::UpdateMount(document))
            },
            MutationOp::RemoveMount { name } => Ok(StateOp::RemoveMount(name)),
            MutationOp::SubmitCredential(submission) => Ok(StateOp::SubmitCredential(
                prepare_credential_document(&self.state, submission).await?,
            )),
            MutationOp::DeleteCredential(key) => Ok(StateOp::DeleteCredential(credential_id(key)?)),
            MutationOp::RevokeCredential(key) => {
                let id = credential_id(key)?;
                let stored = self.state.get_credential(&id).await?.ok_or_else(|| {
                    ManagerError::Credential(CredentialWriteError::NotFound(id.clone()))
                })?;
                let scopes = credential_scopes(&stored)?;
                Ok(StateOp::RevokeCredential { id, scopes })
            },
        }
    }

    async fn retry_stuck(&mut self) -> Result<(), ManagerError> {
        let Some(generation) = self.stuck.take() else {
            return Ok(());
        };
        match generation.drain(Duration::ZERO).await {
            DrainOutcome::Drained => Ok(()),
            DrainOutcome::Stuck { active, generation } => {
                self.stuck = Some(generation);
                Err(ManagerError::RecoveryRequired(format!(
                    "a retired generation still has {active} request(s)"
                )))
            },
        }
    }

    async fn resume_pending_revocations(&mut self) {
        let pending = match self.state.pending_credential_revocations().await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::error!(%error, "could not load pending credential revocations");
                return;
            },
        };
        for pending in pending {
            let id = pending.credential.summary.id.clone();
            if let Err(error) = self
                .finish_pending_revocation(id.clone(), pending.mutation, pending.credential)
                .await
            {
                tracing::error!(credential = %id, %error, "could not resume credential revocation");
            }
        }
    }

    async fn republish_pending_refreshes(&mut self) -> Result<(), ManagerError> {
        self.retry_stuck().await?;
        let draft = GenerationDraft::load(&self.state).await?;
        if !draft.has_pending_refreshes() {
            return Ok(());
        }
        let build = match draft.prepare(&self.state, &self.host).await {
            Ok(build) => build,
            Err(error) => {
                let detail = format!("could not prepare refreshed credential generation: {error}");
                return Err(self.require_recovery(None, detail).await);
            },
        };
        self.activate(build, None).await
    }

    /// Rebuild and activate the serving generation if, and only if, some
    /// mount pins `provider`. Callers gate on
    /// `ProviderImportDisposition::Repaired` before reaching here (an
    /// `Inserted` artifact cannot yet be pinned by an existing mount, and an
    /// `Unchanged` re-import repaired nothing); this is the narrower check
    /// that a repair some mount does not pin need not pay for a rebuild.
    /// Runs with no mutation identity, mirroring
    /// `republish_pending_refreshes`.
    async fn rebuild_for_provider_repair(
        &mut self,
        provider: ProviderId,
    ) -> Result<(), ManagerError> {
        self.retry_stuck().await?;
        let draft = GenerationDraft::load(&self.state).await?;
        let pinned = draft
            .mounts()
            .iter()
            .any(|mount| mount.document.provider.id == provider);
        if !pinned {
            return Ok(());
        }
        let build = match draft.prepare(&self.state, &self.host).await {
            Ok(build) => build,
            Err(error) => {
                let detail = format!(
                    "a repaired provider artifact could not be prepared into its serving \
                     generation: {error}"
                );
                return Err(self.require_recovery(None, detail).await);
            },
        };
        self.activate(build, None).await
    }

    /// Load the durable serving snapshot, prepare a fresh generation from it,
    /// and activate it. This is the whole of "drive the generation
    /// rebuild/activation exactly once per batch": one load, one prepare, one
    /// activate, regardless of which ops the batch carried.
    async fn prepare_and_activate(&mut self, mutation: MutationId) -> Result<(), ManagerError> {
        let draft = GenerationDraft::load(&self.state).await?;
        let build = match draft.prepare(&self.state, &self.host).await {
            Ok(build) => build,
            Err(error) => {
                let detail = format!(
                    "mutation batch committed, but its serving generation could not be prepared: \
                     {error}"
                );
                return Err(self.require_recovery(Some(mutation), detail).await);
            },
        };
        self.activate(build, Some(mutation)).await
    }

    async fn activate(
        &mut self,
        build: GenerationBuild,
        mutation: Option<MutationId>,
    ) -> Result<(), ManagerError> {
        let GenerationParts {
            ready,
            revision,
            pending_refreshes,
        } = build.into_parts();
        let retired = self.serving.publish(ready);
        match retired.drain(GENERATION_DRAIN_GRACE).await {
            DrainOutcome::Drained => {
                if let Err(error) = pending_refreshes.activate(&self.state).await {
                    let detail = format!(
                        "published generation and drained its predecessor, but could not activate \
                         a refreshed credential: {error}"
                    );
                    return Err(self.require_recovery(mutation, detail).await);
                }
                if let Err(error) = self.state.mark_serving(revision).await {
                    let detail = format!(
                        "published generation and drained its predecessor, but could not record \
                         the published generation as serving: {error}"
                    );
                    return Err(self.require_recovery(mutation, detail).await);
                }
                Ok(())
            },
            DrainOutcome::Stuck { active, generation } => {
                self.stuck = Some(generation);
                let detail = format!(
                    "published generation, but its retired predecessor has {active} stuck request(s)"
                );
                Err(self.require_recovery(mutation, detail).await)
            },
        }
    }

    async fn require_recovery(
        &mut self,
        mutation: Option<MutationId>,
        mut detail: String,
    ) -> ManagerError {
        if let Err(error) = self
            .state
            .mark_recovery_required(mutation, detail.clone())
            .await
        {
            use std::fmt::Write as _;
            let _ = write!(
                detail,
                "; could not persist recovery-required state: {error}"
            );
        }
        ManagerError::RecoveryRequired(detail)
    }

    async fn mount_document(
        &self,
        definition: MountDefinition,
    ) -> Result<MountDocument, ManagerError> {
        let provider = self
            .state
            .load_provider_metadata(definition.provider)
            .await?
            .with_context(|| format!("provider {} is not retained", definition.provider))?;
        let credential = definition
            .auth
            .map(|auth| {
                CredentialId::new(
                    provider.reference.meta.name.to_string(),
                    auth.scheme,
                    auth.account_label,
                )
            })
            .transpose()?;
        Ok(MountDocument {
            name: definition.name,
            provider: provider.reference,
            credential,
            limits: definition.limits.map(|limits| MountLimits {
                max_memory_mb: limits.max_memory_mb,
                max_fetch_blob_bytes: limits.max_fetch_blob_bytes,
            }),
            config: serde_json::from_slice(&definition.config)
                .context("mount config is not valid JSON")?,
        })
    }

    /// Complete a revocation whose `RevocationPending` row this batch (or an
    /// earlier interrupted one, at startup) already committed: contact the
    /// upstream provider if its scheme calls for it, then persist the final
    /// `Deleted`/`RevocationUnknown` outcome. This runs outside the mutation
    /// lease and does not touch the serving generation: `RevocationPending`
    /// credentials are already excluded from it by `GenerationDraft::load`.
    async fn finish_pending_revocation(
        &mut self,
        id: CredentialId,
        mutation: MutationId,
        stored: StoredCredential,
    ) -> anyhow::Result<()> {
        let revocation = prepare_credential_revocation(&self.state, &stored).await?;
        let scopes = credential_scopes(&stored)?;
        self.serving.close_active_admission();
        self.finish_revocation(id, mutation, revocation, scopes)
            .await?;
        Ok(())
    }

    async fn complete_credential_revocation_by_id(
        &mut self,
        id: CredentialId,
        mutation: MutationId,
    ) -> anyhow::Result<()> {
        let Some(stored) = self.state.get_credential(&id).await? else {
            anyhow::bail!("revoked credential vanished before its upstream revoke could run");
        };
        self.finish_pending_revocation(id, mutation, stored).await
    }

    async fn finish_revocation(
        &mut self,
        id: CredentialId,
        mutation: MutationId,
        revocation: PreparedCredentialRevocation,
        scopes: Vec<String>,
    ) -> anyhow::Result<()> {
        let remote = tokio::time::timeout(CREDENTIAL_REVOCATION_TIMEOUT, revocation.revoke()).await;
        let finish = match remote {
            Ok(Ok(())) => CredentialRevocationFinish::Deleted,
            Ok(Err(error)) => {
                tracing::warn!(credential = %id, %error, "credential revocation call failed");
                CredentialRevocationFinish::Unknown
            },
            Err(_) => {
                tracing::warn!(
                    credential = %id,
                    seconds = CREDENTIAL_REVOCATION_TIMEOUT.as_secs(),
                    "credential revocation did not finish within the timeout"
                );
                CredentialRevocationFinish::Unknown
            },
        };
        self.state
            .finish_credential_revocation(id, mutation, finish, scopes)
            .await?;
        Ok(())
    }
}

async fn patch_mount(
    state: &StateStore,
    mut document: MountDocument,
    patch: MountPatch,
) -> Result<MountDocument, ManagerError> {
    if let Some(provider_id) = patch.provider {
        let provider = state
            .load_provider_metadata(provider_id)
            .await?
            .with_context(|| format!("provider {provider_id} is not retained"))?;
        document.provider = provider.reference;
    }
    match patch.auth {
        MountField::Keep => {},
        MountField::Clear => document.credential = None,
        MountField::Set(auth) => {
            document.credential = Some(CredentialId::new(
                document.provider.meta.name.to_string(),
                auth.scheme,
                auth.account_label,
            )?);
        },
    }
    match patch.limits {
        MountField::Keep => {},
        MountField::Clear => document.limits = None,
        MountField::Set(limits) => {
            document.limits = Some(MountLimits {
                max_memory_mb: limits.max_memory_mb,
                max_fetch_blob_bytes: limits.max_fetch_blob_bytes,
            });
        },
    }
    match patch.config {
        MountField::Keep => {},
        MountField::Clear => {
            document.config = serde_json::Value::Object(serde_json::Map::default());
        },
        MountField::Set(config) => {
            document.config =
                serde_json::from_slice(&config).context("mount config is not valid JSON")?;
        },
    }
    if let Some(credential) = &document.credential
        && credential.provider_name() != document.provider.meta.name.as_str()
    {
        return Err(ManagerError::Invalid(
            "provider change requires a matching credential or clearing mount auth".to_owned(),
        ));
    }
    Ok(document)
}

fn credential_id(key: CredentialKey) -> Result<CredentialId, omnifs_auth::CredentialIdError> {
    crate::control::mapping::credential_id(key)
}

fn unix_millis_now() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagerError {
    #[error("mutation manager queue is full")]
    Busy,
    #[error("mutation manager stopped")]
    Stopped,
    #[error("mutation {holder} is already in progress; its lease expires at {deadline_unix_ms}")]
    MutationInProgress {
        holder: MutationId,
        deadline_unix_ms: u64,
    },
    #[error("the mutation lease for {0} expired before its apply arrived")]
    LeaseExpired(MutationId),
    #[error("{0} does not hold the daemon's mutation lease")]
    LeaseNotHeld(MutationId),
    #[error("daemon recovery is required: {0}")]
    RecoveryRequired(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Mount(#[from] MountWriteError),
    #[error(transparent)]
    Credential(#[from] CredentialWriteError),
    #[error(transparent)]
    CredentialId(#[from] omnifs_auth::CredentialIdError),
    #[error(transparent)]
    Batch(#[from] BatchError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
    #[error("mutation manager task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_bootstrap::{Bootstrap, Daemon};
    use omnifs_state::StateStoreOptions;
    use std::time::Duration;

    async fn test_manager(lease: Duration) -> (Arc<MutationManager>, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let endpoint = Bootstrap::<Daemon>::under_root(temp.path());
        let state = Arc::new(
            StateStore::open(&endpoint, StateStoreOptions::default())
                .await
                .unwrap(),
        );
        let paths = state.engine_paths();
        let host = Arc::new(
            omnifs_engine::HostOnline::open_runtime(omnifs_engine::HostRuntimeOpen {
                projection: paths.projection_cache().to_path_buf(),
                wasmtime: paths.wasmtime_cache().to_path_buf(),
                clones: paths.clone_cache().to_path_buf(),
            })
            .unwrap(),
        );
        let draft = GenerationDraft::load(&state).await.unwrap();
        let parts = draft.prepare(&state, &host).await.unwrap().into_parts();
        let serving = ServingCell::new([0; 16], parts.ready);
        parts.pending_refreshes.activate(&state).await.unwrap();
        state.mark_serving(parts.revision).await.unwrap();
        let manager = MutationManager::spawn_with_lease(state, host, serving, lease);
        (manager, temp)
    }

    /// Same setup as [`test_manager`], but also hands back the `StateStore`
    /// and `ServingCell` so a test can commit durable state behind the
    /// manager's back (bypassing `apply`, which always rebuilds) and then
    /// observe whether a later call actually rebuilt the serving generation.
    async fn test_manager_with_state(
        lease: Duration,
    ) -> (
        Arc<MutationManager>,
        Arc<StateStore>,
        Arc<omnifs_engine::ServingCell>,
        tempfile::TempDir,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let endpoint = Bootstrap::<Daemon>::under_root(temp.path());
        let state = Arc::new(
            StateStore::open(&endpoint, StateStoreOptions::default())
                .await
                .unwrap(),
        );
        let paths = state.engine_paths();
        let host = Arc::new(
            omnifs_engine::HostOnline::open_runtime(omnifs_engine::HostRuntimeOpen {
                projection: paths.projection_cache().to_path_buf(),
                wasmtime: paths.wasmtime_cache().to_path_buf(),
                clones: paths.clone_cache().to_path_buf(),
            })
            .unwrap(),
        );
        let draft = GenerationDraft::load(&state).await.unwrap();
        let parts = draft.prepare(&state, &host).await.unwrap().into_parts();
        let serving = ServingCell::new([0; 16], parts.ready);
        parts.pending_refreshes.activate(&state).await.unwrap();
        state.mark_serving(parts.revision).await.unwrap();
        let manager = MutationManager::spawn_with_lease(
            Arc::clone(&state),
            host,
            Arc::clone(&serving),
            lease,
        );
        (manager, state, serving, temp)
    }

    /// A minimal valid provider artifact: real `#[provider]`-macro metadata
    /// encoding is exercised in `control/tests.rs`; this only needs to pass
    /// `ProviderManifest` validation so `import_provider` and, later,
    /// `GenerationDraft::prepare` succeed.
    fn demo_provider_wasm() -> Vec<u8> {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "id": "demo",
            "displayName": "Demo",
            "description": "A test provider",
            "provider": "demo.wasm",
            "defaultMount": "demo",
            "refreshIntervalSecs": 0,
            "capabilities": [{
                "kind": "domain",
                "value": "api.demo.test",
                "why": "Test credential injection."
            }],
            "auth": {
                "default": "pat",
                "schemes": [{
                    "staticToken": {
                        "key": "pat",
                        "valuePrefix": "Bearer ",
                        "description": "Demo token",
                        "injectDomains": ["api.demo.test"]
                    }
                }]
            }
        }))
        .unwrap();
        let name = omnifs_provider::PROVIDER_METADATA_SECTION_NAME.as_bytes();
        let mut payload = Vec::new();
        append_uleb(&mut payload, name.len());
        payload.extend_from_slice(name);
        payload.extend_from_slice(&metadata);

        let mut wasm = b"\0asm\x01\0\0\0".to_vec();
        wasm.push(0);
        append_uleb(&mut wasm, payload.len());
        wasm.extend_from_slice(&payload);
        wasm
    }

    fn append_uleb(output: &mut Vec<u8>, mut value: usize) {
        loop {
            let byte = u8::try_from(value & 0x7f).unwrap();
            value >>= 7;
            if value == 0 {
                output.push(byte);
                return;
            }
            output.push(byte | 0x80);
        }
    }

    #[tokio::test]
    async fn begin_while_held_rejects_with_mutation_in_progress() {
        let (manager, _temp) = test_manager(Duration::from_secs(30)).await;
        let first = MutationId::from_bytes([1; 16]);
        let second = MutationId::from_bytes([2; 16]);
        manager.begin(first).await.unwrap();
        let error = manager.begin(second).await.unwrap_err();
        assert!(matches!(
            error,
            ManagerError::MutationInProgress { holder, .. } if holder == first
        ));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn expired_lease_is_stolen_by_next_begin() {
        let (manager, _temp) = test_manager(Duration::from_millis(10)).await;
        // Pause only after setup: opening the store and preparing the first
        // generation do real async I/O with their own internal timeouts,
        // which a clock paused from the start of the test would derail.
        tokio::time::pause();
        let first = MutationId::from_bytes([1; 16]);
        let second = MutationId::from_bytes([2; 16]);
        manager.begin(first).await.unwrap();
        tokio::time::advance(Duration::from_millis(20)).await;
        manager.begin(second).await.unwrap();
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn apply_after_lease_expiry_rejects_with_lease_expired() {
        let (manager, _temp) = test_manager(Duration::from_millis(10)).await;
        tokio::time::pause();
        let id = MutationId::from_bytes([1; 16]);
        manager.begin(id).await.unwrap();
        tokio::time::advance(Duration::from_millis(20)).await;
        let error = manager.apply(id, Vec::new()).await.unwrap_err();
        assert!(matches!(error, ManagerError::LeaseExpired(expired) if expired == id));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn apply_without_begin_rejects_with_lease_not_held() {
        let (manager, _temp) = test_manager(Duration::from_secs(30)).await;
        let id = MutationId::from_bytes([1; 16]);
        let error = manager.apply(id, Vec::new()).await.unwrap_err();
        assert!(matches!(error, ManagerError::LeaseNotHeld(rejected) if rejected == id));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn drop_mutation_is_idempotent() {
        let (manager, _temp) = test_manager(Duration::from_secs(30)).await;
        let id = MutationId::from_bytes([1; 16]);
        // Dropping a mutation that never began succeeds vacuously.
        manager.drop_mutation(id).await.unwrap();
        manager.begin(id).await.unwrap();
        manager.drop_mutation(id).await.unwrap();
        manager.drop_mutation(id).await.unwrap();
        // The slot is free again: another id can begin immediately.
        manager
            .begin(MutationId::from_bytes([2; 16]))
            .await
            .unwrap();
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn active_mutation_reports_the_held_slot_and_clears_on_drop() {
        let (manager, _temp) = test_manager(Duration::from_secs(30)).await;
        let id = MutationId::from_bytes([1; 16]);
        assert!(manager.active_mutation().is_none());
        manager.begin(id).await.unwrap();
        assert_eq!(manager.active_mutation().unwrap().mutation_id, id);
        manager.drop_mutation(id).await.unwrap();
        assert!(manager.active_mutation().is_none());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn provider_repair_rebuild_only_runs_for_a_provider_a_mount_pins() {
        let (manager, state, serving, _temp) =
            test_manager_with_state(Duration::from_secs(30)).await;
        let bytes = demo_provider_wasm();
        let digest = ProviderId::from_wasm_bytes(&bytes);
        let upload = state
            .stage_provider_bytes("demo.wasm", digest, &bytes)
            .await
            .unwrap();
        let outcome = state.import_provider(upload).await.unwrap();
        assert_eq!(
            outcome.disposition,
            omnifs_state::ProviderImportDisposition::Inserted
        );
        let provider = outcome.reference;

        // Commit a mount pinning this provider directly through the durable
        // write path, bypassing the manager's own `apply` (which always
        // rebuilds after a batch): the serving generation must stay exactly
        // as it was until something explicitly asks for a rebuild, so the
        // two assertions below can tell a real rebuild apart from a no-op.
        let document = MountDocument {
            name: omnifs_core::MountName::new("demo").unwrap(),
            provider: provider.clone(),
            credential: None,
            limits: None,
            config: serde_json::Value::Object(serde_json::Map::default()),
        };
        state
            .apply_batch(
                MutationId::from_bytes([7; 16]),
                vec![StateOp::CreateMount(document)],
            )
            .await
            .unwrap();
        assert!(serving.mount_statuses().is_empty());

        // An unrelated provider: no mount pins it, so this must be a no-op
        // that never reaches `GenerationDraft::prepare` at all, and the
        // durably-committed mount must stay absent from serving.
        let unrelated = ProviderId::from_digest([9; 32]);
        manager
            .rebuild_for_provider_repair(unrelated)
            .await
            .unwrap();
        assert!(serving.mount_statuses().is_empty());

        // The provider the mount actually pins: the rebuild must reach
        // `GenerationDraft::prepare`, which is the same single-writer path
        // `apply` uses. This fixture's wasm bytes are just enough to pass
        // `import_provider`'s metadata validation, not a real component, so
        // `prepare` fails to compile it; that failure is exactly the
        // evidence that (unlike the unrelated-provider call above) this call
        // actually attempted the rebuild instead of returning a no-op.
        let error = manager
            .rebuild_for_provider_repair(provider.id)
            .await
            .unwrap_err();
        assert!(matches!(error, ManagerError::RecoveryRequired(_)));
        assert!(serving.mount_statuses().is_empty());

        manager.shutdown().await.unwrap();
    }
}
