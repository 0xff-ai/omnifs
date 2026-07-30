//! Daemon entrypoint and lifecycle owner.

use anyhow::Context as _;
use omnifs_api::{
    ControlError, ControlErrorCode, DaemonPhase, DaemonRecovery, HealthReport, HealthState,
    RecoveryId, RecoveryOffer, RepairAction, RepairDisposition, RepairReceipt,
};
use omnifs_core::{MountRevision, MutationId};
use omnifs_engine::{HostOnline, HostRuntimeOpen, Inspector, ServingCell, init_global_from_env};
use omnifs_state::{ControlStoreRepairDisposition, StateStore, StateStoreOptions};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, warn};

use crate::{
    context::DaemonContext,
    control::{ControlServer, RepairCommand},
    daemon::{Daemon, DaemonParts},
    generation_builder::{GenerationDraft, GenerationParts},
    logging,
    manager::MutationManager,
    provider_bundle::EmbeddedProviders,
};

/// Distinguishes successive repair offers so a stale one cannot be replayed.
/// Only distinctness matters: the offer is already behind a uid-verified
/// socket, and `repair_state` checks `instance_id` separately.
static RECOVERY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn next_recovery_id() -> RecoveryId {
    let mut id = [0_u8; 16];
    id[..8].copy_from_slice(
        &RECOVERY_SEQUENCE
            .fetch_add(1, Ordering::Relaxed)
            .to_le_bytes(),
    );
    RecoveryId::from_bytes(id)
}

/// Bind control first, then keep it available through startup and recovery.
pub async fn run() -> anyhow::Result<()> {
    let context = Arc::new(DaemonContext::resolve()?);
    logging::verify_resolved_profile(&context)?;
    let embedded = Arc::new(EmbeddedProviders::load()?);
    context.prepare_startup_dirs()?;
    let control_listener = context.bind_control_socket()?;
    if let Err(error) = context
        .endpoint()
        .write_process_identity(context.process_identity())
    {
        remove_control_socket(&context);
        return Err(error.into());
    }

    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let (control, mut repairs) = ControlServer::new(
        Arc::clone(&context),
        Arc::clone(&embedded),
        shutdown_tx.clone(),
    );
    let control_task = tokio::spawn(Arc::clone(&control).run(control_listener));
    let signal_task = spawn_signal_task(shutdown_tx.clone());
    let inspector = start_inspector();

    let (result, ready_daemon) = serve_until_stopped(
        &context,
        &embedded,
        &control,
        &shutdown_tx,
        &mut repairs,
        inspector,
    )
    .await;

    control.set_shutting_down();
    // The only send that can unblock `control_task.await` below. A fatal
    // namespace listener exit inside `Daemon::supervise` returns an error
    // without anyone having signalled shutdown, and `Daemon::shutdown`
    // deliberately does not broadcast, so without this send the control
    // server's select loop never notices and the join never returns.
    let _ = shutdown_tx.send(true);
    signal_task.abort();
    let _ = signal_task.await;
    let control_result = control_task
        .await
        .context("join control server")?
        .context("serve control socket");
    let daemon_result = match ready_daemon {
        Some(daemon) => daemon.shutdown().await,
        None => Ok(()),
    };
    unpublish_process_identity(&context);

    crate::first_error([result, control_result, daemon_result])
}

/// Open the store, build the runtime, and serve until stopped, re-entering
/// recovery for as long as startup keeps failing. Control is already bound and
/// answering before this is called, and stays that way across every recovery
/// round: that is the whole point of the loop.
async fn serve_until_stopped(
    context: &Arc<DaemonContext>,
    embedded: &Arc<EmbeddedProviders>,
    control: &Arc<ControlServer>,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
    repairs: &mut tokio::sync::mpsc::Receiver<RepairCommand>,
    inspector: Option<Arc<Inspector>>,
) -> (anyhow::Result<()>, Option<Arc<Daemon>>) {
    let mut store = match open_initial_store(context).await {
        Ok(store) => store,
        Err(failure) => {
            match recover_until_repaired(context, control, shutdown_tx, repairs, failure).await {
                RecoveryOutcome::Repaired(store) => store,
                RecoveryOutcome::Stop(result) => return (result, None),
            }
        },
    };
    loop {
        let failure = match build_daemon(
            Arc::clone(context),
            Arc::clone(embedded),
            inspector.clone(),
            shutdown_tx.clone(),
            store,
        )
        .await
        {
            Ok((daemon, listener_events)) => {
                control.set_ready(Arc::clone(&daemon));
                let result = daemon.supervise(listener_events).await;
                return (result, Some(daemon));
            },
            Err(failure) => failure,
        };
        store = match recover_until_repaired(context, control, shutdown_tx, repairs, failure).await
        {
            RecoveryOutcome::Repaired(next) => next,
            RecoveryOutcome::Stop(result) => return (result, None),
        };
    }
}

fn start_inspector() -> Option<Arc<Inspector>> {
    let inspector = init_global_from_env();
    match inspector.as_ref().map(|inspector| inspector.tee_path()) {
        None => {},
        Some(Some(path)) => {
            info!(path = %path.display(), "inspector stream enabled (in-memory ring + file tee)");
        },
        Some(None) => info!("inspector stream enabled (in-memory ring only)"),
    }
    inspector
}

/// Remove this process's published identity, but only if it is still ours.
fn unpublish_process_identity(context: &DaemonContext) {
    match context
        .endpoint()
        .remove_published_bootstrap_if(context.process_identity())
    {
        Ok(true) => {},
        Ok(false) => warn!(
            path = %context.endpoint().process_identity_path().display(),
            "daemon process identity changed before cleanup; leaving it intact"
        ),
        Err(error) => warn!(
            %error,
            path = %context.endpoint().process_identity_path().display(),
            "failed to remove daemon process identity"
        ),
    }
}

/// Open the control store for the very first startup attempt of this
/// process. Later attempts, after a repair, feed [`build_daemon`] the store
/// [`recover_until_repaired`] hands back instead of calling this again.
async fn open_initial_store(context: &DaemonContext) -> Result<StateStore, StartupFailure> {
    StateStore::open(context.endpoint(), StateStoreOptions::default())
        .await
        .map_err(StartupFailure::store)
}

/// What the recovery loop produced: either a store repaired well enough to
/// retry startup with, or a reason to stop the whole lifecycle.
enum RecoveryOutcome {
    Repaired(StateStore),
    Stop(anyhow::Result<()>),
}

/// Publish `failure` as the daemon's recovery state and wait for either a
/// shutdown request or a successful repair, retrying repairs that fail.
/// Control stays bound and answering throughout: this only touches
/// `control`'s recovery phase and the `repairs` channel it already owns.
async fn recover_until_repaired(
    context: &DaemonContext,
    control: &ControlServer,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
    repairs: &mut tokio::sync::mpsc::Receiver<RepairCommand>,
    mut failure: StartupFailure,
) -> RecoveryOutcome {
    loop {
        warn!(error = %failure.error(), "daemon entered recovery");
        control.set_recovery(failure.recovery());
        let mut shutdown = shutdown_tx.subscribe();
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    return RecoveryOutcome::Stop(Ok(()));
                }
            },
            command = repairs.recv() => {
                let Some(command) = command else {
                    return RecoveryOutcome::Stop(Err(anyhow::anyhow!(
                        "control repair queue closed"
                    )));
                };
                match repair_store(context, command).await {
                    Ok(store) => return RecoveryOutcome::Repaired(store),
                    Err(next) => failure = next,
                }
            },
        }
    }
}

async fn build_daemon(
    context: Arc<DaemonContext>,
    embedded: Arc<EmbeddedProviders>,
    inspector: Option<Arc<Inspector>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    state: StateStore,
) -> Result<
    (
        Arc<Daemon>,
        tokio::sync::broadcast::Receiver<omnifs_vfs::ListenerEvent>,
    ),
    StartupFailure,
> {
    let state = Arc::new(state);
    let paths = state.engine_paths();
    let host = match HostOnline::open_runtime(HostRuntimeOpen {
        projection: paths.projection_cache().to_path_buf(),
        wasmtime: paths.wasmtime_cache().to_path_buf(),
        clones: paths.clone_cache().to_path_buf(),
    }) {
        Ok(host) => Arc::new(host),
        Err(error) => return Err(close_failed_state(state, error.into()).await),
    };
    let draft = match GenerationDraft::load(&state).await {
        Ok(draft) => draft,
        Err(error) => return Err(close_failed_state(state, error).await),
    };
    let build = match draft.prepare(&state, &host).await {
        Ok(build) => build,
        Err(error) => return Err(close_failed_state(state, error).await),
    };
    let GenerationParts {
        ready,
        revision,
        pending_refreshes,
    } = build.into_parts();
    let serving = ServingCell::new(context.namespace_epoch().daemon_instance(), ready);
    if let Err(error) = pending_refreshes.activate(&state).await {
        return Err(close_failed_state(state, error).await);
    }
    if let Err(error) = state.mark_serving(revision).await {
        return Err(close_failed_state(state, error).await);
    }
    // The manager's spawned task owns the only live `HostOnline` handle for
    // the daemon's lifetime; nothing else needs to anchor it.
    let manager = MutationManager::spawn(Arc::clone(&state), host, Arc::clone(&serving));
    let daemon = Arc::new(Daemon::new(DaemonParts {
        context,
        embedded,
        state: Arc::clone(&state),
        serving,
        manager,
        inspector,
        shutdown_tx,
    }));
    match daemon.start().await {
        Ok(events) => Ok((daemon, events)),
        Err(error) => {
            let failure = runtime_failure(&state, error).await;
            if let Err(shutdown_error) = daemon.shutdown().await {
                warn!(%shutdown_error, "failed to close rejected daemon runtime");
            }
            Err(failure)
        },
    }
}

async fn repair_store(
    context: &DaemonContext,
    command: RepairCommand,
) -> Result<StateStore, StartupFailure> {
    let RepairCommand {
        recovery_id,
        action,
        reply,
    } = command;
    let error =
        match StateStore::recreate_control_store(context.endpoint(), StateStoreOptions::default())
            .await
        {
            Ok((store, disposition)) => {
                let disposition = match disposition {
                    ControlStoreRepairDisposition::FreshStoreCreated => {
                        RepairDisposition::FreshStoreCreated
                    },
                    ControlStoreRepairDisposition::CorruptStoreArchived => {
                        RepairDisposition::CorruptStoreArchived
                    },
                };
                let receipt = RepairReceipt {
                    instance_id: context.instance_id().to_owned(),
                    recovery_id,
                    action,
                    disposition,
                };
                let _ = reply.send(Ok(receipt));
                return Ok(store);
            },
            Err(error) => error,
        };
    let _ = reply.send(Err(ControlError::new(
        ControlErrorCode::Internal,
        "control store repair failed",
    )));
    Err(StartupFailure::store(error))
}

async fn close_failed_state(state: Arc<StateStore>, error: anyhow::Error) -> StartupFailure {
    let failure = runtime_failure(&state, error).await;
    if let Err(close_error) = state.shutdown().await {
        warn!(%close_error, "failed to close rejected StateStore");
    }
    failure
}

async fn runtime_failure(state: &StateStore, error: anyhow::Error) -> StartupFailure {
    let durable_revision = state.mount_revision().await.ok();
    let serving = state.serving_state().await.ok();
    StartupFailure::Runtime {
        error,
        durable_revision,
        serving_revision: serving.as_ref().map(|state| state.revision),
        failed_mutation: serving.and_then(|state| state.failed_mutation),
    }
}

/// Why startup did not reach `Ready`: either the control store itself could
/// not be opened (repairable in place), or it opened fine but something
/// downstream of it rejected the daemon runtime (not repairable the same
/// way, so no repair offer is made for it).
enum StartupFailure {
    Store(anyhow::Error),
    Runtime {
        error: anyhow::Error,
        durable_revision: Option<MountRevision>,
        serving_revision: Option<MountRevision>,
        failed_mutation: Option<MutationId>,
    },
}

impl StartupFailure {
    fn store(error: anyhow::Error) -> Self {
        Self::Store(error)
    }

    fn error(&self) -> &anyhow::Error {
        match self {
            Self::Store(error) | Self::Runtime { error, .. } => error,
        }
    }

    fn recovery(&self) -> DaemonRecovery {
        match self {
            Self::Store(_) => DaemonRecovery {
                phase: DaemonPhase::RecoveryRequired,
                durable_revision: None,
                serving_revision: None,
                failed_mutation: None,
                store_health: HealthReport::new(
                    HealthState::Unhealthy,
                    "control store could not be opened",
                ),
                repair: Some(RecoveryOffer {
                    id: next_recovery_id(),
                    actions: vec![RepairAction::RecreateControlStore],
                }),
            },
            Self::Runtime {
                durable_revision,
                serving_revision,
                failed_mutation,
                ..
            } => DaemonRecovery {
                phase: DaemonPhase::RecoveryRequired,
                durable_revision: *durable_revision,
                serving_revision: *serving_revision,
                failed_mutation: *failed_mutation,
                store_health: HealthReport::new(
                    HealthState::Unhealthy,
                    "daemon runtime could not start",
                ),
                repair: None,
            },
        }
    }
}

fn spawn_signal_task(shutdown_tx: tokio::sync::watch::Sender<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let Ok(mut term) = signal(SignalKind::terminate()) else {
                return;
            };
            let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
                return;
            };
            tokio::select! {
                _ = term.recv() => info!(signal = "SIGTERM", "received shutdown signal"),
                _ = interrupt.recv() => info!(signal = "SIGINT", "received shutdown signal"),
            }
            let _ = shutdown_tx.send(true);
        }
    })
}

fn remove_control_socket(context: &DaemonContext) {
    if let Err(error) = std::fs::remove_file(context.control_socket())
        && error.kind() != std::io::ErrorKind::NotFound
    {
        warn!(%error, "failed to remove control socket");
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use hyper_util::rt::TokioIo;
    use omnifs_api::CONTROL_REQUEST_TIMEOUT_SECS;
    use omnifs_api::RepairDisposition;
    use omnifs_api::grpc::{self, wire};
    use prost::Message as _;
    use std::time::Duration;
    use tonic::transport::{Channel, Endpoint};
    use tonic::{Code, Request, Status};
    use tower::service_fn;

    type ControlClient = wire::control_client::ControlClient<Channel>;
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(CONTROL_REQUEST_TIMEOUT_SECS);

    fn unary<T>(message: T) -> Request<T> {
        let mut request = Request::new(message);
        request.set_timeout(REQUEST_TIMEOUT);
        request
    }

    fn transient(status: &Status) -> bool {
        matches!(
            status.code(),
            Code::Unavailable | Code::DeadlineExceeded | Code::ResourceExhausted
        )
    }

    fn readiness_retryable(status: &Status) -> bool {
        if transient(status) {
            return true;
        }
        status.code() == Code::FailedPrecondition
            && wire::ErrorDetail::decode(status.details())
                .ok()
                .and_then(|detail| grpc::error_detail(&detail).ok())
                .is_some_and(|error| error.code == omnifs_api::ControlErrorCode::NotReady)
    }

    async fn client(path: &std::path::Path) -> anyhow::Result<ControlClient> {
        let path = path.to_owned();
        let endpoint = Endpoint::from_static("http://[::]:50051").connect_timeout(REQUEST_TIMEOUT);
        let future = endpoint.connect_with_connector(service_fn(move |_| {
            let path = path.clone();
            async move {
                tokio::net::UnixStream::connect(path)
                    .await
                    .map(TokioIo::new)
            }
        }));
        let channel = tokio::time::timeout(REQUEST_TIMEOUT, future)
            .await
            .map_err(|_| anyhow::anyhow!("control HTTP/2 setup timed out"))??;
        Ok(ControlClient::new(channel))
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(unsafe_code)]
    #[allow(clippy::too_many_lines)]
    async fn corrupt_store_stays_control_visible_and_repairs_in_process() {
        let _env_guard = crate::ENV_LOCK.lock().await;
        let temp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(temp.path()).unwrap();
        unsafe {
            std::env::set_var("OMNIFS_HOME", &home);
        }
        let control_store = home.join("daemon-state/control-store");
        std::fs::create_dir_all(&control_store).unwrap();
        std::fs::write(control_store.join("state.sqlite3"), b"not sqlite").unwrap();

        let daemon = tokio::spawn(async { super::run().await });
        let socket = home.join("control.sock");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let (instance_id, recovery_id) = loop {
            if let Ok(mut control) = client(&socket).await {
                let info = match control.get_daemon_info(unary(wire::Empty {})).await {
                    Ok(response) => Some(
                        grpc::daemon_info(
                            &response.into_inner().info.expect("missing daemon info"),
                        )
                        .expect("invalid daemon info"),
                    ),
                    Err(status) if transient(&status) => None,
                    Err(status) => panic!("daemon info request failed during recovery: {status}"),
                };
                let recovery = match control.get_recovery_state(unary(wire::Empty {})).await {
                    Ok(response) => Some(
                        grpc::daemon_recovery(
                            &response
                                .into_inner()
                                .recovery
                                .expect("missing daemon recovery"),
                        )
                        .expect("invalid daemon recovery"),
                    ),
                    Err(status) if transient(&status) => None,
                    Err(status) => {
                        panic!("daemon recovery request failed during recovery: {status}")
                    },
                };
                if let (Some(info), Some(recovery)) = (info, recovery)
                    && let Some(offer) = recovery.repair
                {
                    break (info.instance_id, offer.id);
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "daemon never exposed repairable recovery state"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };

        let mut control = client(&socket).await.unwrap();
        let repaired = control
            .repair_state(unary(wire::RepairStateRequest {
                instance_id,
                recovery_id: Bytes::copy_from_slice(recovery_id.as_bytes()),
                action: wire::RepairAction::RepairRecreateControlStore as i32,
            }))
            .await
            .unwrap()
            .into_inner()
            .receipt
            .and_then(|receipt| grpc::repair_receipt(&receipt).ok())
            .expect("repair receipt");
        assert!(matches!(
            repaired,
            omnifs_api::RepairReceipt {
                recovery_id: id,
                disposition: RepairDisposition::CorruptStoreArchived,
                ..
            } if id == recovery_id
        ));

        loop {
            if let Ok(mut control) = client(&socket).await {
                match control.ready(unary(wire::Empty {})).await {
                    Ok(_) => break,
                    Err(status) if readiness_retryable(&status) => {},
                    Err(status) => {
                        panic!("daemon readiness request failed after repair: {status}")
                    },
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "daemon never became ready after repair"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut control = client(&socket).await.unwrap();
        control
            .shutdown(unary(wire::ShutdownRequest {
                stop_filesystems: false,
            }))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), daemon)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            std::fs::read_dir(home.join("daemon-state"))
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("control-store.corrupt."))
        );
    }
}
