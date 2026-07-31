//! Ordered daemon-owned reconciliation of desired resources into one serving generation.

use crate::generation_builder::{
    GenerationDraft, GenerationParts, RevocationActionOutcome,
    finish_resource_credential_revocation,
};
use crate::progress::ProgressTargetState;
use crate::provider_preparer::{ProviderPreparationJob, ProviderPreparerHandle, ProviderPriority};
use crate::resource_control::ResourceControl;
use anyhow::Context as _;
use omnifs_api::{
    ActionKind, ActionPhase, CredentialProgress, CredentialProgressStage, ProgressEventKind,
    ProgressTarget, ResourceDefinition, ResourcePhase, ServingProgress, ServingProgressStage,
};
use omnifs_core::{ActionId, ProviderId, ResourceKind, ResourceRevision};
use omnifs_engine::{DrainOutcome, HostOnline, RetiredGeneration, ServingCell};
use omnifs_state::StateStore;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{Mutex, watch};
use tokio::task::{JoinHandle, JoinSet};

const GENERATION_DRAIN_GRACE: Duration = Duration::from_secs(10);
const RECONCILE_RETRY_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONCILE_ATTEMPTS: u32 = 3;

/// Sole owner of declarative generation sequencing.
pub(crate) struct ServingReconciler {
    shutdown: watch::Sender<bool>,
    provider_imports: Arc<StdMutex<HashMap<ProviderId, bool>>>,
    provider_import_wakeup: watch::Sender<u64>,
    task: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone, Copy)]
struct ProviderImportWakeup {
    provider_id: ProviderId,
    repaired: bool,
}

struct ReconcileRuntime {
    state: Arc<StateStore>,
    host: Arc<HostOnline>,
    serving: Arc<ServingCell>,
    resources: Arc<ResourceControl>,
    preparer: ProviderPreparerHandle,
}

impl ServingReconciler {
    pub(crate) fn spawn(
        state: Arc<StateStore>,
        host: Arc<HostOnline>,
        serving: Arc<ServingCell>,
        resources: Arc<ResourceControl>,
        preparer: ProviderPreparerHandle,
    ) -> Arc<Self> {
        let (shutdown, shutdown_rx) = watch::channel(false);
        let provider_imports = Arc::new(StdMutex::new(HashMap::new()));
        let (provider_import_wakeup, provider_import_rx) = watch::channel(0);
        let runtime = Arc::new(ReconcileRuntime {
            state,
            host,
            serving,
            resources,
            preparer,
        });
        let task = tokio::spawn(run(
            runtime,
            shutdown_rx,
            Arc::clone(&provider_imports),
            provider_import_rx,
        ));
        Arc::new(Self {
            shutdown,
            provider_imports,
            provider_import_wakeup,
            task: Mutex::new(Some(task)),
        })
    }

    pub(crate) fn provider_imported(&self, provider_id: ProviderId, repaired: bool) {
        coalesce_provider_import(&self.provider_imports, provider_id, repaired);
        self.provider_import_wakeup
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(true);
        let Some(task) = self.task.lock().await.take() else {
            return Ok(());
        };
        task.await.context("join serving reconciler")
    }
}

/// Fold repeated content-addressed imports into one reconciliation wakeup.
/// A repair must survive coalescing so a previously failed digest is retried.
fn coalesce_provider_import(
    imports: &Arc<StdMutex<HashMap<ProviderId, bool>>>,
    provider_id: ProviderId,
    repaired: bool,
) {
    let mut pending = imports
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pending
        .entry(provider_id)
        .and_modify(|was_repaired| *was_repaired |= repaired)
        .or_insert(repaired);
}

async fn run(
    runtime: Arc<ReconcileRuntime>,
    mut shutdown: watch::Receiver<bool>,
    provider_imports: Arc<StdMutex<HashMap<ProviderId, bool>>>,
    mut provider_import_wakeup: watch::Receiver<u64>,
) {
    let mut revisions = runtime.resources.subscribe_revisions();
    let mut actions = runtime.resources.subscribe_actions();
    let mut refreshes = runtime.state.subscribe_credential_refreshes();
    let mut drain_tasks = JoinSet::new();
    let mut reconcile_now = true;
    loop {
        if *shutdown.borrow() {
            break;
        }
        if reconcile_now {
            reconcile_now =
                match reconcile_latest(&runtime, &mut revisions, &mut shutdown, &mut drain_tasks)
                    .await
                {
                    ReconcileLoopOutcome::Settled => false,
                    ReconcileLoopOutcome::Superseded => true,
                    ReconcileLoopOutcome::Shutdown => break,
                };
            if reconcile_now {
                continue;
            }
        }
        tokio::select! {
            changed = revisions.changed() => {
                if changed.is_err() {
                    break;
                }
                let revision = *revisions.borrow_and_update();
                reconcile_now = revision_requires_reconcile(&runtime.resources, revision);
            },
            changed = actions.changed() => {
                if changed.is_err() {
                    break;
                }
                reconcile_now = true;
            },
            changed = refreshes.changed() => {
                if changed.is_err() {
                    break;
                }
                reconcile_now = true;
            },
            changed = provider_import_wakeup.changed() => {
                if changed.is_err() {
                    break;
                }
                let pending = {
                    let mut pending = provider_imports
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    std::mem::take(&mut *pending)
                };
                for (provider_id, repaired) in pending {
                    let imported = ProviderImportWakeup { provider_id, repaired };
                    match enqueue_imported_provider(&runtime.state, &runtime.preparer, imported).await {
                        Ok(used_by_desired) => reconcile_now |= used_by_desired,
                        Err(error) => {
                            tracing::warn!(
                                provider = %imported.provider_id,
                                %error,
                                "could not enqueue imported provider"
                            );
                        },
                    }
                }
            },
            joined = drain_tasks.join_next(), if !drain_tasks.is_empty() => {
                if let Some(Err(error)) = joined {
                    tracing::warn!(%error, "retired generation drain task failed");
                }
            },
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            },
        }
    }
    drain_tasks.abort_all();
    while drain_tasks.join_next().await.is_some() {}
}

fn revision_requires_reconcile(resources: &ResourceControl, revision: ResourceRevision) -> bool {
    !matches!(
        resources
            .progress()
            .target_state(ProgressTarget::DesiredRevision(revision)),
        ProgressTargetState::Ready | ProgressTargetState::Failed
    )
}

async fn enqueue_imported_provider(
    state: &StateStore,
    preparer: &ProviderPreparerHandle,
    imported: ProviderImportWakeup,
) -> anyhow::Result<bool> {
    let stored = state
        .load_provider(imported.provider_id)
        .await?
        .with_context(|| format!("imported provider {} disappeared", imported.provider_id))?;
    let job = ProviderPreparationJob::new(
        stored.reference.id,
        stored.reference.meta.name.to_string(),
        Vec::new(),
        stored.bytes,
    )?;
    if imported.repaired {
        preparer
            .requeue_repaired(job, ProviderPriority::Retained)
            .await?;
    } else {
        preparer.enqueue(job, ProviderPriority::Retained).await?;
    }
    provider_is_mounted(state, imported.provider_id).await
}

async fn provider_is_mounted(state: &StateStore, provider_id: ProviderId) -> anyhow::Result<bool> {
    let desired = state.resource_snapshot().await?;
    let aliases: BTreeSet<_> = desired
        .resources
        .resources()
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Provider(provider) if provider.artifact == provider_id => {
                Some(provider.name.clone())
            },
            _ => None,
        })
        .collect();
    Ok(desired.resources.resources().iter().any(|resource| {
        matches!(resource, ResourceDefinition::Mount(mount) if aliases.contains(&mount.provider))
    }))
}

async fn reconcile_latest(
    runtime: &ReconcileRuntime,
    revisions: &mut watch::Receiver<ResourceRevision>,
    shutdown: &mut watch::Receiver<bool>,
    drain_tasks: &mut JoinSet<()>,
) -> ReconcileLoopOutcome {
    let desired = match runtime.state.resource_snapshot().await {
        Ok(desired) => desired,
        Err(error) => {
            let failure = ReconcileFailure::new(error);
            publish_failure(&runtime.resources, None, &[], &failure);
            return ReconcileLoopOutcome::Settled;
        },
    };
    let revision = desired.revision;
    revisions.borrow_and_update();
    let actions = match start_pending_actions(&runtime.state, &runtime.resources).await {
        Ok(actions) => actions,
        Err(error) => {
            let failure = ReconcileFailure::new(error);
            publish_failure(&runtime.resources, Some(&desired), &[], &failure);
            return ReconcileLoopOutcome::Settled;
        },
    };
    let mut attempt = 0;
    loop {
        attempt += 1;
        match reconcile_once(runtime, &desired, &actions, attempt, revisions, shutdown).await {
            Ok(ReconcileOutcome::Published(published)) => {
                finish_publication(
                    &runtime.state,
                    &runtime.resources,
                    revision,
                    attempt,
                    *published,
                    &actions,
                    drain_tasks,
                )
                .await;
                return ReconcileLoopOutcome::Settled;
            },
            Ok(ReconcileOutcome::Superseded) => return ReconcileLoopOutcome::Superseded,
            Ok(ReconcileOutcome::Shutdown) => return ReconcileLoopOutcome::Shutdown,
            Err(error) if attempt < MAX_RECONCILE_ATTEMPTS => {
                publish_retry(&runtime.resources, revision, &actions, attempt, &error);
                tokio::select! {
                    () = tokio::time::sleep(RECONCILE_RETRY_DELAY) => {},
                    interrupt = reconcile_interrupt(revisions, shutdown, revision) => {
                        return match interrupt {
                            ReconcileInterrupt::Superseded(latest) => {
                                publish_superseded(&runtime.resources, revision, &actions, latest, attempt);
                                ReconcileLoopOutcome::Superseded
                            },
                            ReconcileInterrupt::Shutdown => ReconcileLoopOutcome::Shutdown,
                        };
                    },
                }
            },
            Err(error) => {
                publish_failure(&runtime.resources, Some(&desired), &actions, &error);
                fail_actions(&runtime.state, &runtime.resources, &actions, &error).await;
                return ReconcileLoopOutcome::Settled;
            },
        }
    }
}

enum ReconcileLoopOutcome {
    Settled,
    Superseded,
    Shutdown,
}

enum ReconcileOutcome {
    Published(Box<PublishedGeneration>),
    Superseded,
    Shutdown,
}

struct PublishedGeneration {
    retired: RetiredGeneration,
    mount_revision: omnifs_core::ResourceRevision,
    pending_refreshes: crate::generation_builder::PendingRefreshes,
}

struct ReconcileFailure {
    provider: Option<ProviderId>,
    source: anyhow::Error,
}

impl ReconcileFailure {
    fn new(source: impl Into<anyhow::Error>) -> Self {
        Self {
            provider: None,
            source: source.into(),
        }
    }

    fn provider(provider: ProviderId, source: impl Into<anyhow::Error>) -> Self {
        Self {
            provider: Some(provider),
            source: source.into(),
        }
    }
}

impl std::fmt::Display for ReconcileFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source, formatter)
    }
}

#[derive(Clone, Copy)]
enum ReconcileInterrupt {
    Superseded(ResourceRevision),
    Shutdown,
}

#[allow(clippy::too_many_lines)] // the ordered prepare, build, fence, and publish path
async fn reconcile_once(
    runtime: &ReconcileRuntime,
    desired: &omnifs_state::ResourceSnapshot,
    actions: &[ActionId],
    retry_count: u32,
    revisions: &mut watch::Receiver<ResourceRevision>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<ReconcileOutcome, ReconcileFailure> {
    let revision = desired.revision;
    mark_resources_preparing(&runtime.resources, revision);
    publish_serving_targets(
        &runtime.resources,
        revision,
        actions,
        ServingProgressStage::Queued,
        0,
        1,
        retry_count.saturating_sub(1),
        None,
    );

    let required =
        enqueue_required_providers(&runtime.state, &runtime.preparer, desired, retry_count > 1)
            .await
            .map_err(ReconcileFailure::new)?;
    publish_serving_targets(
        &runtime.resources,
        revision,
        actions,
        ServingProgressStage::WaitingProviders,
        0,
        u32::try_from(required.len()).unwrap_or(u32::MAX),
        retry_count.saturating_sub(1),
        None,
    );
    for provider in &required {
        tokio::select! {
            interrupt = reconcile_interrupt(revisions, shutdown, revision) => {
                return Ok(interrupted_outcome(
                    &runtime.resources,
                    revision,
                    actions,
                    retry_count,
                    interrupt,
                ));
            },
            result = runtime.preparer.wait_ready(*provider) => {
                result
                    .with_context(|| format!("prepare required provider {provider}"))
                    .map_err(|error| ReconcileFailure::provider(*provider, error))?;
            },
        }
    }
    publish_serving_targets(
        &runtime.resources,
        revision,
        actions,
        ServingProgressStage::ProvidersReady,
        u32::try_from(required.len()).unwrap_or(u32::MAX),
        u32::try_from(required.len()).unwrap_or(u32::MAX),
        retry_count.saturating_sub(1),
        None,
    );

    publish_serving_targets(
        &runtime.resources,
        revision,
        actions,
        ServingProgressStage::Building,
        0,
        1,
        retry_count.saturating_sub(1),
        None,
    );
    let build = tokio::select! {
        interrupt = reconcile_interrupt(revisions, shutdown, revision) => {
            return Ok(interrupted_outcome(
                &runtime.resources,
                revision,
                actions,
                retry_count,
                interrupt,
            ));
        },
        result = async {
            GenerationDraft::load_resources(&runtime.state)
                .await?
                .prepare(&runtime.state, &runtime.host)
                .await
        } => result.map_err(ReconcileFailure::new)?,
    };
    let GenerationParts {
        ready,
        revision: mount_revision,
        pending_refreshes,
    } = build.into_parts();
    publish_serving_targets(
        &runtime.resources,
        revision,
        actions,
        ServingProgressStage::Built,
        1,
        1,
        retry_count.saturating_sub(1),
        None,
    );
    let publication_fence = runtime.resources.publication_fence();
    let _publication_guard = publication_fence.lock().await;
    let latest = runtime
        .state
        .resource_snapshot()
        .await
        .map_err(ReconcileFailure::new)?
        .revision;
    if latest != revision {
        publish_superseded(&runtime.resources, revision, actions, latest, retry_count);
        return Ok(ReconcileOutcome::Superseded);
    }

    publish_serving_targets(
        &runtime.resources,
        revision,
        actions,
        ServingProgressStage::Publishing,
        0,
        1,
        retry_count.saturating_sub(1),
        None,
    );
    if pending_revoke_exists(&runtime.state, actions)
        .await
        .map_err(ReconcileFailure::new)?
    {
        runtime.serving.close_active_admission();
    }
    let retired = runtime.serving.publish(ready);
    Ok(ReconcileOutcome::Published(Box::new(PublishedGeneration {
        retired,
        mount_revision,
        pending_refreshes,
    })))
}

async fn reconcile_interrupt(
    revisions: &mut watch::Receiver<ResourceRevision>,
    shutdown: &mut watch::Receiver<bool>,
    revision: ResourceRevision,
) -> ReconcileInterrupt {
    loop {
        tokio::select! {
            changed = revisions.changed() => {
                if changed.is_err() {
                    return ReconcileInterrupt::Shutdown;
                }
                let latest = *revisions.borrow_and_update();
                if latest != revision {
                    return ReconcileInterrupt::Superseded(latest);
                }
            },
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return ReconcileInterrupt::Shutdown;
                }
            },
        }
    }
}

fn interrupted_outcome(
    resources: &ResourceControl,
    revision: ResourceRevision,
    actions: &[ActionId],
    retry_count: u32,
    interrupt: ReconcileInterrupt,
) -> ReconcileOutcome {
    match interrupt {
        ReconcileInterrupt::Superseded(latest) => {
            publish_superseded(resources, revision, actions, latest, retry_count);
            ReconcileOutcome::Superseded
        },
        ReconcileInterrupt::Shutdown => ReconcileOutcome::Shutdown,
    }
}

fn publish_superseded(
    resources: &ResourceControl,
    revision: ResourceRevision,
    actions: &[ActionId],
    latest: ResourceRevision,
    retry_count: u32,
) {
    publish_serving_targets(
        resources,
        revision,
        actions,
        ServingProgressStage::Superseded,
        0,
        1,
        retry_count.saturating_sub(1),
        Some(format!("superseded by desired revision {latest}")),
    );
    resources.progress().publish(
        ProgressTarget::DesiredRevision(revision),
        ProgressEventKind::RevisionSuperseded {
            revision,
            replaced_by: latest,
        },
    );
}

async fn finish_publication(
    state: &Arc<StateStore>,
    resources: &Arc<ResourceControl>,
    revision: ResourceRevision,
    retry_count: u32,
    published: PublishedGeneration,
    actions: &[ActionId],
    drain_tasks: &mut JoinSet<()>,
) {
    let PublishedGeneration {
        retired,
        mount_revision,
        pending_refreshes,
    } = published;
    publish_serving_targets(
        resources,
        revision,
        actions,
        ServingProgressStage::Draining,
        0,
        1,
        retry_count.saturating_sub(1),
        None,
    );

    let mut post_publish_errors = Vec::new();
    if let Err(error) = pending_refreshes.activate(state).await {
        tracing::warn!(revision = revision.get(), %error, "published credential refresh activation failed");
        post_publish_errors.push(format!("credential refresh activation failed: {error:#}"));
    }
    let revoke_actions =
        complete_actions_after_publish(state, resources, actions, &mut post_publish_errors).await;
    if let Err(error) = state.mark_serving(mount_revision).await {
        tracing::warn!(revision = revision.get(), %error, "published serving revision could not be recorded");
        post_publish_errors.push(format!("serving revision record failed: {error:#}"));
    }

    if !post_publish_errors.is_empty() {
        publish_serving_targets(
            resources,
            revision,
            &revoke_actions,
            ServingProgressStage::Degraded,
            1,
            1,
            retry_count.saturating_sub(1),
            Some(post_publish_errors.join("; ")),
        );
    }
    let has_attachment_work = resources.mark_namespace_ready(revision);
    if !has_attachment_work {
        resources.progress().publish(
            ProgressTarget::DesiredRevision(revision),
            ProgressEventKind::RevisionReady(revision),
        );
    }

    let state = Arc::clone(state);
    let resources = Arc::clone(resources);
    drain_tasks.spawn(async move {
        drain_retired_generation(
            retired,
            state,
            resources,
            revision,
            retry_count,
            revoke_actions,
            !post_publish_errors.is_empty(),
        )
        .await;
    });
}

async fn complete_actions_after_publish(
    state: &StateStore,
    resources: &ResourceControl,
    actions: &[ActionId],
    errors: &mut Vec<String>,
) -> Vec<ActionId> {
    let mut revoke_actions = Vec::new();
    for action_id in actions {
        let receipt = match state.action_receipt(*action_id).await {
            Ok(Some(receipt)) => receipt,
            Ok(None) => continue,
            Err(error) => {
                errors.push(format!("read action {action_id}: {error:#}"));
                continue;
            },
        };
        if receipt.kind == ActionKind::RevokeCredential {
            revoke_actions.push(*action_id);
            continue;
        }
        if receipt.kind == ActionKind::RestartAttachment {
            continue;
        }
        match state
            .transition_action(*action_id, ActionPhase::Ready, None, None)
            .await
        {
            Ok(ready) => publish_completed_action(resources, ready),
            Err(error) => errors.push(format!("complete action {action_id}: {error:#}")),
        }
    }
    revoke_actions
}

async fn drain_retired_generation(
    mut retired: RetiredGeneration,
    state: Arc<StateStore>,
    resources: Arc<ResourceControl>,
    revision: ResourceRevision,
    retry_count: u32,
    revoke_actions: Vec<ActionId>,
    post_publish_degraded: bool,
) {
    loop {
        match retired.drain(GENERATION_DRAIN_GRACE).await {
            DrainOutcome::Drained => {
                complete_revocations_after_drain(&state, &resources, &revoke_actions).await;
                if !post_publish_degraded {
                    publish_background_serving(
                        &resources,
                        revision,
                        &revoke_actions,
                        ServingProgressStage::Ready,
                        retry_count.saturating_sub(1),
                        None,
                    );
                }
                return;
            },
            DrainOutcome::Stuck { active, generation } => {
                publish_background_serving(
                    &resources,
                    revision,
                    &revoke_actions,
                    ServingProgressStage::Degraded,
                    retry_count.saturating_sub(1),
                    Some(format!(
                        "retired generation still has {active} active request(s); drain will retry"
                    )),
                );
                retired = generation;
                tokio::time::sleep(RECONCILE_RETRY_DELAY).await;
            },
        }
    }
}

fn publish_background_serving(
    resources: &ResourceControl,
    revision: ResourceRevision,
    actions: &[ActionId],
    stage: ServingProgressStage,
    retry_count: u32,
    detail: Option<String>,
) {
    let progress = serving_progress(revision, stage, 1, 1, retry_count, detail);
    resources
        .progress()
        .record_serving_for_revision(revision, progress.clone());
    for action in actions {
        resources
            .progress()
            .record_action_serving(*action, progress.clone());
    }
}

async fn enqueue_required_providers(
    state: &StateStore,
    preparer: &ProviderPreparerHandle,
    desired: &omnifs_state::ResourceSnapshot,
    retry_failed: bool,
) -> anyhow::Result<Vec<ProviderId>> {
    let providers: BTreeMap<_, _> = desired
        .resources
        .resources()
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Provider(provider) => {
                Some((provider.name.clone(), provider.artifact))
            },
            _ => None,
        })
        .collect();
    let mounted_names: BTreeSet<_> = desired
        .resources
        .resources()
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Mount(mount) => Some(mount.provider.clone()),
            _ => None,
        })
        .collect();
    let mut aliases = HashMap::<ProviderId, Vec<_>>::new();
    for name in mounted_names {
        let provider = providers
            .get(&name)
            .copied()
            .with_context(|| format!("mounted provider resource `{name}` is absent"))?;
        aliases.entry(provider).or_default().push(name);
    }
    for (provider_id, resource_names) in &aliases {
        let stored = state
            .load_provider(*provider_id)
            .await?
            .with_context(|| format!("provider {provider_id} is not retained"))?;
        let job = ProviderPreparationJob::new(
            *provider_id,
            stored.reference.meta.name.to_string(),
            resource_names.clone(),
            stored.bytes,
        )?;
        if retry_failed
            && preparer.status(*provider_id).is_some_and(|status| {
                status.phase == crate::provider_preparer::ProviderPreparationPhase::Failed
            })
        {
            preparer
                .requeue_repaired(job, ProviderPriority::Desired)
                .await?;
        } else {
            preparer.enqueue(job, ProviderPriority::Desired).await?;
        }
    }
    let mut providers: Vec<_> = aliases.into_keys().collect();
    providers.sort_by_key(|provider| *provider.as_bytes());
    Ok(providers)
}

async fn start_pending_actions(
    state: &StateStore,
    resources: &ResourceControl,
) -> anyhow::Result<Vec<ActionId>> {
    let pending = state.pending_actions().await?;
    let mut actions = Vec::with_capacity(pending.len());
    for receipt in pending {
        let Some(action_stage) = pending_credential_action_stage(receipt.kind) else {
            continue;
        };
        let receipt = if receipt.phase == ActionPhase::Accepted {
            state
                .transition_action(receipt.action_id, ActionPhase::Running, None, None)
                .await?
        } else {
            receipt
        };
        resources.publish_action(&receipt);
        resources.progress().record_credential(
            ProgressTarget::Action(receipt.action_id),
            CredentialProgress {
                key: receipt.target.clone(),
                stage: action_stage,
                error_code: None,
                detail: None,
            },
        );
        actions.push(receipt.action_id);
    }
    Ok(actions)
}

/// Reconstruct the non-secret action stage after daemon restart. Attachments
/// have their own reconciler and intentionally do not enter this generation
/// owner.
const fn pending_credential_action_stage(kind: ActionKind) -> Option<CredentialProgressStage> {
    match kind {
        ActionKind::SetCredentialMaterial => Some(CredentialProgressStage::Refreshing),
        ActionKind::RevokeCredential => Some(CredentialProgressStage::Revoking),
        ActionKind::RestartAttachment => None,
    }
}

async fn complete_revocations_after_drain(
    state: &StateStore,
    resources: &ResourceControl,
    actions: &[ActionId],
) {
    for action_id in actions {
        let Ok(Some(receipt)) = state.action_receipt(*action_id).await else {
            continue;
        };
        let outcome =
            finish_resource_credential_revocation(state, &receipt.target.name, receipt.action_id)
                .await;
        match outcome {
            Ok(RevocationActionOutcome::Deleted) => {
                match state
                    .transition_action(*action_id, ActionPhase::Ready, None, None)
                    .await
                {
                    Ok(ready) => publish_completed_action(resources, ready),
                    Err(error) => tracing::warn!(
                        action = %action_id,
                        %error,
                        "could not complete drained credential action"
                    ),
                }
            },
            Ok(RevocationActionOutcome::Unknown) => {
                publish_failed_action(
                    state,
                    resources,
                    *action_id,
                    "credential_revocation_unknown",
                    "upstream credential revocation outcome is unknown",
                )
                .await;
            },
            Err(error) => {
                publish_failed_action(
                    state,
                    resources,
                    *action_id,
                    "credential_revocation_failed",
                    &format!("{error:#}"),
                )
                .await;
            },
        }
    }
}

fn publish_completed_action(resources: &ResourceControl, ready: omnifs_api::ActionReceipt) {
    let action_id = ready.action_id;
    resources.progress().record_credential(
        ProgressTarget::Action(action_id),
        CredentialProgress {
            key: ready.target.clone(),
            stage: CredentialProgressStage::Ready,
            error_code: None,
            detail: None,
        },
    );
    resources.publish_action(&ready);
    resources.progress().publish(
        ProgressTarget::Action(action_id),
        ProgressEventKind::ActionCompleted(ready),
    );
}

async fn publish_failed_action(
    state: &StateStore,
    resources: &ResourceControl,
    action_id: ActionId,
    error_code: &str,
    detail: &str,
) {
    let Ok(failed) = state
        .transition_action(
            action_id,
            ActionPhase::Failed,
            Some(error_code.into()),
            Some(detail.into()),
        )
        .await
    else {
        return;
    };
    resources.progress().record_credential(
        ProgressTarget::Action(action_id),
        CredentialProgress {
            key: failed.target.clone(),
            stage: CredentialProgressStage::Failed,
            error_code: Some(error_code.into()),
            detail: Some(detail.into()),
        },
    );
    resources.publish_action(&failed);
    resources.progress().publish(
        ProgressTarget::Action(action_id),
        ProgressEventKind::ActionFailed {
            receipt: failed,
            error_code: error_code.into(),
            detail: detail.into(),
        },
    );
}

async fn pending_revoke_exists(state: &StateStore, actions: &[ActionId]) -> anyhow::Result<bool> {
    for action in actions {
        if state
            .action_receipt(*action)
            .await?
            .is_some_and(|receipt| receipt.kind == ActionKind::RevokeCredential)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn mark_resources_preparing(resources: &ResourceControl, revision: ResourceRevision) {
    resources
        .progress()
        .update_revision_snapshot(revision, |snapshot| {
            // A persisted serving revision describes the last daemon's
            // publication. Once this daemon starts rebuilding the generation,
            // the current revision is not terminal until publication is
            // proved again. This also keeps an unchanged apply attached to
            // real startup work instead of completing from stale observation.
            snapshot.observed_revision = None;
            for status in &mut snapshot.resources {
                if status.desired_revision != revision {
                    continue;
                }
                status.phase = match status.key.kind {
                    ResourceKind::Attachment => ResourcePhase::Pending,
                    ResourceKind::Provider | ResourceKind::Credential | ResourceKind::Mount => {
                        ResourcePhase::Preparing
                    },
                };
                status.observed_revision = None;
                status.error_code = None;
                status.detail = None;
            }
        });
}

#[allow(clippy::too_many_arguments)] // closed progress fields stay visible at each transition
fn publish_serving_targets(
    resources: &ResourceControl,
    revision: ResourceRevision,
    actions: &[ActionId],
    stage: ServingProgressStage,
    completed: u32,
    total: u32,
    retry_count: u32,
    detail: Option<String>,
) {
    let progress = serving_progress(revision, stage, completed, total, retry_count, detail);
    resources
        .progress()
        .record_serving_for_revision(revision, progress.clone());
    for action in actions {
        resources
            .progress()
            .record_action_serving(*action, progress.clone());
    }
}

fn serving_progress(
    revision: ResourceRevision,
    stage: ServingProgressStage,
    completed: u32,
    total: u32,
    retry_count: u32,
    detail: Option<String>,
) -> ServingProgress {
    ServingProgress {
        revision,
        stage,
        completed,
        total,
        error_code: None,
        detail,
        queued_generations: u32::from(stage == ServingProgressStage::Queued),
        retry_count,
        next_retry_unix_ms: (stage == ServingProgressStage::Retrying).then(next_retry_unix_ms),
    }
}

fn next_retry_unix_ms() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from((now + RECONCILE_RETRY_DELAY).as_millis()).unwrap_or(u64::MAX)
}

fn publish_retry(
    resources: &ResourceControl,
    revision: ResourceRevision,
    actions: &[ActionId],
    attempt: u32,
    error: &ReconcileFailure,
) {
    tracing::warn!(
        revision = revision.get(),
        attempt,
        error = %error.source,
        "serving reconciliation will retry"
    );
    publish_serving_targets(
        resources,
        revision,
        actions,
        ServingProgressStage::Retrying,
        0,
        1,
        attempt,
        Some(format!(
            "retrying after reconcile failure: {:#}",
            error.source
        )),
    );
}

fn publish_failure(
    resources: &ResourceControl,
    desired: Option<&omnifs_state::ResourceSnapshot>,
    actions: &[ActionId],
    error: &ReconcileFailure,
) {
    let revision = desired.map_or_else(
        || {
            resources
                .progress()
                .snapshot_for(ProgressTarget::Current)
                .1
                .desired_revision
        },
        |desired| desired.revision,
    );
    let detail = format!("{:#}", error.source);
    tracing::warn!(
        revision = revision.get(),
        provider = error.provider.map(|provider| provider.to_string()),
        error = %error.source,
        "serving reconciliation reached a stable failure"
    );
    let mut failed_provider_names = BTreeSet::new();
    let mut failed_credential_names = BTreeSet::new();
    let mut failed_mount_names = BTreeSet::new();
    if let (Some(desired), Some(provider_id)) = (desired, error.provider) {
        for resource in desired.resources.resources() {
            if let ResourceDefinition::Provider(provider) = resource
                && provider.artifact == provider_id
            {
                failed_provider_names.insert(provider.name.clone());
            }
        }
        for resource in desired.resources.resources() {
            if let ResourceDefinition::Credential(credential) = resource
                && failed_provider_names.contains(&credential.provider)
            {
                failed_credential_names.insert(credential.name.clone());
            }
        }
        for resource in desired.resources.resources() {
            if let ResourceDefinition::Mount(mount) = resource
                && (failed_provider_names.contains(&mount.provider)
                    || mount
                        .credential
                        .as_ref()
                        .is_some_and(|name| failed_credential_names.contains(name)))
            {
                failed_mount_names.insert(mount.name.clone());
            }
        }
    }
    resources
        .progress()
        .update_revision_snapshot(revision, |snapshot| {
            for status in &mut snapshot.resources {
                if status.desired_revision != revision {
                    continue;
                }
                let failed = match (error.provider, status.key.kind) {
                    (_, ResourceKind::Attachment) | (None, ResourceKind::Provider) => false,
                    (Some(_), ResourceKind::Provider) => {
                        failed_provider_names.contains(&status.key.name)
                    },
                    (Some(_), ResourceKind::Credential) => {
                        failed_credential_names.contains(&status.key.name)
                    },
                    (Some(_), ResourceKind::Mount) => failed_mount_names.contains(&status.key.name),
                    (None, ResourceKind::Credential | ResourceKind::Mount) => true,
                };
                if failed {
                    status.phase = ResourcePhase::Failed;
                    status.error_code = Some(if error.provider.is_some() {
                        "provider_preparation_failed".into()
                    } else {
                        "serving_reconcile_failed".into()
                    });
                    status.detail = Some(detail.clone());
                } else if status.key.kind == ResourceKind::Provider {
                    status.phase = ResourcePhase::Ready;
                    status.error_code = None;
                    status.detail = None;
                }
            }
        });
    publish_serving_targets(
        resources,
        revision,
        actions,
        ServingProgressStage::Failed,
        0,
        1,
        MAX_RECONCILE_ATTEMPTS,
        Some(detail.clone()),
    );
    resources.progress().publish(
        ProgressTarget::DesiredRevision(revision),
        ProgressEventKind::RevisionFailed {
            revision,
            error_code: "serving_reconcile_failed".into(),
            detail,
        },
    );
}

async fn fail_actions(
    state: &StateStore,
    resources: &ResourceControl,
    actions: &[ActionId],
    error: &ReconcileFailure,
) {
    let detail = format!("{:#}", error.source);
    for action_id in actions {
        publish_failed_action(
            state,
            resources,
            *action_id,
            "serving_reconcile_failed",
            &detail,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::{ActionPhase, ProgressSnapshot, ResourceStatus};
    use omnifs_core::{ResourceKey, ResourceKind, ResourceName};
    use omnifs_state::StateStoreOptions;

    fn name(value: &str) -> ResourceName {
        ResourceName::new(value).unwrap()
    }

    fn receipt(
        action_id: ActionId,
        kind: ActionKind,
        phase: ActionPhase,
    ) -> omnifs_api::ActionReceipt {
        omnifs_api::ActionReceipt {
            action_id,
            kind,
            target: ResourceKey::new(ResourceKind::Credential, name("account")),
            action_generation: 1,
            phase,
            error_code: None,
            detail: None,
        }
    }

    fn snapshot(revision: u64, observed: Option<u64>) -> ProgressSnapshot {
        ProgressSnapshot {
            desired_revision: ResourceRevision::new(revision),
            observed_revision: observed.map(ResourceRevision::new),
            resources: vec![ResourceStatus {
                key: ResourceKey::new(ResourceKind::Mount, name("mount")),
                desired_revision: ResourceRevision::new(revision),
                observed_revision: observed.map(ResourceRevision::new),
                phase: ResourcePhase::Preparing,
                error_code: None,
                detail: None,
            }],
            actions: Vec::new(),
            providers: Vec::new(),
            serving: None,
            credentials: Vec::new(),
            attachments: Vec::new(),
        }
    }

    async fn fixture(
        snapshot: ProgressSnapshot,
    ) -> (tempfile::TempDir, Arc<StateStore>, Arc<ResourceControl>) {
        let temp = tempfile::tempdir().unwrap();
        let paths = omnifs_state::DaemonStatePaths::new(temp.path().join("daemon-state"));
        let state = Arc::new(
            StateStore::open(paths, StateStoreOptions::default())
                .await
                .unwrap(),
        );
        let control = ResourceControl::new(Arc::clone(&state), "reconciler-test")
            .await
            .unwrap();
        control
            .progress()
            .publish_snapshot(ProgressTarget::Current, snapshot);
        (temp, state, control)
    }

    #[test]
    fn coalesced_imports_keep_the_repair_bit_for_one_digest() {
        let id = ProviderId::from_digest([7; 32]);
        let imports = Arc::new(StdMutex::new(HashMap::new()));
        coalesce_provider_import(&imports, id, false);
        coalesce_provider_import(&imports, id, true);
        coalesce_provider_import(&imports, id, false);

        let pending = imports.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending.get(&id), Some(&true));
    }

    #[test]
    fn pending_credential_actions_recover_their_safe_stage_by_kind() {
        assert_eq!(
            pending_credential_action_stage(ActionKind::SetCredentialMaterial),
            Some(CredentialProgressStage::Refreshing)
        );
        assert_eq!(
            pending_credential_action_stage(ActionKind::RevokeCredential),
            Some(CredentialProgressStage::Revoking)
        );
        assert_eq!(
            pending_credential_action_stage(ActionKind::RestartAttachment),
            None
        );
    }

    #[tokio::test]
    async fn stale_revision_is_terminal_without_replacing_the_last_good_snapshot() {
        let (_temp, state, resources) = fixture(snapshot(2, Some(1))).await;
        let mut stream = resources
            .progress()
            .subscribe(ProgressTarget::DesiredRevision(ResourceRevision::new(2)));
        let _ = stream.recv().await.unwrap();

        publish_superseded(
            &resources,
            ResourceRevision::new(2),
            &[],
            ResourceRevision::new(3),
            1,
        );

        assert_eq!(
            resources
                .progress()
                .target_state(ProgressTarget::DesiredRevision(ResourceRevision::new(2))),
            crate::progress::ProgressTargetState::Watching
        );
        let (_, current) = resources.progress().snapshot_for(ProgressTarget::Current);
        assert_eq!(current.observed_revision, Some(ResourceRevision::new(1)));
        assert!(matches!(
            stream.recv().await.unwrap().event,
            ProgressEventKind::ServingProgress(_)
        ));
        assert!(matches!(
            stream.recv().await.unwrap().event,
            ProgressEventKind::RevisionSuperseded { .. }
        ));
        assert!(stream.recv().await.is_none());
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn preparing_clears_a_stale_terminal_observation() {
        let (_temp, state, resources) = fixture(snapshot(1, Some(1))).await;
        mark_resources_preparing(&resources, ResourceRevision::new(1));

        let (_, current) = resources.progress().snapshot_for(ProgressTarget::Current);
        assert_eq!(current.observed_revision, None);
        assert_eq!(current.resources[0].phase, ResourcePhase::Preparing);
        assert_eq!(current.resources[0].observed_revision, None);
        assert_eq!(
            resources
                .progress()
                .target_state(ProgressTarget::DesiredRevision(ResourceRevision::new(1))),
            crate::progress::ProgressTargetState::Watching
        );
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unchanged_terminal_revision_wakeup_does_not_rebuild_the_generation() {
        let mut ready = snapshot(1, Some(1));
        ready.resources[0].phase = ResourcePhase::Ready;
        let (_temp, state, resources) = fixture(ready).await;

        assert!(!revision_requires_reconcile(
            &resources,
            ResourceRevision::new(1)
        ));

        mark_resources_preparing(&resources, ResourceRevision::new(1));
        assert!(revision_requires_reconcile(
            &resources,
            ResourceRevision::new(1)
        ));
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stable_failure_preserves_last_good_revision_and_marks_only_the_new_target_failed() {
        let (_temp, state, resources) = fixture(snapshot(2, Some(1))).await;
        let failure = ReconcileFailure::new(anyhow::anyhow!("synthetic generation failure"));
        publish_failure(&resources, None, &[], &failure);

        let (_, current) = resources.progress().snapshot_for(ProgressTarget::Current);
        assert_eq!(current.observed_revision, Some(ResourceRevision::new(1)));
        assert_eq!(current.resources[0].phase, ResourcePhase::Failed);
        assert_eq!(
            current.resources[0].error_code.as_deref(),
            Some("serving_reconcile_failed")
        );
        assert_eq!(
            current.serving.as_ref().map(|serving| serving.stage),
            Some(ServingProgressStage::Failed)
        );
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retry_and_drain_degraded_stages_keep_action_correlation() {
        let action_id = ActionId::from_bytes([8; 16]);
        let (_temp, state, resources) = fixture(snapshot(2, Some(1))).await;
        resources.publish_action(&receipt(
            action_id,
            ActionKind::SetCredentialMaterial,
            ActionPhase::Running,
        ));

        publish_retry(
            &resources,
            ResourceRevision::new(2),
            &[action_id],
            2,
            &ReconcileFailure::new(anyhow::anyhow!("retry me")),
        );
        publish_background_serving(
            &resources,
            ResourceRevision::new(2),
            &[action_id],
            ServingProgressStage::Degraded,
            2,
            Some("retired generation remains active".into()),
        );

        let (_, revision) = resources
            .progress()
            .snapshot_for(ProgressTarget::DesiredRevision(ResourceRevision::new(2)));
        assert_eq!(
            revision.serving.as_ref().map(|serving| serving.stage),
            Some(ServingProgressStage::Degraded)
        );
        let (_, action) = resources
            .progress()
            .snapshot_for(ProgressTarget::Action(action_id));
        assert_eq!(action.serving, revision.serving);
        assert_eq!(action.actions[0].phase, ActionPhase::Running);
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn action_terminal_event_follows_the_correlated_serving_snapshot() {
        let action_id = ActionId::from_bytes([9; 16]);
        let (_temp, state, resources) = fixture(snapshot(2, Some(1))).await;
        resources.publish_action(&receipt(
            action_id,
            ActionKind::SetCredentialMaterial,
            ActionPhase::Running,
        ));
        let mut stream = resources
            .progress()
            .subscribe(ProgressTarget::Action(action_id));
        let _ = stream.recv().await.unwrap();

        publish_serving_targets(
            &resources,
            ResourceRevision::new(2),
            &[action_id],
            ServingProgressStage::Building,
            0,
            1,
            0,
            None,
        );
        publish_completed_action(
            &resources,
            receipt(
                action_id,
                ActionKind::SetCredentialMaterial,
                ActionPhase::Ready,
            ),
        );

        let (_, action) = resources
            .progress()
            .snapshot_for(ProgressTarget::Action(action_id));
        assert_eq!(action.actions[0].phase, ActionPhase::Ready);
        assert_eq!(
            action.serving.as_ref().map(|serving| serving.stage),
            Some(ServingProgressStage::Building)
        );
        assert!(matches!(
            stream.recv().await.unwrap().event,
            ProgressEventKind::ServingProgress(_)
        ));
        assert!(matches!(
            stream.recv().await.unwrap().event,
            ProgressEventKind::CredentialProgress(_)
        ));
        assert!(matches!(
            stream.recv().await.unwrap().event,
            ProgressEventKind::Snapshot(_)
        ));
        assert!(matches!(
            stream.recv().await.unwrap().event,
            ProgressEventKind::ActionCompleted(_)
        ));
        assert!(stream.recv().await.is_none());
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_interrupt_never_retargets_a_revision_stream() {
        let (_temp, state, resources) = fixture(snapshot(2, Some(1))).await;
        let outcome = interrupted_outcome(
            &resources,
            ResourceRevision::new(2),
            &[],
            1,
            ReconcileInterrupt::Shutdown,
        );
        assert!(matches!(outcome, ReconcileOutcome::Shutdown));
        assert_eq!(
            resources
                .progress()
                .target_state(ProgressTarget::DesiredRevision(ResourceRevision::new(2))),
            crate::progress::ProgressTargetState::Watching
        );
        state.shutdown().await.unwrap();
    }
}
