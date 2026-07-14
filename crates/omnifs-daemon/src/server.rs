//! Control API server:
//! `/v1/{ready,status,providers,mounts,shutdown,events}`.
//!
//! Serves daemon runtime facts and shutdown, and the
//! inspector event stream over HTTP on the local control socket. See
//! `docs/contracts/50-control-plane.md`.

use anyhow::Context as _;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use omnifs_api::{
    ApiError, CredentialHealth, DaemonBackend, DaemonHealth, DaemonStatus, DaemonSubsystem,
    ErrorCode, FrontendAttachTargetReport, FrontendAttachTargetRequest,
    FrontendAttachTargetVsockReport, FrontendDelivery, FrontendInfo, FsType, HealthState,
    MountInfo, ProviderArtifact, ProviderSummary, ReadyInfo, StopReport, SubsystemHealth,
};
use omnifs_engine::{Inspector, MountRuntimes};
use omnifs_workspace::provider::{Catalog, CatalogError, Provider};
use omnifs_workspace::runtime_record::{AttachRecord, RuntimeRecord};
use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::{info, warn};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::context::DaemonContext;
use omnifs_vfs_wire::ListenerTarget;

#[derive(OpenApi)]
#[openapi(
    info(title = "omnifs daemon control API", version = "8.0"),
    components(schemas(
        ReadyInfo,
        ApiError,
        ErrorCode,
        DaemonStatus,
        DaemonHealth,
        SubsystemHealth,
        DaemonSubsystem,
        HealthState,
        FrontendInfo,
        FrontendDelivery,
        FsType,
        DaemonBackend,
        MountInfo,
        CredentialHealth,
        ProviderArtifact,
        ProviderSummary,
        StopReport,
        FrontendAttachTargetRequest,
        FrontendAttachTargetReport,
        FrontendAttachTargetVsockReport,
    ))
)]
struct ApiDoc;

/// A host address approved for the namespace attach listener. Loopback is
/// always valid. On native Linux, the only additional authority is the IPv4
/// address assigned to Docker's default `docker0` bridge.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AttachBindAddr(Ipv4Addr);

impl AttachBindAddr {
    pub(crate) const fn loopback() -> Self {
        Self(Ipv4Addr::LOCALHOST)
    }

    fn requested(candidate: Option<Ipv4Addr>) -> anyhow::Result<Self> {
        let candidate = candidate.unwrap_or(Ipv4Addr::LOCALHOST);
        if candidate == Ipv4Addr::LOCALHOST {
            return Ok(Self::loopback());
        }

        #[cfg(target_os = "linux")]
        if nix::ifaddrs::getifaddrs()
            .context("enumerate host network interfaces")?
            .any(|interface| {
                interface.interface_name == "docker0"
                    && interface
                        .address
                        .as_ref()
                        .and_then(nix::sys::socket::SockaddrStorage::as_sockaddr_in)
                        .is_some_and(|address| address.ip() == candidate)
            })
        {
            return Ok(Self(candidate));
        }

        anyhow::bail!(
            "attach listener may bind only to loopback or Linux's default Docker bridge gateway, not {candidate}"
        )
    }
}

/// The outcome of binding an attach transport. `NamespaceNotReady` is not an
/// error: it is the transient window before the VFS server exists.
pub(crate) enum AttachOutcome {
    Bound(omnifs_vfs_wire::ListenerTarget),
    NamespaceNotReady,
}

fn attach_record(target: &ListenerTarget) -> anyhow::Result<AttachRecord> {
    match target {
        ListenerTarget::Tcp { addr, token } => Ok(AttachRecord::Tcp {
            addr: addr.to_string(),
            token: token.clone(),
        }),
        ListenerTarget::Vsock { socket_path, token } => Ok(AttachRecord::Vsock {
            socket_path: socket_path.clone(),
            token: token.clone(),
        }),
        ListenerTarget::Local { .. } => {
            anyhow::bail!("local listener is not a durable attach target")
        },
    }
}

pub(crate) struct RuntimeRecordStore {
    path: PathBuf,
    record: Mutex<Option<RuntimeRecord>>,
    published: AtomicBool,
}

impl RuntimeRecordStore {
    pub(crate) fn new(path: PathBuf, record: RuntimeRecord) -> Arc<Self> {
        Arc::new(Self {
            path,
            record: Mutex::new(Some(record)),
            published: AtomicBool::new(false),
        })
    }

    pub(crate) fn publish(&self) -> anyhow::Result<()> {
        let guard = self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = guard.as_ref() else {
            anyhow::bail!("runtime record has already been removed");
        };
        record.write(&self.path)?;
        self.published.store(true, Ordering::Release);
        Ok(())
    }

    fn set_attach(&self, target: AttachRecord) -> anyhow::Result<()> {
        let mut guard = self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = guard.as_mut() else {
            anyhow::bail!("runtime record has already been removed")
        };
        let previous = record.clone();
        record.set_attach(target);
        if self.published.load(Ordering::Acquire)
            && let Err(error) = record.write(&self.path)
        {
            *record = previous;
            return Err(error).with_context(|| {
                format!(
                    "persist attach listener in runtime record {}",
                    self.path.display()
                )
            });
        }
        Ok(())
    }

    fn remove_attach(&self, target: &AttachRecord) -> anyhow::Result<()> {
        let mut guard = self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = guard.as_mut() else {
            anyhow::bail!("runtime record has already been removed")
        };
        let previous = record.clone();
        record.remove_attach(target);
        if record.attach == previous.attach {
            return Ok(());
        }
        if self.published.load(Ordering::Acquire)
            && let Err(error) = record.write(&self.path)
        {
            *record = previous;
            return Err(error).with_context(|| {
                format!(
                    "persist removed attach listener in runtime record {}",
                    self.path.display()
                )
            });
        }
        Ok(())
    }

    pub(crate) fn remove(&self) {
        let mut guard = self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let published = self.published.swap(false, Ordering::AcqRel);
        guard.take();
        if published && let Err(error) = RuntimeRecord::remove(&self.path) {
            warn!(%error, path = %self.path.display(), "failed to remove runtime record");
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TaskEvent {
    Control,
}

fn check_startup_events(
    events_rx: &mut tokio::sync::mpsc::UnboundedReceiver<TaskEvent>,
) -> anyhow::Result<()> {
    while let Ok(event) = events_rx.try_recv() {
        match event {
            TaskEvent::Control => anyhow::bail!("control API listener exited before readiness"),
        }
    }
    Ok(())
}

pub struct Daemon {
    context: DaemonContext,
    registry: Arc<MountRuntimes>,
    inspector: Option<Arc<Inspector>>,
    runtime_record: Arc<RuntimeRecordStore>,
    vfs: OnceLock<Arc<omnifs_vfs_wire::VfsServer>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    events_tx: OnceLock<tokio::sync::mpsc::UnboundedSender<TaskEvent>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    socket_paths: Mutex<Vec<PathBuf>>,
}

impl Daemon {
    pub(crate) fn new(
        context: DaemonContext,
        registry: Arc<MountRuntimes>,
        inspector: Option<Arc<Inspector>>,
        runtime_record: Arc<RuntimeRecordStore>,
    ) -> Self {
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        Self {
            context,
            registry,
            inspector,
            runtime_record,
            vfs: OnceLock::new(),
            shutdown_tx,
            events_tx: OnceLock::new(),
            tasks: Mutex::new(Vec::new()),
            socket_paths: Mutex::new(Vec::new()),
        }
    }

    fn send_event(&self, event: TaskEvent) {
        if let Some(sender) = self.events_tx.get() {
            let _ = sender.send(event);
        }
    }

    fn track_task(&self, task: tokio::task::JoinHandle<()>) {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
    }

    fn track_socket(&self, path: PathBuf) {
        self.socket_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(path);
    }

    fn cleanup_sockets(&self) {
        let paths = std::mem::take(
            &mut *self
                .socket_paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for path in paths {
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                warn!(%error, path = %path.display(), "failed to remove daemon socket");
            }
        }
    }

    async fn stop_tasks(&self) {
        let mut tasks = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for task in &tasks {
            task.abort();
        }
        while let Some(task) = tasks.pop() {
            let _ = task.await;
        }
    }

    /// Record the shared namespace once it is built after atomic startup load.
    /// A second call is a no-op: the namespace is built exactly once per daemon
    /// start.
    pub fn set_namespace(&self, namespace: Arc<omnifs_engine::TreeNamespace>) {
        let server =
            omnifs_vfs_wire::VfsServer::new(namespace, self.context.instance_id().to_string());
        let _ = self.vfs.set(server);
    }

    pub fn ensure_attach_tcp(
        &self,
        bind_addr: AttachBindAddr,
        port: u16,
    ) -> anyhow::Result<AttachOutcome> {
        if self.vfs.get().is_none_or(|vfs| !vfs.ready()) {
            return Ok(AttachOutcome::NamespaceNotReady);
        }
        self.ensure_attach_tcp_with_token(bind_addr, port, None)
    }

    fn ensure_attach_tcp_with_token(
        &self,
        bind_addr: AttachBindAddr,
        port: u16,
        requested_token: Option<String>,
    ) -> anyhow::Result<AttachOutcome> {
        let Some(vfs) = self.vfs.get() else {
            return Ok(AttachOutcome::NamespaceNotReady);
        };
        let (target, newly_bound) = vfs
            .ensure_tcp_with_status(bind_addr.0, port, requested_token)
            .context("bind namespace TCP listener")?;
        let record = match attach_record(&target) {
            Ok(record) => record,
            Err(error) => {
                if newly_bound {
                    vfs.remove_listener(&target);
                }
                return Err(error);
            },
        };
        if let Err(error) = self.runtime_record.set_attach(record) {
            if newly_bound {
                vfs.remove_listener(&target);
            }
            return Err(error);
        }
        Ok(AttachOutcome::Bound(target))
    }

    pub fn ensure_attach_uds(&self) -> anyhow::Result<AttachOutcome> {
        if self.vfs.get().is_none_or(|vfs| !vfs.ready()) {
            return Ok(AttachOutcome::NamespaceNotReady);
        }
        self.ensure_attach_uds_with_token(None)
    }

    fn ensure_attach_uds_with_token(
        &self,
        requested_token: Option<String>,
    ) -> anyhow::Result<AttachOutcome> {
        let Some(vfs) = self.vfs.get() else {
            return Ok(AttachOutcome::NamespaceNotReady);
        };
        let path = self.context.vsock_attach_socket();
        let (target, newly_bound) = vfs
            .ensure_vsock_with_status(path, requested_token)
            .context("bind namespace vsock listener")?;
        let record = match attach_record(&target) {
            Ok(record) => record,
            Err(error) => {
                if newly_bound {
                    vfs.remove_listener(&target);
                }
                return Err(error);
            },
        };
        if let Err(error) = self.runtime_record.set_attach(record) {
            if newly_bound {
                vfs.remove_listener(&target);
            }
            return Err(error);
        }
        Ok(AttachOutcome::Bound(target))
    }

    /// Own the daemon's complete serving lifetime. Startup binds every fixed
    /// listener, restores persisted dynamic authority, and publishes the new
    /// record only after all required listeners are alive. The same method owns
    /// task joins, provider shutdown, record removal, and socket cleanup.
    pub async fn run(self: Arc<Self>, previous: Option<RuntimeRecord>) -> anyhow::Result<()> {
        let result = self.run_inner(previous).await;
        let _ = self.shutdown_tx.send(true);
        self.stop_tasks().await;
        if let Some(vfs) = self.vfs.get() {
            vfs.shutdown().await;
        }
        self.registry.shutdown_all();
        self.runtime_record.remove();
        self.cleanup_sockets();
        result
    }

    async fn run_inner(self: &Arc<Self>, previous: Option<RuntimeRecord>) -> anyhow::Result<()> {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = self.events_tx.set(events_tx);
        let vfs = self.vfs.get().context("VFS server was not initialized")?;
        let listener_events = vfs.listener_events();
        let startup_gate = vfs.begin_startup();
        self.start_fixed_listeners(startup_gate)?;
        self.restore_attach_listeners(previous.as_ref())?;

        check_startup_events(&mut events_rx)?;
        // The VFS-owned startup gate keeps the bound control and namespace
        // tasks from serving or exiting until this durable publication succeeds.
        self.runtime_record.publish()?;
        vfs.mark_ready();
        anyhow::ensure!(
            vfs.ready(),
            "required namespace attach listener exited before readiness"
        );
        info!("namespace listeners ready");
        self.spawn_signal_task();
        self.supervise(&mut events_rx, listener_events).await
    }

    fn start_fixed_listeners(
        self: &Arc<Self>,
        startup_gate: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let control_socket = self.context.control_socket();
        let control_listener = self.context.bind_control_socket()?;
        self.track_socket(control_socket);
        let rt = tokio::runtime::Handle::current();
        self.spawn_control_unix(control_listener, &rt, startup_gate)?;
        let vfs = self.vfs.get().context("VFS server was not initialized")?;
        vfs.serve_local(self.context.local_attach_socket())
            .context("bind local namespace listener")?;
        Ok(())
    }

    fn restore_attach_listeners(
        self: &Arc<Self>,
        previous: Option<&RuntimeRecord>,
    ) -> anyhow::Result<()> {
        if let Some(previous) = previous {
            for target in &previous.attach {
                match target {
                    AttachRecord::Tcp { addr, token } => {
                        let addr: SocketAddr = addr.parse().with_context(|| {
                            format!("invalid persisted attach TCP address `{addr}`")
                        })?;
                        let ip = match addr.ip() {
                            std::net::IpAddr::V4(ip) => ip,
                            std::net::IpAddr::V6(_) => {
                                anyhow::bail!("persisted attach TCP address must be IPv4: {addr}")
                            },
                        };
                        self.ensure_attach_tcp_with_token(
                            AttachBindAddr::requested(Some(ip))?,
                            addr.port(),
                            Some(token.clone()),
                        )?;
                    },
                    AttachRecord::Vsock { socket_path, token } => {
                        anyhow::ensure!(
                            socket_path == &self.context.vsock_attach_socket(),
                            "persisted vsock attach socket path {} is not the daemon-approved path",
                            socket_path.display()
                        );
                        self.ensure_attach_uds_with_token(Some(token.clone()))?;
                    },
                }
            }
        }
        if let Some(port) = self.context.attach_tcp_port()
            && self.vfs.get().is_some_and(|vfs| !vfs.ready())
        {
            self.ensure_attach_tcp_with_token(AttachBindAddr::loopback(), port, None)?;
        }
        Ok(())
    }

    async fn supervise(
        &self,
        events_rx: &mut tokio::sync::mpsc::UnboundedReceiver<TaskEvent>,
        mut listener_events: tokio::sync::broadcast::Receiver<omnifs_vfs_wire::ListenerEvent>,
    ) -> anyhow::Result<()> {
        let mut shutdown = self.shutdown_tx.subscribe();
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_ok() && *shutdown.borrow() {
                        return Ok(());
                    }
                }
                event = events_rx.recv() => match event {
                    Some(TaskEvent::Control) => anyhow::bail!("control API listener exited"),
                    None => anyhow::bail!("daemon task supervision channel closed"),
                },
                event = listener_events.recv() => match event {
                    Ok(omnifs_vfs_wire::ListenerEvent::Exited { target }) => {
                        if matches!(target, omnifs_vfs_wire::ListenerTarget::Local { .. }) {
                            anyhow::bail!("local namespace listener exited");
                        }
                        self.runtime_record.remove_attach(&attach_record(&target)?)?;
                        match &target {
                            ListenerTarget::Local { path } => {
                                warn!(transport = "local", path = %path.display(), "namespace listener exited; target is unavailable");
                            },
                            ListenerTarget::Tcp { addr, .. } => {
                                warn!(transport = "tcp", address = %addr, "namespace listener exited; target is unavailable");
                            },
                            ListenerTarget::Vsock { socket_path, .. } => {
                                warn!(transport = "vsock", path = %socket_path.display(), "namespace listener exited; target is unavailable");
                            },
                        }
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        anyhow::bail!("VFS listener supervision channel closed");
                    },
                },
            }
        }
    }

    fn spawn_signal_task(self: &Arc<Self>) {
        let daemon = Arc::clone(self);
        let task = tokio::spawn(async move {
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
                let _ = daemon.shutdown_tx.send(true);
            }
        });
        self.track_task(task);
    }

    /// Serve the control API over the Unix socket, where auth is filesystem
    /// permissions on the workspace-owned socket.
    pub fn spawn_control_unix(
        self: &Arc<Self>,
        listener: std::os::unix::net::UnixListener,
        rt: &tokio::runtime::Handle,
        mut startup_gate: tokio::sync::watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(listener)?;
        info!("control API listening (unix socket, filesystem-permission auth)");
        let app = Self::router(Arc::clone(self));
        let daemon = Arc::clone(self);
        let task = rt.spawn(async move {
            let cancelled = if *startup_gate.borrow() {
                false
            } else {
                startup_gate.changed().await.is_err() || !*startup_gate.borrow()
            };
            if cancelled {
                daemon.send_event(TaskEvent::Control);
                return;
            }
            if let Err(error) = axum::serve(listener, app).await {
                warn!(%error, "control API server exited");
            }
            daemon.send_event(TaskEvent::Control);
        });
        self.track_task(task);
        Ok(())
    }

    fn control_status(&self) -> DaemonStatus {
        let entries = self.registry.runtime_entries();
        let mut mounts = Vec::with_capacity(entries.len());
        for (mount, runtime) in entries {
            mounts.push(MountInfo {
                provider_name: runtime.provider_name().to_string(),
                provider_id: runtime.provider_id().to_string(),
                auth_health: runtime
                    .auth_health()
                    .map(|health| api_credential_health_kind(&health)),
                mount,
            });
        }
        mounts.sort_by(|a, b| a.mount.cmp(&b.mount));
        let Some(vfs) = self.vfs.get() else {
            return self.context.status(false, Vec::new(), mounts);
        };
        self.context.status(vfs.ready(), vfs.attachments(), mounts)
    }

    pub fn trigger_shutdown(self: &Arc<Self>) {
        let _ = self.shutdown_tx.send(true);
    }

    fn event_stream(&self) -> Response {
        let Some(inspector) = self.inspector.clone() else {
            return error_response(
                StatusCode::NOT_FOUND,
                ErrorCode::Internal,
                "inspector stream disabled (OMNIFS_INSPECTOR=0)",
            );
        };

        let subscription = inspector.subscribe();
        let stream = tokio_stream::iter(subscription.history)
            .map(Ok)
            .chain(BroadcastStream::new(subscription.live))
            .filter_map(|item| match item {
                Ok(record) => match record.to_json_line() {
                    Ok(line) => Some(line),
                    Err(error) => {
                        warn!(%error, "failed to serialize inspector record");
                        None
                    },
                },
                Err(BroadcastStreamRecvError::Lagged(n)) => Some(format!("# dropped {n} events\n")),
            });
        let body = Body::from_stream(stream.map(Ok::<_, Infallible>));

        Response::builder()
            .header(header::CONTENT_TYPE, "application/x-ndjson")
            .body(body)
            .expect("static response parts are valid")
    }

    fn api_router() -> OpenApiRouter<Arc<Self>> {
        OpenApiRouter::new()
            .routes(routes!(ready))
            .routes(routes!(status))
            .routes(routes!(providers_list))
            .routes(routes!(mounts_list))
            .routes(routes!(mount_inspect))
            .routes(routes!(shutdown))
            .routes(routes!(events))
            .routes(routes!(frontend_attach_target))
            .routes(routes!(frontend_attach_target_vsock))
    }

    fn router(state: Arc<Self>) -> Router {
        let (router, _) = Self::api_router().with_state(state).split_for_parts();
        router
            .fallback(route_not_found)
            .method_not_allowed_fallback(method_not_allowed)
    }
}

pub fn openapi() -> utoipa::openapi::OpenApi {
    let mut openapi = ApiDoc::openapi();
    let (_, paths) = Daemon::api_router().split_for_parts();
    openapi.merge(paths);
    openapi
}

pub fn openapi_json() -> String {
    openapi()
        .to_pretty_json()
        .expect("OpenAPI document serializes")
}

#[utoipa::path(
    get,
    path = "/v1/ready",
    operation_id = "ready",
    responses(
        (status = 200, description = "namespace listeners are serving", body = ReadyInfo),
        (status = 503, description = "namespace listeners are not serving yet", body = ApiError),
    ),
)]
async fn ready(State(daemon): State<Arc<Daemon>>) -> Response {
    let ready = daemon.control_status().ready();
    if ready {
        Json(ReadyInfo { ready }).into_response()
    } else {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal,
            "namespace listeners are not serving yet",
        )
    }
}

#[utoipa::path(
    get,
    path = "/v1/status",
    operation_id = "status",
    responses((status = 200, description = "daemon runtime facts", body = DaemonStatus)),
)]
async fn status(State(daemon): State<Arc<Daemon>>) -> Json<DaemonStatus> {
    Json(daemon.control_status())
}

#[utoipa::path(
    get,
    path = "/v1/providers",
    operation_id = "providers_list",
    responses(
        (status = 200, description = "installed provider catalog", body = [ProviderSummary]),
        (status = 500, description = "provider catalog unavailable", body = ApiError),
    ),
)]
async fn providers_list(State(daemon): State<Arc<Daemon>>) -> Response {
    let catalog = Catalog::open(daemon.context.providers_dir());
    match provider_summaries(&catalog) {
        Ok(providers) => Json(providers).into_response(),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            format!("provider catalog unavailable: {error}"),
        ),
    }
}

#[utoipa::path(
    get,
    path = "/v1/mounts",
    operation_id = "mounts_list",
    responses((status = 200, description = "loaded provider mounts", body = [MountInfo])),
)]
async fn mounts_list(State(daemon): State<Arc<Daemon>>) -> Json<Vec<MountInfo>> {
    Json(daemon.control_status().mounts)
}

#[utoipa::path(
    get,
    path = "/v1/mounts/{name}",
    operation_id = "mount_inspect",
    params(("name" = String, Path, description = "mount name")),
    responses(
        (status = 200, description = "the mount", body = MountInfo),
        (status = 404, description = "mount not found", body = ApiError),
    ),
)]
async fn mount_inspect(
    State(daemon): State<Arc<Daemon>>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    match daemon
        .control_status()
        .mounts
        .into_iter()
        .find(|mount| mount.mount == name)
    {
        Some(info) => Json(info).into_response(),
        None => error_response(
            StatusCode::NOT_FOUND,
            ErrorCode::MountNotFound,
            format!("mount `{name}` not found"),
        ),
    }
}

/// `POST /v1/shutdown`: release the daemon's serving latch and exit. Frontend
/// processes have independent lifetimes and are torn down by the CLI.
#[utoipa::path(
    post,
    path = "/v1/shutdown",
    operation_id = "shutdown",
    responses((status = 200, description = "daemon state at shutdown", body = StopReport)),
)]
async fn shutdown(State(daemon): State<Arc<Daemon>>) -> Json<StopReport> {
    let status = daemon.control_status();
    let report = StopReport {
        frontends: status.frontends,
        providers_dropped: status.mounts.len(),
    };
    daemon.trigger_shutdown();
    Json(report)
}

/// `POST /v1/frontend/attach-target`: bind the TCP namespace attach listener on a
/// running daemon, so a containerized frontend (the Docker Desktop path, which
/// cannot share a host Unix socket into its Linux VM) can be started later
/// without restarting the daemon. Docker Desktop uses loopback; native Linux
/// asks for the Docker bridge gateway so the container can cross network
/// namespaces without exposing the listener on every host interface.
/// Idempotent: a repeat call returns the already-bound address and token
/// unchanged, since a listener cannot be re-pointed once serving.
#[utoipa::path(
    post,
    path = "/v1/frontend/attach-target",
    operation_id = "frontend_attach_target",
    request_body = Option<FrontendAttachTargetRequest>,
    responses(
        (status = 200, description = "the TCP attach listener's address and per-instance token", body = FrontendAttachTargetReport),
        (status = 400, description = "the requested address is not an approved attach boundary", body = ApiError),
        (status = 503, description = "the namespace is not ready yet", body = ApiError),
        (status = 500, description = "failed to bind the attach listener", body = ApiError),
    ),
)]
async fn frontend_attach_target(
    State(daemon): State<Arc<Daemon>>,
    request: Option<Json<FrontendAttachTargetRequest>>,
) -> Response {
    let request = request.map(|Json(request)| request).unwrap_or_default();
    if request.driver != FrontendDelivery::Docker {
        return error_response(
            StatusCode::BAD_REQUEST,
            ErrorCode::SpecInvalid,
            format!(
                "unsupported attach driver `{}`; only `docker` is accepted on this route \
                 (krunkit attaches over vsock instead, via /v1/frontend/attach-target/vsock)",
                request.driver
            ),
        );
    }
    let bind_addr = match AttachBindAddr::requested(request.bind_ip) {
        Ok(bind_addr) => bind_addr,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::SpecInvalid,
                error.to_string(),
            );
        },
    };
    match daemon.ensure_attach_tcp(bind_addr, 0) {
        Ok(AttachOutcome::Bound(omnifs_vfs_wire::ListenerTarget::Tcp { addr, token })) => {
            Json(FrontendAttachTargetReport {
                addr: addr.to_string(),
                token,
            })
            .into_response()
        },
        Ok(AttachOutcome::Bound(_)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            "unexpected listener target",
        ),
        Ok(AttachOutcome::NamespaceNotReady) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal,
            "the namespace is not ready yet",
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            error.to_string(),
        ),
    }
}

/// `POST /v1/frontend/attach-target/vsock`: bind the token-checking UDS namespace
/// attach listener on a running daemon, for the krunkit vsock-proxy path (a
/// macOS guest VM with no shared host Unix socket and no Docker-style loopback
/// either; it dials host vsock instead, and krunkit proxies every connection
/// onto this socket). Takes no request body: unlike the TCP listener there is
/// no bind address to choose, only the daemon-picked path under the
/// workspace. Idempotent: a repeat call returns the already-bound path and
/// token unchanged, since a listener cannot be re-pointed once serving.
#[utoipa::path(
    post,
    path = "/v1/frontend/attach-target/vsock",
    operation_id = "frontend_attach_target_vsock",
    responses(
        (status = 200, description = "the UDS attach listener's socket path and per-instance token", body = FrontendAttachTargetVsockReport),
        (status = 503, description = "the namespace is not ready yet", body = ApiError),
        (status = 500, description = "failed to bind the attach listener", body = ApiError),
    ),
)]
async fn frontend_attach_target_vsock(State(daemon): State<Arc<Daemon>>) -> Response {
    match daemon.ensure_attach_uds() {
        Ok(AttachOutcome::Bound(omnifs_vfs_wire::ListenerTarget::Vsock { socket_path, token })) => {
            Json(FrontendAttachTargetVsockReport {
                socket_path: socket_path.display().to_string(),
                token,
            })
            .into_response()
        },
        Ok(AttachOutcome::Bound(_)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            "unexpected listener target",
        ),
        Ok(AttachOutcome::NamespaceNotReady) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal,
            "the namespace is not ready yet",
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            error.to_string(),
        ),
    }
}

/// Stream the inspector history snapshot followed by live records as
/// newline-framed JSON using the same wire format the raw TCP listener used to
/// speak, now chunk-encoded by HTTP. A lagged subscriber gets a
/// `# dropped N events` comment line and resumes from the newest record.
#[utoipa::path(
    get,
    path = "/v1/events",
    operation_id = "events",
    responses(
        (status = 200, description = "newline-framed inspector event stream", content_type = "application/x-ndjson", body = String),
        (status = 404, description = "inspector stream disabled", body = ApiError),
    ),
)]
async fn events(State(daemon): State<Arc<Daemon>>) -> Response {
    daemon.event_stream()
}

async fn route_not_found() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        ErrorCode::Internal,
        "control route not found",
    )
}

async fn method_not_allowed() -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        ErrorCode::Internal,
        "method not allowed",
    )
}

fn error_response(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiError {
            code,
            message: message.into(),
            detail: None,
        }),
    )
        .into_response()
}

fn api_credential_health_kind(health: &omnifs_auth::CredentialHealth) -> CredentialHealth {
    match health {
        omnifs_auth::CredentialHealth::Ready => CredentialHealth::Ready,
        omnifs_auth::CredentialHealth::ExpiringSoon => CredentialHealth::ExpiringSoon,
        omnifs_auth::CredentialHealth::Expired => CredentialHealth::Expired,
        omnifs_auth::CredentialHealth::RefreshFailed { .. } => CredentialHealth::RefreshFailed,
        omnifs_auth::CredentialHealth::NeedsConsent => CredentialHealth::NeedsConsent,
        omnifs_auth::CredentialHealth::Missing => CredentialHealth::Missing,
        omnifs_auth::CredentialHealth::StaticUnvalidated => CredentialHealth::StaticUnvalidated,
    }
}

fn provider_summaries(catalog: &Catalog) -> Result<Vec<ProviderSummary>, CatalogError> {
    let mut by_name = BTreeMap::new();
    for provider in catalog.installed()? {
        by_name
            .entry(provider.meta.name.clone())
            .or_insert_with(Vec::new)
            .push(api_provider_artifact(&provider));
    }
    for artifacts in by_name.values_mut() {
        artifacts.sort_by(|a, b| {
            a.version
                .cmp(&b.version)
                .then_with(|| a.id_hash.cmp(&b.id_hash))
        });
    }

    let mut names = catalog
        .installable()?
        .into_iter()
        .map(|provider| provider.meta.name)
        .collect::<BTreeSet<_>>();
    names.extend(by_name.keys().cloned());

    names
        .into_iter()
        .map(|name| {
            Ok(ProviderSummary {
                installed: by_name.remove(&name).unwrap_or_default(),
                name: name.to_string(),
            })
        })
        .collect()
}

fn api_provider_artifact(provider: &Provider) -> ProviderArtifact {
    ProviderArtifact {
        version: provider.meta.version.as_ref().map(ToString::to_string),
        id_hash: provider.id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{StatusCode, header};
    use tower::ServiceExt as _;

    #[test]
    fn checked_in_openapi_matches_implementation() {
        let checked_in: serde_json::Value =
            serde_json::from_str(include_str!("../../omnifs-api/openapi/daemon.json"))
                .expect("checked-in OpenAPI spec parses");
        let generated: serde_json::Value =
            serde_json::from_str(&super::openapi_json()).expect("generated OpenAPI spec parses");

        assert_eq!(checked_in, generated);
    }

    #[test]
    fn runtime_record_store_fences_late_updates_after_removal() {
        use omnifs_workspace::runtime_record::{
            AttachRecord, Endpoint, RecordedBackend, RuntimeRecord,
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        let store = super::RuntimeRecordStore::new(
            path.clone(),
            RuntimeRecord::new(
                omnifs_workspace::mounts::Revision::new("a".repeat(40)).unwrap(),
                Endpoint::Unix {
                    path: dir.path().join("control.sock"),
                },
                RecordedBackend::Native { pid: 1 },
                "instance".to_string(),
            ),
        );

        store
            .set_attach(AttachRecord::Tcp {
                addr: "127.0.0.1:1".to_string(),
                token: "a".repeat(32),
            })
            .unwrap();
        assert!(!path.exists());
        store.publish().unwrap();
        store
            .set_attach(AttachRecord::Vsock {
                socket_path: dir.path().join("vsock.sock"),
                token: "b".repeat(32),
            })
            .unwrap();
        assert_eq!(RuntimeRecord::read(&path).unwrap().unwrap().attach.len(), 2);

        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            store
                .set_attach(AttachRecord::Tcp {
                    addr: "127.0.0.1:3".to_string(),
                    token: "d".repeat(32),
                })
                .is_err()
        );
        assert!(
            store
                .remove_attach(&AttachRecord::Tcp {
                    addr: "127.0.0.1:1".to_string(),
                    token: "a".repeat(32),
                })
                .is_err()
        );

        std::fs::remove_dir(&path).unwrap();
        store.publish().unwrap();
        let recovered = RuntimeRecord::read(&path).unwrap().unwrap();
        assert!(
            recovered.attach.iter().any(
                |target| matches!(target, AttachRecord::Tcp { addr, .. } if addr == "127.0.0.1:1")
            ),
            "failed persistence must roll back the in-memory replacement"
        );
        assert_eq!(recovered.attach.len(), 2);
        store.remove();
        assert!(
            store
                .set_attach(AttachRecord::Tcp {
                    addr: "127.0.0.1:2".to_string(),
                    token: "c".repeat(32),
                })
                .is_err()
        );
        assert!(!path.exists());
    }

    #[test]
    fn pre_ready_control_exit_blocks_publication() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        sender.send(super::TaskEvent::Control).unwrap();
        let error = super::check_startup_events(&mut receiver).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("control API listener exited before readiness")
        );
    }

    #[test]
    fn attach_bind_accepts_only_loopback_without_a_verified_bridge() {
        assert_eq!(
            super::AttachBindAddr::requested(None).unwrap().0,
            std::net::Ipv4Addr::LOCALHOST
        );
        assert!(super::AttachBindAddr::requested(Some(std::net::Ipv4Addr::UNSPECIFIED)).is_err());
        assert!(
            super::AttachBindAddr::requested(Some(std::net::Ipv4Addr::new(192, 0, 2, 1))).is_err()
        );
    }

    /// Fetch and decode `/v1/status` from `router`.
    async fn fetch_status(router: &Router) -> omnifs_api::DaemonStatus {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    /// Poll `/v1/status` until `predicate` holds or the deadline passes. The
    /// registry update is asynchronous (the observer callback runs on the
    /// connection's own task), so a status assertion right after
    /// connect/disconnect needs to tolerate a short delay rather than racing
    /// it.
    async fn wait_for_status(
        router: &Router,
        mut predicate: impl FnMut(&omnifs_api::DaemonStatus) -> bool,
    ) -> omnifs_api::DaemonStatus {
        for _ in 0..200 {
            let status = fetch_status(router).await;
            if predicate(&status) {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("status did not converge to the expected shape within the deadline");
    }

    async fn assert_non_docker_attach_is_rejected(router: &Router) {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/frontend/attach-target")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"driver":"local"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    async fn shutdown_report(router: &Router) -> omnifs_api::StopReport {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/shutdown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        serde_json::from_slice(
            &to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    fn assert_attached_frontend(status: &omnifs_api::DaemonStatus) {
        let attached = status
            .frontends
            .iter()
            .find(|frontend| frontend.delivery == omnifs_api::FrontendDelivery::Docker)
            .unwrap();
        assert_eq!(attached.fs_type, omnifs_api::FsType::Fuse);
        assert_eq!(attached.source, "wire");
        assert_eq!(attached.mount_point, PathBuf::from("/guest/omnifs"));
        assert!(
            status
                .health
                .subsystem(omnifs_api::DaemonSubsystem::Frontend)
                .unwrap()
                .message
                .contains("attached fuse at /guest/omnifs via docker")
        );
    }

    /// A frontend attached through the TCP namespace listener appears in
    /// `/v1/status` with the listener-owned `docker` delivery label, then
    /// disappears when its connection closes.
    #[tokio::test]
    #[allow(unsafe_code)] // env::set_var requires unsafe; see SAFETY below.
    async fn attached_frontend_appears_and_disappears_in_status() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: cargo-nextest isolates each test into its own process (the
        // same pattern the omnifs-vfs-wire trace-propagation test documents
        // for OMNIFS_INSPECTOR), so mutating OMNIFS_HOME here cannot race a
        // parallel test.
        unsafe {
            std::env::set_var("OMNIFS_HOME", dir.path());
        }

        let args = crate::app::DaemonArgs {
            mount_revision: omnifs_workspace::mounts::Revision::new("a".repeat(40)).unwrap(),
            mount_snapshot: dir.path().join("mounts"),
            attach_tcp: None,
        };
        std::fs::create_dir_all(&args.mount_snapshot).unwrap();
        let context = crate::context::DaemonContext::resolve(&args).unwrap();
        context.prepare_startup_dirs().unwrap();

        let cloner =
            Arc::new(omnifs_engine::GitCloner::new(context.cache_dir().join("clones")).unwrap());
        let desired = omnifs_workspace::mounts::Registry::load(&args.mount_snapshot).unwrap();
        let registry = Arc::new(
            omnifs_engine::MountRuntimes::load(
                context.host_context(),
                cloner,
                &desired,
                &tokio::runtime::Handle::current(),
            )
            .unwrap(),
        );
        let runtime_record =
            super::RuntimeRecordStore::new(context.runtime_record_file(), context.runtime_record());
        let daemon = Arc::new(super::Daemon::new(
            context,
            Arc::clone(&registry),
            None,
            Arc::clone(&runtime_record),
        ));

        let rt = tokio::runtime::Handle::current();
        let namespace = omnifs_engine::TreeNamespace::new(Arc::clone(&registry), rt.clone());
        daemon.set_namespace(Arc::clone(&namespace));

        let router = super::Daemon::router(Arc::clone(&daemon));
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/frontend/attach-target")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let vfs = daemon.vfs.get().unwrap();
        vfs.serve_local(dir.path().join("local.sock")).unwrap();
        vfs.mark_ready();

        runtime_record.publish().unwrap();
        std::fs::remove_file(&runtime_record.path).unwrap();
        std::fs::create_dir(&runtime_record.path).unwrap();
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/frontend/attach-target")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            vfs.ready(),
            "rolling back a newly bound listener must preserve readiness"
        );
        std::fs::remove_dir(&runtime_record.path).unwrap();
        runtime_record.publish().unwrap();
        assert!(
            omnifs_workspace::runtime_record::RuntimeRecord::read(&runtime_record.path)
                .unwrap()
                .unwrap()
                .attach
                .is_empty(),
            "failed persistence must not retain the rolled-back listener"
        );

        let target = match daemon
            .ensure_attach_tcp(super::AttachBindAddr::loopback(), 0)
            .unwrap()
        {
            super::AttachOutcome::Bound(target) => target,
            super::AttachOutcome::NamespaceNotReady => {
                panic!("the namespace was set before binding the attach listener")
            },
        };

        assert_non_docker_attach_is_rejected(&router).await;

        let baseline = fetch_status(&router).await;
        assert!(
            baseline
                .frontends
                .iter()
                .all(|frontend| frontend.delivery != omnifs_api::FrontendDelivery::Docker),
            "no attached frontend before any client connects"
        );

        let identity = omnifs_vfs_wire::FrontendIdentity {
            kind: omnifs_vfs_wire::FrontendKind::Fuse,
            mount_point: PathBuf::from("/guest/omnifs"),
        };
        let attach_target = match &target {
            omnifs_vfs_wire::ListenerTarget::Tcp { addr, token } => {
                omnifs_vfs_wire::AttachTarget::Tcp {
                    addr: addr.to_string(),
                    token: token.clone(),
                }
            },
            _ => panic!("TCP attach returned a non-TCP target"),
        };
        let wire =
            omnifs_vfs_wire::WireNamespace::attach(attach_target.clone(), identity, rt.clone())
                .await
                .unwrap();
        let wire2 = omnifs_vfs_wire::WireNamespace::attach(
            attach_target,
            omnifs_vfs_wire::FrontendIdentity {
                kind: omnifs_vfs_wire::FrontendKind::Fuse,
                mount_point: PathBuf::from("/guest/omnifs"),
            },
            rt.clone(),
        )
        .await
        .unwrap();

        let attached_status = wait_for_status(&router, |status| {
            status
                .frontends
                .iter()
                .any(|frontend| frontend.delivery == omnifs_api::FrontendDelivery::Docker)
        })
        .await;
        assert_attached_frontend(&attached_status);

        let report = shutdown_report(&router).await;
        assert_eq!(report.frontends.len(), 1);
        assert_eq!(
            report.frontends[0].delivery,
            omnifs_api::FrontendDelivery::Docker
        );

        drop(wire);

        let retained = wait_for_status(&router, |status| {
            status
                .frontends
                .iter()
                .any(|frontend| frontend.delivery == omnifs_api::FrontendDelivery::Docker)
        })
        .await;
        assert_attached_frontend(&retained);

        drop(wire2);

        let after_disconnect = wait_for_status(&router, |status| {
            status
                .frontends
                .iter()
                .all(|frontend| frontend.delivery != omnifs_api::FrontendDelivery::Docker)
        })
        .await;
        assert!(
            after_disconnect
                .frontends
                .iter()
                .all(|frontend| frontend.delivery != omnifs_api::FrontendDelivery::Docker)
        );
    }
}
