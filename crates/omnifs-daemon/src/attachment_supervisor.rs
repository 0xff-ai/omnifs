//! Daemon-owned reconciliation of desired attachments into exact runtimes.

use crate::progress::ProgressHub;
use crate::resource_control::ResourceControl;
use anyhow::Context as _;
use omnifs_api::{
    ActionKind, ActionPhase, ActionReceipt, AttachmentProgress, AttachmentProgressStage,
    ProgressEventKind, ProgressTarget, ResourcePhase,
};
use omnifs_core::{
    AttachmentRuntime, AttachmentSpec, ResourceKey, ResourceKind, ResourceName, ResourceRevision,
};
use omnifs_fs_runtime::{
    AttachEndpoints, ConfirmedRuntime, RuntimeDriver, RuntimeEvent, RuntimeEventReceiver,
    RuntimeEventSink, RuntimePaths, RuntimeStage, RuntimeState,
};
use omnifs_state::{
    AttachmentInstance, AttachmentObservation, AttachmentPhase, DesiredAttachment, StateStore,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Semaphore, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};

const MAX_ACTIVE_ATTACHMENTS: usize = 4;
const RUNTIME_EVENT_CAPACITY: usize = 32;
const SESSION_WAIT: Duration = Duration::from_secs(30);
const SESSION_LIVENESS_POLL: Duration = Duration::from_millis(250);
const SESSION_DRAIN: Duration = Duration::from_secs(10);
const RETRY_BASE: Duration = Duration::from_millis(250);
const RETRY_CAP: Duration = Duration::from_secs(8);
const MAX_RETRY_ATTEMPTS: u32 = 5;

type StopAllReply = tokio::sync::oneshot::Sender<anyhow::Result<Vec<ResourceName>>>;

/// Sole daemon owner of Attachment runtime sequencing and recovery.
pub(crate) struct AttachmentSupervisor {
    shutdown: watch::Sender<bool>,
    /// Interrupts an in-flight reconciliation pass before the command or
    /// shutdown path performs exact durable-identity cleanup.
    interrupt: watch::Sender<u64>,
    commands: mpsc::Sender<StopAllReply>,
    task: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,
}

#[derive(Clone)]
struct ReconcileContext {
    state: Arc<StateStore>,
    resources: Arc<ResourceControl>,
    vfs: Arc<omnifs_vfs::VfsServer>,
    paths: RuntimePaths,
    endpoints: AttachEndpoints,
    launch_slots: Arc<Semaphore>,
    queued: Arc<AtomicU32>,
    active: Arc<AtomicU32>,
}

struct Work {
    name: ResourceName,
    desired: Option<DesiredAttachment>,
    instance: AttachmentInstance,
    action: Option<ActionReceipt>,
    retry_count: u32,
}

enum WorkOutcome {
    Done,
    Retry,
}

impl AttachmentSupervisor {
    pub(crate) fn spawn(
        state: Arc<StateStore>,
        resources: Arc<ResourceControl>,
        vfs: Arc<omnifs_vfs::VfsServer>,
        paths: RuntimePaths,
        endpoints: AttachEndpoints,
    ) -> Arc<Self> {
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (interrupt, interrupt_rx) = watch::channel(0_u64);
        let (commands, command_rx) = mpsc::channel(1);
        let context = ReconcileContext {
            state,
            resources,
            vfs,
            paths,
            endpoints,
            launch_slots: Arc::new(Semaphore::new(MAX_ACTIVE_ATTACHMENTS)),
            queued: Arc::new(AtomicU32::new(0)),
            active: Arc::new(AtomicU32::new(0)),
        };
        let task = tokio::spawn(run(context, shutdown_rx, interrupt_rx, command_rx));
        Arc::new(Self {
            shutdown,
            interrupt,
            commands,
            task: Mutex::new(Some(task)),
        })
    }

    /// Stop every exact runtime while preserving desired attachment rows.
    pub(crate) async fn stop_all(&self) -> anyhow::Result<Vec<ResourceName>> {
        self.interrupt.send_modify(|generation| {
            *generation = generation.saturating_add(1);
        });
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.commands
            .send(reply)
            .await
            .context("attachment supervisor stopped before stop-all")?;
        receive
            .await
            .context("attachment supervisor dropped stop-all reply")?
    }

    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.interrupt.send_modify(|generation| {
            *generation = generation.saturating_add(1);
        });
        self.shutdown.send_replace(true);
        let Some(task) = self.task.lock().await.take() else {
            return Ok(());
        };
        task.await.context("join attachment supervisor")?
    }
}

async fn run(
    context: ReconcileContext,
    mut shutdown: watch::Receiver<bool>,
    mut interrupt: watch::Receiver<u64>,
    mut commands: mpsc::Receiver<StopAllReply>,
) -> anyhow::Result<()> {
    let mut revisions = context.resources.subscribe_revisions();
    let mut namespaces = context.resources.subscribe_namespace_revisions();
    let mut actions = context.resources.subscribe_actions();
    let mut sessions = context.vfs.session_changes();
    let mut retries = HashMap::<ResourceName, u32>::new();
    let mut reconcile_now = true;
    let mut next_retry = None;
    let mut suspended = false;

    loop {
        if *shutdown.borrow() {
            let _ = stop_all_runtimes(&context).await?;
            return Ok(());
        }
        if reconcile_now && !suspended {
            next_retry = match reconcile_latest(&context, &mut retries, &mut interrupt).await? {
                ReconcilePass::Completed(next_retry) => next_retry,
                // The caller that requested the interrupt remains queued in
                // `commands` (or `shutdown` is already set). Return to the
                // event loop so it can perform exact identity cleanup now.
                ReconcilePass::Interrupted => None,
            };
            reconcile_now = false;
        }
        let retry_sleep =
            tokio::time::sleep(next_retry.unwrap_or_else(|| Duration::from_hours(24)));
        tokio::pin!(retry_sleep);
        tokio::select! {
            changed = revisions.changed() => {
                if changed.is_err() {
                    let _ = stop_all_runtimes(&context).await?;
                    return Ok(());
                }
                revisions.borrow_and_update();
                reconcile_now = !suspended;
            },
            changed = namespaces.changed() => {
                if changed.is_err() {
                    let _ = stop_all_runtimes(&context).await?;
                    return Ok(());
                }
                namespaces.borrow_and_update();
                reconcile_now = !suspended;
            },
            changed = actions.changed() => {
                if changed.is_err() {
                    let _ = stop_all_runtimes(&context).await?;
                    return Ok(());
                }
                actions.borrow_and_update();
                reconcile_now = !suspended;
            },
            changed = sessions.changed() => {
                if changed.is_err() {
                    let _ = stop_all_runtimes(&context).await?;
                    return Ok(());
                }
                sessions.borrow_and_update();
                reconcile_now = !suspended;
            },
            command = commands.recv() => {
                if let Some(reply) = command {
                    let result = stop_all_runtimes(&context).await;
                    let _ = reply.send(result);
                    suspended = true;
                    reconcile_now = false;
                }
            },
            () = &mut retry_sleep => {
                reconcile_now = !suspended;
            },
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = stop_all_runtimes(&context).await?;
                    return Ok(());
                }
            },
        }
    }
}

enum ReconcilePass {
    Completed(Option<Duration>),
    Interrupted,
}

async fn reconcile_latest(
    context: &ReconcileContext,
    retries: &mut HashMap<ResourceName, u32>,
    interrupt: &mut watch::Receiver<u64>,
) -> anyhow::Result<ReconcilePass> {
    let desired = context
        .state
        .desired_attachments()
        .await?
        .into_iter()
        .map(|attachment| (attachment.definition.name.clone(), attachment))
        .collect::<BTreeMap<_, _>>();
    let instances = context
        .state
        .attachment_instances()
        .await?
        .into_iter()
        .map(|instance| (instance.name.clone(), instance))
        .collect::<BTreeMap<_, _>>();
    let actions = context
        .state
        .pending_actions()
        .await?
        .into_iter()
        .filter(|action| action.kind == ActionKind::RestartAttachment)
        .map(|action| (action.target.name.clone(), action))
        .collect::<BTreeMap<_, _>>();
    let names = desired
        .keys()
        .chain(instances.keys())
        .chain(actions.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let now = unix_seconds();
    let current_revision = context.state.resource_snapshot().await?.revision;
    let mut work = Vec::with_capacity(names.len());
    let mut earliest_retry = None;
    for name in names {
        let instance = instances
            .get(&name)
            .cloned()
            .unwrap_or_else(|| AttachmentInstance::pending(name.clone()));
        if let Some(retry_at) = instance.retry_at
            && retry_at > now
        {
            let delay = Duration::from_secs(u64::try_from(retry_at - now).unwrap_or(1));
            earliest_retry =
                Some(earliest_retry.map_or(delay, |current: Duration| current.min(delay)));
            continue;
        }
        work.push(Work {
            name: name.clone(),
            desired: desired.get(&name).cloned(),
            instance,
            action: actions.get(&name).cloned(),
            retry_count: retries.get(&name).copied().unwrap_or(0),
        });
    }
    context.queued.store(
        u32::try_from(work.len()).unwrap_or(u32::MAX),
        Ordering::Release,
    );
    let mut tasks = JoinSet::new();
    for item in work {
        let context = context.clone();
        tasks.spawn(async move { reconcile_one(&context, current_revision, item).await });
    }
    loop {
        let joined = tokio::select! {
            joined = tasks.join_next() => joined,
            changed = interrupt.changed() => {
                // A closed sender is also an interruption. Do not let an
                // orphaned supervisor leave runtime work behind.
                let _ = changed;
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                // Every reconciliation task has now either completed or been
                // aborted. The next pass repopulates these coalesced
                // counters from durable work.
                context.queued.store(0, Ordering::Release);
                context.active.store(0, Ordering::Release);
                return Ok(ReconcilePass::Interrupted);
            },
        };
        let Some(joined) = joined else { break };
        let (name, outcome) = joined.context("join attachment reconciliation task")??;
        match outcome {
            WorkOutcome::Done => {
                retries.remove(&name);
            },
            WorkOutcome::Retry => {
                let attempt = retries.entry(name).or_default();
                *attempt = attempt.saturating_add(1);
                let delay = retry_delay(*attempt);
                earliest_retry =
                    Some(earliest_retry.map_or(delay, |current: Duration| current.min(delay)));
            },
        }
    }
    Ok(ReconcilePass::Completed(earliest_retry))
}

async fn reconcile_one(
    context: &ReconcileContext,
    current_revision: ResourceRevision,
    mut work: Work,
) -> anyhow::Result<(ResourceName, WorkOutcome)> {
    let name = work.name.clone();
    let permit = context
        .launch_slots
        .clone()
        .acquire_owned()
        .await
        .context("attachment launch semaphore closed")?;
    context.queued.fetch_sub(1, Ordering::AcqRel);
    context.active.fetch_add(1, Ordering::AcqRel);
    let result = reconcile_one_active(context, current_revision, &mut work).await;
    context.active.fetch_sub(1, Ordering::AcqRel);
    drop(permit);
    result.map(|outcome| (name, outcome))
}

#[allow(
    clippy::too_many_lines,
    reason = "the attachment state machine stays linear so each durable fence precedes its effect"
)]
async fn reconcile_one_active(
    context: &ReconcileContext,
    current_revision: ResourceRevision,
    work: &mut Work,
) -> anyhow::Result<WorkOutcome> {
    if let Some(action) = &mut work.action
        && action.phase != ActionPhase::Running
    {
        *action = context
            .state
            .transition_action(action.action_id, ActionPhase::Running, None, None)
            .await?;
        context.resources.publish_action(action);
    }

    let Some(desired) = work.desired.clone() else {
        return reconcile_deletion(context, current_revision, work).await;
    };
    if work.instance.phase == AttachmentPhase::Failed
        && work.instance.observed_version == Some(desired.version)
        && work.action.is_none()
    {
        return Ok(WorkOutcome::Done);
    }
    if !namespace_ready(context.resources.progress(), desired.revision) {
        if !update_observation(&context.state, &mut work.instance, |observation| {
            observation.phase = AttachmentPhase::WaitingForNamespace;
            observation.retry_at = None;
            observation.last_error_code = None;
            observation.last_error_detail = None;
        })
        .await?
        {
            return Ok(WorkOutcome::Done);
        }
        context.resources.mark_attachment_phase(
            current_revision,
            &work.name,
            ResourcePhase::Pending,
            None,
            None,
        );
        publish_phase(
            context,
            &desired,
            work.action.as_ref(),
            AttachmentProgressStage::WaitingForNamespace,
            PhaseReport {
                retry_count: work.retry_count,
                ..PhaseReport::default()
            },
        );
        return Ok(WorkOutcome::Done);
    }

    let restart = work.action.is_some() || work.instance.observed_version != Some(desired.version);
    if let (Some(observed_spec), Some(runtime_instance)) = (
        work.instance.observed_spec.clone(),
        work.instance.runtime_instance.clone(),
    ) {
        let observed_version = work
            .instance
            .observed_version
            .context("attachment observed spec has no version")?;
        let driver = match runtime_driver(context, &work.name, &observed_spec) {
            Ok(driver) => driver,
            Err(error) => {
                tracing::warn!(attachment = %work.name, %error, "stored attachment runtime spec is invalid");
                return terminal_failure(
                    context,
                    current_revision,
                    &desired,
                    work,
                    "attachment_runtime_config_invalid",
                    "the stored attachment runtime specification is invalid",
                )
                .await;
            },
        };
        let confirmed = driver.confirmed(&runtime_instance).await;
        match confirmed {
            Ok(Some(confirmed)) if !restart && confirmed.is_running() => {
                tracing::debug!(
                    attachment = %work.name,
                    %runtime_instance,
                    "confirmed retained attachment runtime; waiting for its exact VFS session"
                );
                let expected = session(&work.name, &observed_spec, &runtime_instance);
                match wait_for_confirmed_session(context, &driver, &expected, &runtime_instance)
                    .await?
                {
                    ConfirmedSession::Attached => {
                        if !update_observation(&context.state, &mut work.instance, |observation| {
                            observation.phase = AttachmentPhase::Ready;
                            observation.retry_at = None;
                            observation.last_error_code = None;
                            observation.last_error_detail = None;
                        })
                        .await?
                        {
                            return Ok(WorkOutcome::Done);
                        }
                        finish_ready(
                            context,
                            &desired,
                            work.action.as_ref(),
                            &work.instance,
                            current_revision,
                        )
                        .await?;
                        return Ok(WorkOutcome::Done);
                    },
                    ConfirmedSession::Absent => {},
                    ConfirmedSession::Stopped(confirmed)
                    | ConfirmedSession::TimedOut(confirmed) => {
                        if let Err(error) = stop_exact(
                            context,
                            &work.name,
                            &observed_spec,
                            &runtime_instance,
                            confirmed,
                        )
                        .await
                        {
                            tracing::warn!(attachment = %work.name, %error, "unready attachment runtime stop failed");
                            return retry_or_fail(
                                context,
                                current_revision,
                                &desired,
                                work,
                                "attachment_stop_failed",
                                "the previous attachment runtime could not be stopped",
                            )
                            .await;
                        }
                    },
                }
                if !clear_observed(context, current_revision, work).await? {
                    return Ok(WorkOutcome::Done);
                }
            },
            Ok(Some(confirmed)) => {
                tracing::debug!(
                    attachment = %work.name,
                    %runtime_instance,
                    "confirmed attachment runtime requires replacement"
                );
                if !record_stopping(context, current_revision, &desired, work).await? {
                    return Ok(WorkOutcome::Done);
                }
                if let Err(error) = stop_exact(
                    context,
                    &work.name,
                    &observed_spec,
                    &runtime_instance,
                    confirmed,
                )
                .await
                {
                    tracing::warn!(attachment = %work.name, %error, "attachment restart stop failed");
                    return retry_or_fail(
                        context,
                        current_revision,
                        &desired,
                        work,
                        "attachment_stop_failed",
                        "the previous attachment runtime could not be stopped",
                    )
                    .await;
                }
                if !clear_observed(context, current_revision, work).await? {
                    return Ok(WorkOutcome::Done);
                }
            },
            Ok(None) => {
                tracing::debug!(
                    attachment = %work.name,
                    %runtime_instance,
                    "durable attachment runtime is absent; clearing its observation"
                );
                if !clear_observed(context, current_revision, work).await? {
                    return Ok(WorkOutcome::Done);
                }
            },
            Err(error) => {
                tracing::warn!(
                    attachment = %work.name,
                    version = %observed_version,
                    %error,
                    "attachment runtime identity could not be proved"
                );
                return terminal_failure(
                    context,
                    current_revision,
                    &desired,
                    work,
                    "attachment_identity_conflict",
                    "the existing attachment runtime does not match its durable identity",
                )
                .await;
            },
        }
    }

    let runtime_instance = match work.instance.runtime_instance.clone() {
        Some(instance) => instance,
        None => random_runtime_instance()?,
    };
    // Persist the exact launch identity before any process, container, or VM
    // effect. A concurrent desired update then keeps enough observed state to
    // stop this old runtime after a crash.
    if !update_observation(&context.state, &mut work.instance, |observation| {
        observation.observed_version = Some(desired.version);
        observation.observed_spec = Some(desired.definition.spec.clone());
        observation.runtime_instance = Some(runtime_instance.clone());
        observation.phase = AttachmentPhase::Starting;
        observation.retry_at = None;
        observation.last_error_code = None;
        observation.last_error_detail = None;
    })
    .await?
    {
        return Ok(WorkOutcome::Done);
    }
    context.resources.mark_attachment_phase(
        current_revision,
        &work.name,
        ResourcePhase::Preparing,
        None,
        None,
    );
    publish_phase(
        context,
        &desired,
        work.action.as_ref(),
        AttachmentProgressStage::Starting,
        PhaseReport {
            retry_count: work.retry_count,
            ..PhaseReport::default()
        },
    );

    let expected = session(&work.name, &desired.definition.spec, &runtime_instance);
    let (events, receiver) = RuntimeEventSink::bounded(RUNTIME_EVENT_CAPACITY);
    let event_task = tokio::spawn(forward_runtime_events(
        receiver,
        Arc::clone(&context.resources),
        desired.clone(),
        work.action.clone(),
        Arc::clone(&context.queued),
        Arc::clone(&context.active),
    ));
    let driver = match runtime_driver_with_events(
        context,
        &work.name,
        &desired.definition.spec,
        events,
    ) {
        Ok(driver) => driver,
        Err(error) => {
            tracing::warn!(attachment = %work.name, %error, "desired attachment runtime spec is invalid");
            event_task.abort();
            let _ = event_task.await;
            return terminal_failure(
                context,
                current_revision,
                &desired,
                work,
                "attachment_runtime_config_invalid",
                "the desired attachment runtime specification is invalid",
            )
            .await;
        },
    };
    let attached = context.vfs.wait_for_session(&expected, SESSION_WAIT);
    let launch = driver
        .launch(&runtime_instance, &context.endpoints, async move {
            if attached.await {
                Ok(())
            } else {
                anyhow::bail!("timed out waiting for exact VFS session")
            }
        })
        .await;
    drop(driver);
    event_task
        .await
        .context("join runtime progress forwarder")?;
    if let Err(error) = launch {
        tracing::warn!(attachment = %work.name, stage = ?error.stage(), %error, "attachment launch failed");
        return retry_or_fail(
            context,
            current_revision,
            &desired,
            work,
            "attachment_launch_failed",
            "the attachment runtime could not start",
        )
        .await;
    }
    if !context.vfs.wait_for_session(&expected, SESSION_WAIT).await {
        return retry_or_fail(
            context,
            current_revision,
            &desired,
            work,
            "attachment_session_timeout",
            "the attachment runtime did not establish its exact VFS session",
        )
        .await;
    }
    let latest = context.state.desired_attachments().await?;
    if !latest.iter().any(|candidate| {
        candidate.definition.name == work.name && candidate.version == desired.version
    }) {
        let driver = match runtime_driver(context, &work.name, &desired.definition.spec) {
            Ok(driver) => driver,
            Err(error) => {
                tracing::warn!(attachment = %work.name, %error, "superseded attachment runtime spec is invalid");
                return Ok(WorkOutcome::Done);
            },
        };
        match driver.confirmed(&runtime_instance).await {
            Ok(Some(confirmed)) => {
                if let Err(error) = stop_exact(
                    context,
                    &work.name,
                    &desired.definition.spec,
                    &runtime_instance,
                    confirmed,
                )
                .await
                {
                    tracing::warn!(attachment = %work.name, %error, "superseded attachment stop will retry against the latest desired version");
                }
            },
            Ok(None) => {},
            Err(error) => {
                tracing::warn!(attachment = %work.name, %error, "superseded attachment probe will retry against the latest desired version");
            },
        }
        return Ok(WorkOutcome::Done);
    }
    if !update_observation(&context.state, &mut work.instance, |observation| {
        observation.phase = AttachmentPhase::Ready;
        observation.retry_at = None;
        observation.last_error_code = None;
        observation.last_error_detail = None;
    })
    .await?
    {
        return Ok(WorkOutcome::Done);
    }
    finish_ready(
        context,
        &desired,
        work.action.as_ref(),
        &work.instance,
        current_revision,
    )
    .await?;
    Ok(WorkOutcome::Done)
}

#[allow(
    clippy::too_many_lines,
    reason = "deletion keeps identity proof, stop, drain, absence proof, and tombstone clear ordered"
)]
async fn reconcile_deletion(
    context: &ReconcileContext,
    revision: ResourceRevision,
    work: &mut Work,
) -> anyhow::Result<WorkOutcome> {
    fail_action_for_deleted_attachment(context, work).await?;
    if !update_observation(&context.state, &mut work.instance, |observation| {
        observation.phase = AttachmentPhase::Deleting;
        observation.retry_at = None;
    })
    .await?
    {
        return Ok(WorkOutcome::Done);
    }
    publish_deletion(context, revision, work);
    if let (Some(spec), Some(runtime_instance)) = (
        work.instance.observed_spec.clone(),
        work.instance.runtime_instance.clone(),
    ) {
        let driver = match runtime_driver(context, &work.name, &spec) {
            Ok(driver) => driver,
            Err(error) => {
                tracing::warn!(attachment = %work.name, %error, "deleting attachment runtime spec is invalid");
                return retry_deleted(
                    context,
                    revision,
                    work,
                    "attachment_delete_runtime_config_invalid",
                    "the deleting attachment runtime specification is invalid",
                )
                .await;
            },
        };
        match driver.confirmed(&runtime_instance).await {
            Ok(Some(confirmed)) => {
                if let Err(error) =
                    stop_exact(context, &work.name, &spec, &runtime_instance, confirmed).await
                {
                    tracing::warn!(attachment = %work.name, %error, "attachment deletion stop failed");
                    return retry_deleted(
                        context,
                        revision,
                        work,
                        "attachment_delete_stop_failed",
                        "the deleting runtime could not be stopped",
                    )
                    .await;
                }
            },
            Ok(None) => {},
            Err(error) => {
                tracing::warn!(attachment = %work.name, %error, "attachment deletion identity check failed");
                return terminal_deleted_failure(
                    context,
                    revision,
                    work,
                    "attachment_delete_identity_conflict",
                    "the deleting runtime no longer matches its durable identity",
                )
                .await;
            },
        }
        if let Err(error) = wait_for_session_absence(context, &work.name, &runtime_instance).await {
            tracing::warn!(attachment = %work.name, %error, "attachment deletion session did not drain");
            return retry_deleted(
                context,
                revision,
                work,
                "attachment_delete_session_drain_failed",
                "the deleting attachment session did not drain",
            )
            .await;
        }
        match driver.confirmed(&runtime_instance).await {
            Ok(None) => {},
            Ok(Some(_)) => {
                return retry_deleted(
                    context,
                    revision,
                    work,
                    "attachment_delete_incomplete",
                    "the attachment runtime is still present after stop",
                )
                .await;
            },
            Err(error) => {
                tracing::warn!(attachment = %work.name, %error, "attachment deletion absence check failed");
                return retry_deleted(
                    context,
                    revision,
                    work,
                    "attachment_delete_probe_failed",
                    "the deleting runtime absence check failed",
                )
                .await;
            },
        }
    }
    let cleared = context
        .state
        .clear_attachment_instance_if_deleting(
            work.name.clone(),
            work.instance.runtime_instance.clone(),
        )
        .await?;
    if !cleared {
        // A newer desired declaration won the race while the old runtime was
        // being stopped. Preserve that row and let the next pass reconcile
        // from its durable desired and observed identities.
        return Ok(WorkOutcome::Done);
    }
    let terminal = context
        .resources
        .clear_deleted_attachment(revision, &work.name);
    if terminal {
        publish_revision_ready(context.resources.progress(), revision);
    }
    Ok(WorkOutcome::Done)
}

async fn fail_action_for_deleted_attachment(
    context: &ReconcileContext,
    work: &mut Work,
) -> anyhow::Result<()> {
    const CODE: &str = "attachment_deleted";
    const DETAIL: &str = "the attachment was removed from desired state before restart completed";

    let Some(action) = &mut work.action else {
        return Ok(());
    };
    *action = context
        .state
        .transition_action(
            action.action_id,
            ActionPhase::Failed,
            Some(CODE.to_owned()),
            Some(DETAIL.to_owned()),
        )
        .await?;
    context.resources.publish_action(action);
    context.resources.progress().publish(
        ProgressTarget::Action(action.action_id),
        ProgressEventKind::ActionFailed {
            receipt: action.clone(),
            error_code: CODE.to_owned(),
            detail: DETAIL.to_owned(),
        },
    );
    Ok(())
}

async fn stop_all_runtimes(context: &ReconcileContext) -> anyhow::Result<Vec<ResourceName>> {
    let instances = context.state.attachment_instances().await?;
    let mut stragglers = BTreeSet::new();
    for mut instance in instances {
        let (Some(spec), Some(runtime_instance)) = (
            instance.observed_spec.clone(),
            instance.runtime_instance.clone(),
        ) else {
            continue;
        };
        if !update_observation(&context.state, &mut instance, |observation| {
            observation.phase = AttachmentPhase::Stopping;
            observation.retry_at = None;
        })
        .await?
        {
            stragglers.insert(instance.name.clone());
            continue;
        }
        let driver = match runtime_driver(context, &instance.name, &spec) {
            Ok(driver) => driver,
            Err(error) => {
                tracing::warn!(attachment = %instance.name, %error, "could not open attachment runtime during shutdown");
                stragglers.insert(instance.name.clone());
                continue;
            },
        };
        let stopped = match driver.confirmed(&runtime_instance).await {
            Ok(Some(confirmed)) => {
                match stop_exact(context, &instance.name, &spec, &runtime_instance, confirmed).await
                {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(attachment = %instance.name, %error, "attachment runtime did not stop during shutdown");
                        false
                    },
                }
            },
            Ok(None) => true,
            Err(error) => {
                tracing::warn!(attachment = %instance.name, %error, "attachment runtime identity could not be proved during shutdown");
                false
            },
        };
        if !stopped
            || wait_for_session_absence(context, &instance.name, &runtime_instance)
                .await
                .is_err()
        {
            stragglers.insert(instance.name.clone());
            continue;
        }
        if instance.deleting || instance.desired_version.is_none() {
            let _ = context
                .state
                .clear_attachment_instance_if_deleting(
                    instance.name.clone(),
                    instance.runtime_instance.clone(),
                )
                .await?;
        } else if !update_observation(&context.state, &mut instance, |observation| {
            observation.observed_version = None;
            observation.observed_spec = None;
            observation.runtime_instance = None;
            observation.phase = AttachmentPhase::Pending;
            observation.retry_at = None;
        })
        .await?
        {
            stragglers.insert(instance.name.clone());
        }
    }
    stragglers.extend(
        context
            .vfs
            .drain_sessions(SESSION_DRAIN)
            .await
            .into_iter()
            .map(|session| session.attachment),
    );
    Ok(stragglers.into_iter().collect())
}

async fn record_stopping(
    context: &ReconcileContext,
    current_revision: ResourceRevision,
    desired: &DesiredAttachment,
    work: &mut Work,
) -> anyhow::Result<bool> {
    if !update_observation(&context.state, &mut work.instance, |observation| {
        observation.phase = AttachmentPhase::Stopping;
        observation.retry_at = None;
    })
    .await?
    {
        return Ok(false);
    }
    context.resources.mark_attachment_phase(
        current_revision,
        &work.name,
        ResourcePhase::Preparing,
        None,
        None,
    );
    publish_phase(
        context,
        desired,
        work.action.as_ref(),
        AttachmentProgressStage::Stopping,
        PhaseReport {
            retry_count: work.retry_count,
            ..PhaseReport::default()
        },
    );
    Ok(true)
}

async fn finish_ready(
    context: &ReconcileContext,
    desired: &DesiredAttachment,
    action: Option<&ActionReceipt>,
    _instance: &AttachmentInstance,
    current_revision: ResourceRevision,
) -> anyhow::Result<()> {
    publish_phase(
        context,
        desired,
        action,
        AttachmentProgressStage::Ready,
        PhaseReport::default(),
    );
    let desired_terminal = context
        .resources
        .mark_attachment_ready(desired.revision, &desired.definition.name);
    if desired_terminal {
        publish_revision_ready(context.resources.progress(), desired.revision);
    }
    if current_revision != desired.revision
        && context
            .resources
            .mark_attachment_ready(current_revision, &desired.definition.name)
    {
        publish_revision_ready(context.resources.progress(), current_revision);
    }
    if let Some(action) = action {
        let ready = context
            .state
            .transition_action(action.action_id, ActionPhase::Ready, None, None)
            .await?;
        context.resources.publish_action(&ready);
        context.resources.progress().publish(
            ProgressTarget::Action(ready.action_id),
            ProgressEventKind::ActionCompleted(ready),
        );
    }
    Ok(())
}

async fn retry_or_fail(
    context: &ReconcileContext,
    current_revision: ResourceRevision,
    desired: &DesiredAttachment,
    work: &mut Work,
    code: &str,
    detail: &str,
) -> anyhow::Result<WorkOutcome> {
    if work.retry_count.saturating_add(1) >= MAX_RETRY_ATTEMPTS {
        return terminal_failure(context, current_revision, desired, work, code, detail).await;
    }
    let next = retry_delay(work.retry_count.saturating_add(1));
    let retry_at =
        unix_seconds().saturating_add(i64::try_from(next.as_secs().max(1)).unwrap_or(i64::MAX));
    if !update_observation(&context.state, &mut work.instance, |observation| {
        observation.phase = AttachmentPhase::Retrying;
        observation.retry_at = Some(retry_at);
        observation.last_error_code = Some(code.to_owned());
        observation.last_error_detail = Some(detail.to_owned());
    })
    .await?
    {
        return Ok(WorkOutcome::Done);
    }
    if let Some(action) = &mut work.action {
        *action = context
            .state
            .transition_action(action.action_id, ActionPhase::Retrying, None, None)
            .await?;
        context.resources.publish_action(action);
    }
    publish_phase(
        context,
        desired,
        work.action.as_ref(),
        AttachmentProgressStage::Retrying,
        PhaseReport {
            retry_count: work.retry_count.saturating_add(1),
            error_code: Some(code.to_owned()),
            detail: Some(detail.to_owned()),
            next_retry_unix_ms: Some(
                unix_millis().saturating_add(u64::try_from(next.as_millis()).unwrap_or(u64::MAX)),
            ),
            ..PhaseReport::default()
        },
    );
    mark_resource_phase(
        context,
        current_revision,
        &work.name,
        ResourcePhase::Retrying,
        Some(code),
        Some(detail),
    );
    Ok(WorkOutcome::Retry)
}

async fn terminal_failure(
    context: &ReconcileContext,
    current_revision: ResourceRevision,
    desired: &DesiredAttachment,
    work: &mut Work,
    code: &str,
    detail: &str,
) -> anyhow::Result<WorkOutcome> {
    if !update_observation(&context.state, &mut work.instance, |observation| {
        observation.phase = AttachmentPhase::Failed;
        observation.retry_at = None;
        observation.last_error_code = Some(code.to_owned());
        observation.last_error_detail = Some(detail.to_owned());
    })
    .await?
    {
        return Ok(WorkOutcome::Done);
    }
    publish_phase(
        context,
        desired,
        work.action.as_ref(),
        AttachmentProgressStage::Failed,
        PhaseReport {
            retry_count: work.retry_count,
            error_code: Some(code.to_owned()),
            detail: Some(detail.to_owned()),
            ..PhaseReport::default()
        },
    );
    mark_resource_phase(
        context,
        current_revision,
        &work.name,
        ResourcePhase::Failed,
        Some(code),
        Some(detail),
    );
    context.resources.progress().publish(
        ProgressTarget::DesiredRevision(desired.revision),
        ProgressEventKind::RevisionFailed {
            revision: desired.revision,
            error_code: code.to_owned(),
            detail: detail.to_owned(),
        },
    );
    if let Some(action) = &work.action {
        let failed = context
            .state
            .transition_action(
                action.action_id,
                ActionPhase::Failed,
                Some(code.to_owned()),
                Some(detail.to_owned()),
            )
            .await?;
        context.resources.publish_action(&failed);
        context.resources.progress().publish(
            ProgressTarget::Action(failed.action_id),
            ProgressEventKind::ActionFailed {
                receipt: failed,
                error_code: code.to_owned(),
                detail: detail.to_owned(),
            },
        );
    }
    Ok(WorkOutcome::Done)
}

async fn retry_deleted(
    context: &ReconcileContext,
    revision: ResourceRevision,
    work: &mut Work,
    code: &str,
    detail: &str,
) -> anyhow::Result<WorkOutcome> {
    if !update_observation(&context.state, &mut work.instance, |observation| {
        observation.phase = AttachmentPhase::Retrying;
        observation.retry_at = Some(unix_seconds().saturating_add(1));
        observation.last_error_code = Some(code.to_owned());
        observation.last_error_detail = Some(detail.to_owned());
    })
    .await?
    {
        return Ok(WorkOutcome::Done);
    }
    publish_deletion(context, revision, work);
    Ok(WorkOutcome::Retry)
}

async fn terminal_deleted_failure(
    context: &ReconcileContext,
    revision: ResourceRevision,
    work: &mut Work,
    code: &str,
    detail: &str,
) -> anyhow::Result<WorkOutcome> {
    if !update_observation(&context.state, &mut work.instance, |observation| {
        observation.phase = AttachmentPhase::Failed;
        observation.retry_at = None;
        observation.last_error_code = Some(code.to_owned());
        observation.last_error_detail = Some(detail.to_owned());
    })
    .await?
    {
        return Ok(WorkOutcome::Done);
    }
    mark_resource_phase(
        context,
        revision,
        &work.name,
        ResourcePhase::Failed,
        Some(code),
        Some(detail),
    );
    context.resources.progress().publish(
        ProgressTarget::DesiredRevision(revision),
        ProgressEventKind::RevisionFailed {
            revision,
            error_code: code.to_owned(),
            detail: detail.to_owned(),
        },
    );
    Ok(WorkOutcome::Done)
}

#[derive(Default)]
struct PhaseReport {
    retry_count: u32,
    error_code: Option<String>,
    detail: Option<String>,
    completed_bytes: u64,
    next_retry_unix_ms: Option<u64>,
}

fn publish_phase(
    context: &ReconcileContext,
    desired: &DesiredAttachment,
    action: Option<&ActionReceipt>,
    stage: AttachmentProgressStage,
    report: PhaseReport,
) {
    let progress = AttachmentProgress {
        key: desired.definition.key(),
        desired_revision: desired.revision,
        runtime: desired.definition.spec.runtime(),
        stage,
        completed_bytes: report.completed_bytes,
        total_bytes: None,
        queued_attachments: context.queued.load(Ordering::Acquire),
        active_attachments: context.active.load(Ordering::Acquire),
        error_code: report.error_code,
        detail: report.detail,
        retry_count: report.retry_count,
        next_retry_unix_ms: report.next_retry_unix_ms,
    };
    context.resources.progress().record_attachment(
        ProgressTarget::DesiredRevision(desired.revision),
        progress.clone(),
    );
    if let Some(action) = action {
        context
            .resources
            .progress()
            .record_attachment(ProgressTarget::Action(action.action_id), progress);
    }
}

fn publish_deletion(context: &ReconcileContext, revision: ResourceRevision, work: &Work) {
    let runtime = work
        .instance
        .observed_spec
        .as_ref()
        .or(work.instance.desired_spec.as_ref())
        .map_or(AttachmentRuntime::Host, AttachmentSpec::runtime);
    context.resources.progress().record_attachment(
        ProgressTarget::DesiredRevision(revision),
        AttachmentProgress {
            key: ResourceKey::new(ResourceKind::Attachment, work.name.clone()),
            desired_revision: revision,
            runtime,
            stage: AttachmentProgressStage::Deleting,
            completed_bytes: 0,
            total_bytes: None,
            queued_attachments: context.queued.load(Ordering::Acquire),
            active_attachments: context.active.load(Ordering::Acquire),
            error_code: work.instance.last_error_code.clone(),
            detail: work.instance.last_error_detail.clone(),
            retry_count: work.retry_count,
            next_retry_unix_ms: work
                .instance
                .retry_at
                .and_then(|seconds| u64::try_from(seconds).ok())
                .and_then(|seconds| seconds.checked_mul(1_000)),
        },
    );
    mark_resource_phase(
        context,
        revision,
        &work.name,
        ResourcePhase::Deleting,
        work.instance.last_error_code.as_deref(),
        work.instance.last_error_detail.as_deref(),
    );
}

fn mark_resource_phase(
    context: &ReconcileContext,
    revision: ResourceRevision,
    name: &ResourceName,
    phase: ResourcePhase,
    error_code: Option<&str>,
    detail: Option<&str>,
) {
    context
        .resources
        .mark_attachment_phase(revision, name, phase, error_code, detail);
}

async fn forward_runtime_events(
    mut receiver: RuntimeEventReceiver,
    resources: Arc<ResourceControl>,
    desired: DesiredAttachment,
    action: Option<ActionReceipt>,
    queued: Arc<AtomicU32>,
    active: Arc<AtomicU32>,
) {
    while let Some(event) = receiver.recv().await {
        let (stage, completed_bytes, total_bytes) = runtime_progress(&event);
        let Some(stage) = stage else {
            continue;
        };
        let progress = AttachmentProgress {
            key: desired.definition.key(),
            desired_revision: desired.revision,
            runtime: desired.definition.spec.runtime(),
            stage,
            completed_bytes,
            total_bytes,
            queued_attachments: queued.load(Ordering::Acquire),
            active_attachments: active.load(Ordering::Acquire),
            error_code: None,
            detail: None,
            retry_count: 0,
            next_retry_unix_ms: None,
        };
        resources.progress().record_attachment(
            ProgressTarget::DesiredRevision(desired.revision),
            progress.clone(),
        );
        if let Some(action) = &action {
            resources
                .progress()
                .record_attachment(ProgressTarget::Action(action.action_id), progress);
        }
    }
}

fn runtime_progress(event: &RuntimeEvent) -> (Option<AttachmentProgressStage>, u64, Option<u64>) {
    match event {
        RuntimeEvent::Download {
            completed_bytes,
            total_bytes,
            ..
        } => (
            Some(AttachmentProgressStage::PullingImage),
            *completed_bytes,
            *total_bytes,
        ),
        RuntimeEvent::DownloadFinished {
            completed_bytes, ..
        } => (
            Some(AttachmentProgressStage::Materializing),
            completed_bytes.unwrap_or(0),
            *completed_bytes,
        ),
        RuntimeEvent::Stage { stage, state, .. } => {
            let stage = match stage {
                RuntimeStage::Probe => AttachmentProgressStage::Queued,
                RuntimeStage::MaterializeImage => AttachmentProgressStage::Materializing,
                RuntimeStage::StartProcess
                | RuntimeStage::StartContainer
                | RuntimeStage::StartVm => AttachmentProgressStage::Starting,
                RuntimeStage::WaitForOsMount => AttachmentProgressStage::Mounting,
                RuntimeStage::WaitForVfsSession => AttachmentProgressStage::WaitingForNamespace,
                RuntimeStage::Stop => AttachmentProgressStage::Stopping,
            };
            let stage = match state {
                RuntimeState::Stopped if stage == AttachmentProgressStage::Stopping => {
                    AttachmentProgressStage::Queued
                },
                _ => stage,
            };
            (Some(stage), 0, None)
        },
        RuntimeEvent::MountReady { .. } => (Some(AttachmentProgressStage::Mounting), 0, None),
        RuntimeEvent::Image { .. }
        | RuntimeEvent::ImageRetry { .. }
        | RuntimeEvent::Container { .. }
        | RuntimeEvent::DownloadFailed { .. }
        | RuntimeEvent::Failed { .. } => (None, 0, None),
    }
}

fn runtime_driver(
    context: &ReconcileContext,
    name: &ResourceName,
    spec: &AttachmentSpec,
) -> anyhow::Result<RuntimeDriver> {
    runtime_driver_with_events(context, name, spec, RuntimeEventSink::discard())
}

fn runtime_driver_with_events(
    context: &ReconcileContext,
    name: &ResourceName,
    spec: &AttachmentSpec,
    events: RuntimeEventSink,
) -> anyhow::Result<RuntimeDriver> {
    RuntimeDriver::new(&context.paths, name.clone(), spec.clone(), events)
}

async fn stop_exact(
    context: &ReconcileContext,
    name: &ResourceName,
    spec: &AttachmentSpec,
    runtime_instance: &str,
    confirmed: omnifs_fs_runtime::ConfirmedRuntime,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        confirmed.runtime_instance() == runtime_instance,
        "confirmed attachment runtime instance changed before exact stop"
    );
    let expected_session = session(name, spec, runtime_instance);
    context
        .vfs
        .begin_session_stop(&expected_session)
        .map_err(anyhow::Error::msg)?;

    let driver = runtime_driver(context, name, spec)?;
    driver
        .stop_confirmed(runtime_instance, confirmed)
        .await
        .map_err(anyhow::Error::new)?;
    anyhow::ensure!(
        driver.confirmed(runtime_instance).await?.is_none(),
        "attachment runtime is still present after exact stop"
    );
    context
        .vfs
        .close_stopped_session(&expected_session)
        .map_err(anyhow::Error::msg)?;
    wait_for_session_absence(context, name, runtime_instance).await?;
    context
        .vfs
        .finish_session_stop(&expected_session)
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

async fn wait_for_session_absence(
    context: &ReconcileContext,
    name: &ResourceName,
    runtime_instance: &str,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + SESSION_DRAIN;
    let mut changed = context.vfs.session_changes();
    loop {
        if !context.vfs.sessions().iter().any(|session| {
            &session.attachment == name && session.runtime_instance == runtime_instance
        }) {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero()
            || tokio::time::timeout(remaining, changed.changed())
                .await
                .is_err()
        {
            anyhow::bail!("exact VFS session remained after runtime stop");
        }
    }
}

fn session(
    name: &ResourceName,
    spec: &AttachmentSpec,
    runtime_instance: &str,
) -> omnifs_vfs::Session {
    omnifs_vfs::Session {
        attachment: name.clone(),
        spec: spec.clone(),
        runtime_instance: runtime_instance.to_owned(),
    }
}

enum ConfirmedSession {
    Attached,
    Stopped(ConfirmedRuntime),
    Absent,
    TimedOut(ConfirmedRuntime),
}

async fn wait_for_confirmed_session(
    context: &ReconcileContext,
    driver: &RuntimeDriver,
    expected: &omnifs_vfs::Session,
    runtime_instance: &str,
) -> anyhow::Result<ConfirmedSession> {
    let deadline = tokio::time::Instant::now() + SESSION_WAIT;
    loop {
        if context
            .vfs
            .wait_for_session(expected, SESSION_LIVENESS_POLL)
            .await
        {
            return Ok(ConfirmedSession::Attached);
        }
        match driver.confirmed(runtime_instance).await? {
            Some(current) if current.is_running() => {
                if tokio::time::Instant::now() >= deadline {
                    return Ok(ConfirmedSession::TimedOut(current));
                }
            },
            Some(stopped) => return Ok(ConfirmedSession::Stopped(stopped)),
            None => return Ok(ConfirmedSession::Absent),
        }
    }
}

fn namespace_ready(progress: &ProgressHub, revision: ResourceRevision) -> bool {
    let (_, snapshot) = progress.snapshot_for(ProgressTarget::Current);
    snapshot.serving.as_ref().is_some_and(|serving| {
        serving.revision >= revision
            && matches!(
                serving.stage,
                omnifs_api::ServingProgressStage::Draining
                    | omnifs_api::ServingProgressStage::Degraded
                    | omnifs_api::ServingProgressStage::Ready
            )
    })
}

async fn clear_observed(
    context: &ReconcileContext,
    current_revision: ResourceRevision,
    work: &mut Work,
) -> anyhow::Result<bool> {
    let updated = update_observation(&context.state, &mut work.instance, |observation| {
        observation.observed_version = None;
        observation.observed_spec = None;
        observation.runtime_instance = None;
        observation.phase = AttachmentPhase::Pending;
        observation.retry_at = None;
    })
    .await?;
    if updated {
        context.resources.mark_attachment_phase(
            current_revision,
            &work.name,
            ResourcePhase::Pending,
            None,
            None,
        );
    }
    Ok(updated)
}

async fn update_observation(
    state: &StateStore,
    instance: &mut AttachmentInstance,
    update: impl FnOnce(&mut AttachmentObservation),
) -> anyhow::Result<bool> {
    let mut observation = AttachmentObservation::from_instance(instance);
    update(&mut observation);
    let Some(updated) = state.write_attachment_observation(observation).await? else {
        return Ok(false);
    };
    *instance = updated;
    Ok(true)
}

fn random_runtime_instance() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generate attachment runtime instance")?;
    Ok(hex::encode(bytes))
}

fn retry_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    RETRY_BASE
        .checked_mul(1_u32 << shift)
        .unwrap_or(RETRY_CAP)
        .min(RETRY_CAP)
}

fn unix_seconds() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}

fn unix_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn publish_revision_ready(progress: &ProgressHub, revision: ResourceRevision) {
    progress.publish(
        ProgressTarget::DesiredRevision(revision),
        ProgressEventKind::RevisionReady(revision),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::{
        AttachmentDefinition, NormalizedResourceSet, ProgressSnapshot, ResourceDefinition,
    };
    use omnifs_core::{ActionId, AttachmentSpec, MutationId};
    use omnifs_state::{AttachmentActionRequest, ResourceApplyRequest, StateStoreOptions};
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use tokio::sync::broadcast;

    type TestFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

    /// A no-op namespace is enough for supervisor tests that stop before a
    /// runtime launch. It keeps the tests on the production `VfsServer` and
    /// durable state paths without an OS mount or an attach listener.
    struct EmptyNamespace {
        events: broadcast::Sender<omnifs_vfs::NsEvent>,
    }

    impl EmptyNamespace {
        fn new() -> Arc<Self> {
            let (events, _) = broadcast::channel(1);
            Arc::new(Self { events })
        }
    }

    impl omnifs_vfs::Namespace for EmptyNamespace {
        fn lookup<'a>(
            &'a self,
            _parent: omnifs_core::path::Path,
            _name: &'a str,
        ) -> TestFuture<'a, Result<omnifs_vfs::LookupAnswer, omnifs_vfs::NsError>> {
            Box::pin(async { Err(omnifs_vfs::NsError::Network) })
        }

        fn getattr(
            &self,
            _path: omnifs_core::path::Path,
        ) -> TestFuture<'_, Result<omnifs_vfs::Attrs, omnifs_vfs::NsError>> {
            Box::pin(async { Err(omnifs_vfs::NsError::Network) })
        }

        fn getattr_exact(
            &self,
            _path: omnifs_core::path::Path,
        ) -> TestFuture<'_, Result<omnifs_vfs::Attrs, omnifs_vfs::NsError>> {
            Box::pin(async { Err(omnifs_vfs::NsError::Network) })
        }

        fn readdir(
            &self,
            _path: omnifs_core::path::Path,
            _cursor: omnifs_vfs::DirCursor,
            _budget: usize,
        ) -> TestFuture<'_, Result<omnifs_vfs::DirPage, omnifs_vfs::NsError>> {
            Box::pin(async { Err(omnifs_vfs::NsError::Network) })
        }

        fn read(
            &self,
            _path: omnifs_core::path::Path,
            _offset: u64,
            _len: u32,
        ) -> TestFuture<'_, Result<omnifs_vfs::ReadAnswer, omnifs_vfs::NsError>> {
            Box::pin(async { Err(omnifs_vfs::NsError::Network) })
        }

        fn readlink(
            &self,
            _path: omnifs_core::path::Path,
        ) -> TestFuture<'_, Result<PathBuf, omnifs_vfs::NsError>> {
            Box::pin(async { Err(omnifs_vfs::NsError::Network) })
        }

        fn subscribe(&self) -> omnifs_vfs::EventStream {
            omnifs_vfs::EventStream::from_broadcast(self.events.subscribe())
        }
    }

    struct EmptyServingNamespace {
        namespace: Arc<EmptyNamespace>,
        events: Arc<omnifs_vfs::NamespaceEventHub>,
        cancellation: watch::Sender<bool>,
    }

    impl EmptyServingNamespace {
        fn new() -> Arc<Self> {
            let epoch = omnifs_vfs::NamespaceEpoch::initial([0x55; 16]);
            Arc::new(Self {
                namespace: EmptyNamespace::new(),
                events: omnifs_vfs::NamespaceEventHub::new(epoch, 1),
                cancellation: watch::channel(false).0,
            })
        }
    }

    impl omnifs_vfs::ServingNamespace for EmptyServingNamespace {
        fn acquire(&self) -> Result<omnifs_vfs::NamespaceLease, omnifs_vfs::NsError> {
            Ok(omnifs_vfs::NamespaceLease::new(
                self.current_epoch(),
                self.namespace.clone(),
                (),
                self.cancellation.subscribe(),
            ))
        }

        fn subscribe(&self) -> omnifs_vfs::NamespaceSubscription {
            self.events.subscribe()
        }

        fn current_epoch(&self) -> omnifs_vfs::NamespaceEpoch {
            self.events.current_epoch()
        }
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        state: Arc<StateStore>,
        resources: Arc<ResourceControl>,
        context: ReconcileContext,
        name: ResourceName,
    }

    impl Fixture {
        async fn work(&self) -> Work {
            let desired = self
                .state
                .desired_attachments()
                .await
                .unwrap()
                .into_iter()
                .find(|attachment| attachment.definition.name == self.name);
            let instance = self
                .state
                .attachment_instance(&self.name)
                .await
                .unwrap()
                .unwrap_or_else(|| AttachmentInstance::pending(self.name.clone()));
            Work {
                name: self.name.clone(),
                desired,
                instance,
                action: None,
                retry_count: 0,
            }
        }

        async fn apply(&self, desired: NormalizedResourceSet, mutation: u8) -> ResourceRevision {
            let snapshot = self.state.resource_snapshot().await.unwrap();
            self.state
                .apply_resources(ResourceApplyRequest {
                    mutation_id: MutationId::from_bytes([mutation; 16]),
                    base_revision: snapshot.revision,
                    expected_desired_digest: desired.digest(),
                    desired,
                    credential_secrets: Vec::new(),
                })
                .await
                .unwrap()
                .revision
        }
    }

    async fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let paths = omnifs_state::DaemonStatePaths::new(temp.path().join("daemon-state"));
        let state = Arc::new(
            StateStore::open(paths, StateStoreOptions::default())
                .await
                .unwrap(),
        );
        let name = ResourceName::new("supervisor-test").unwrap();
        let desired = attachment_set(name.clone(), PathBuf::from("/tmp/omnifs-supervisor-test"));
        let snapshot = state.resource_snapshot().await.unwrap();
        state
            .apply_resources(ResourceApplyRequest {
                mutation_id: MutationId::from_bytes([0x51; 16]),
                base_revision: snapshot.revision,
                expected_desired_digest: desired.digest(),
                desired,
                credential_secrets: Vec::new(),
            })
            .await
            .unwrap();
        let resources = ResourceControl::new(Arc::clone(&state), "attachment-supervisor-test")
            .await
            .unwrap();
        let root = temp.path().join("runtime");
        let context = ReconcileContext {
            state: Arc::clone(&state),
            resources: Arc::clone(&resources),
            vfs: omnifs_vfs::VfsServer::new(EmptyServingNamespace::new()),
            paths: RuntimePaths::daemon_owned(
                temp.path().to_path_buf(),
                false,
                root.join("attachments"),
                root.join("logs"),
                root.join("images"),
                PathBuf::from("/usr/bin/false"),
            ),
            endpoints: AttachEndpoints::default(),
            launch_slots: Arc::new(Semaphore::new(MAX_ACTIVE_ATTACHMENTS)),
            queued: Arc::new(AtomicU32::new(0)),
            active: Arc::new(AtomicU32::new(0)),
        };
        Fixture {
            _temp: temp,
            state,
            resources,
            context,
            name,
        }
    }

    fn attachment_set(name: ResourceName, location: PathBuf) -> NormalizedResourceSet {
        let protocol = if cfg!(target_os = "linux") {
            omnifs_core::AttachmentProtocol::Fuse
        } else {
            omnifs_core::AttachmentProtocol::Nfs
        };
        let spec =
            AttachmentSpec::new(protocol, AttachmentRuntime::Host, location, None, None).unwrap();
        NormalizedResourceSet::new(vec![ResourceDefinition::Attachment(AttachmentDefinition {
            name,
            spec,
        })])
        .unwrap()
    }

    #[test]
    fn retry_backoff_is_capped() {
        assert_eq!(retry_delay(1), Duration::from_millis(250));
        assert_eq!(retry_delay(2), Duration::from_millis(500));
        assert_eq!(retry_delay(3), Duration::from_secs(1));
        assert_eq!(retry_delay(99), RETRY_CAP);
    }

    #[test]
    fn runtime_events_never_invent_byte_totals() {
        let (stage, completed, total) = runtime_progress(&RuntimeEvent::Download {
            artifact: omnifs_fs_runtime::Artifact::FilesystemImage,
            completed_bytes: 41,
            total_bytes: None,
            source: "registry".to_owned(),
        });
        assert_eq!(stage, Some(AttachmentProgressStage::PullingImage));
        assert_eq!(completed, 41);
        assert_eq!(total, None);
    }

    #[test]
    fn newer_serving_generation_satisfies_an_unchanged_attachment() {
        let progress = ProgressHub::new(
            "attachment-supervisor-test",
            ProgressSnapshot {
                desired_revision: ResourceRevision::new(2),
                observed_revision: Some(ResourceRevision::new(2)),
                resources: Vec::new(),
                actions: Vec::new(),
                providers: Vec::new(),
                serving: Some(omnifs_api::ServingProgress {
                    revision: ResourceRevision::new(2),
                    stage: omnifs_api::ServingProgressStage::Ready,
                    completed: 1,
                    total: 1,
                    error_code: None,
                    detail: None,
                    queued_generations: 0,
                    retry_count: 0,
                    next_retry_unix_ms: None,
                }),
                credentials: Vec::new(),
                attachments: Vec::new(),
            },
        );

        assert!(namespace_ready(&progress, ResourceRevision::new(1)));
    }

    #[tokio::test]
    async fn waits_for_a_matching_serving_revision_before_runtime_work() {
        let fixture = fixture().await;
        let mut work = fixture.work().await;
        let revision = fixture.state.resource_snapshot().await.unwrap().revision;

        assert!(matches!(
            reconcile_one_active(&fixture.context, revision, &mut work)
                .await
                .unwrap(),
            WorkOutcome::Done
        ));

        let instance = fixture
            .state
            .attachment_instance(&fixture.name)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(instance.phase, AttachmentPhase::WaitingForNamespace);
        assert!(instance.runtime_instance.is_none());
        let (_, progress) = fixture
            .resources
            .progress()
            .snapshot_for(ProgressTarget::DesiredRevision(revision));
        assert!(progress.attachments.iter().any(|attachment| {
            attachment.key.name == fixture.name
                && attachment.stage == AttachmentProgressStage::WaitingForNamespace
        }));
        fixture.state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retained_attachment_readiness_and_loss_track_the_current_restart_revision() {
        let fixture = fixture().await;
        let work = fixture.work().await;
        let desired = work.desired.unwrap();
        let current_revision = desired.revision.next().unwrap();
        fixture.resources.progress().publish_snapshot(
            ProgressTarget::DesiredRevision(current_revision),
            ProgressSnapshot {
                desired_revision: current_revision,
                observed_revision: None,
                resources: vec![omnifs_api::ResourceStatus {
                    key: desired.definition.key(),
                    desired_revision: current_revision,
                    observed_revision: None,
                    phase: ResourcePhase::Pending,
                    error_code: None,
                    detail: None,
                }],
                actions: Vec::new(),
                providers: Vec::new(),
                serving: None,
                credentials: Vec::new(),
                attachments: Vec::new(),
            },
        );

        finish_ready(
            &fixture.context,
            &desired,
            None,
            &work.instance,
            current_revision,
        )
        .await
        .unwrap();

        let (_, snapshot) = fixture
            .resources
            .progress()
            .snapshot_for(ProgressTarget::DesiredRevision(current_revision));
        assert_eq!(snapshot.observed_revision, Some(current_revision));
        assert_eq!(snapshot.resources[0].phase, ResourcePhase::Ready);
        assert_eq!(
            snapshot.resources[0].observed_revision,
            Some(current_revision)
        );

        fixture.resources.mark_attachment_phase(
            current_revision,
            &desired.definition.name,
            ResourcePhase::Pending,
            None,
            None,
        );
        let (_, snapshot) = fixture
            .resources
            .progress()
            .snapshot_for(ProgressTarget::DesiredRevision(current_revision));
        assert_eq!(snapshot.observed_revision, None);
        assert_eq!(snapshot.resources[0].phase, ResourcePhase::Pending);
        assert_eq!(snapshot.resources[0].observed_revision, None);
        assert_eq!(
            fixture
                .resources
                .progress()
                .target_state(ProgressTarget::DesiredRevision(current_revision)),
            crate::progress::ProgressTargetState::Watching
        );
        fixture.state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn deleting_an_attachment_finishes_its_pending_restart_action() {
        let fixture = fixture().await;
        let action_id = ActionId::from_bytes([0x61; 16]);
        let accepted = fixture
            .state
            .accept_attachment_action(AttachmentActionRequest {
                action_id,
                attachment: fixture.name.clone(),
                base_action_generation: 0,
            })
            .await
            .unwrap();
        let revision = fixture.apply(NormalizedResourceSet::empty(), 0x62).await;
        let mut work = fixture.work().await;
        work.action = Some(accepted);
        assert!(work.desired.is_none());

        assert!(matches!(
            reconcile_deletion(&fixture.context, revision, &mut work)
                .await
                .unwrap(),
            WorkOutcome::Done
        ));

        let receipt = fixture
            .state
            .action_receipt(action_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.phase, ActionPhase::Failed);
        assert_eq!(receipt.error_code.as_deref(), Some("attachment_deleted"));
        assert!(fixture.state.pending_actions().await.unwrap().is_empty());
        assert!(
            fixture
                .state
                .attachment_instance(&fixture.name)
                .await
                .unwrap()
                .is_none()
        );
        fixture.state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retry_limit_records_a_terminal_failure_without_more_attempts() {
        let fixture = fixture().await;
        let mut work = fixture.work().await;
        let desired = work.desired.clone().unwrap();
        for attempt in 1..MAX_RETRY_ATTEMPTS {
            work.retry_count = attempt - 1;
            assert!(matches!(
                retry_or_fail(
                    &fixture.context,
                    desired.revision,
                    &desired,
                    &mut work,
                    "attachment_launch_failed",
                    "the attachment runtime could not start",
                )
                .await
                .unwrap(),
                WorkOutcome::Retry
            ));
            assert_eq!(work.instance.phase, AttachmentPhase::Retrying);
            assert!(work.instance.retry_at.is_some());
        }
        work.retry_count = MAX_RETRY_ATTEMPTS - 1;
        assert!(matches!(
            retry_or_fail(
                &fixture.context,
                desired.revision,
                &desired,
                &mut work,
                "attachment_launch_failed",
                "the attachment runtime could not start",
            )
            .await
            .unwrap(),
            WorkOutcome::Done
        ));
        let instance = fixture
            .state
            .attachment_instance(&fixture.name)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(instance.phase, AttachmentPhase::Failed);
        assert_eq!(
            instance.last_error_code.as_deref(),
            Some("attachment_launch_failed")
        );
        assert_eq!(instance.retry_at, None);
        fixture.state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn superseded_work_cannot_publish_a_stale_retry_observation() {
        let fixture = fixture().await;
        let mut stale_work = fixture.work().await;
        let replacement = attachment_set(
            fixture.name.clone(),
            PathBuf::from("/tmp/omnifs-supervisor-test-replaced"),
        );
        fixture.apply(replacement, 0x63).await;
        let stale_desired = stale_work.desired.clone().unwrap();

        assert!(matches!(
            retry_or_fail(
                &fixture.context,
                stale_desired.revision,
                &stale_desired,
                &mut stale_work,
                "attachment_launch_failed",
                "the attachment runtime could not start",
            )
            .await
            .unwrap(),
            WorkOutcome::Done
        ));

        let instance = fixture
            .state
            .attachment_instance(&fixture.name)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(instance.phase, AttachmentPhase::Pending);
        assert_eq!(instance.last_error_code, None);
        assert_eq!(instance.retry_at, None);
        assert_ne!(
            instance.desired_version,
            stale_work.instance.desired_version
        );
        fixture.state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stop_all_joins_without_deleting_desired_attachments() {
        let fixture = fixture().await;
        let supervisor = AttachmentSupervisor::spawn(
            Arc::clone(&fixture.state),
            Arc::clone(&fixture.resources),
            Arc::clone(&fixture.context.vfs),
            fixture.context.paths.clone(),
            fixture.context.endpoints.clone(),
        );

        let stopped = tokio::time::timeout(Duration::from_secs(1), supervisor.stop_all())
            .await
            .expect("stop-all must not wait for a namespace")
            .unwrap();
        assert!(stopped.is_empty());
        assert_eq!(fixture.state.desired_attachments().await.unwrap().len(), 1);
        supervisor.shutdown().await.unwrap();
        assert_eq!(fixture.state.desired_attachments().await.unwrap().len(), 1);
        fixture.state.shutdown().await.unwrap();
    }
}
