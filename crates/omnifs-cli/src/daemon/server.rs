//! Typed local control protocol server.

use anyhow::Context as _;
use omnifs_api::events::InspectorLine;
use omnifs_api::{
    CONTROL_MAX_LINE_BYTES, CONTROL_PROTOCOL_VERSION, ControlError, ControlErrorCode,
    ControlOperation, ControlOutcome, ControlReply, ControlRequest, CredentialHealth, DaemonStatus,
    MountInfo,
};
use omnifs_engine::{Inspector, MountTable};
use omnifs_workspace::DaemonState;
use omnifs_workspace::daemon_record::DaemonRecord;
use omnifs_workspace::mounts::{Registry, Revision};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tracing::{info, warn};

use super::context::DaemonContext;
use crate::client::read_control_line;

const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Loopback, or Linux's Docker bridge gateway when that interface exists.
/// Never binds all interfaces.
#[cfg(target_os = "linux")]
fn docker_bind_ip() -> anyhow::Result<Ipv4Addr> {
    for interface in nix::ifaddrs::getifaddrs().context("enumerate host network interfaces")? {
        if interface.interface_name != "docker0" {
            continue;
        }
        if let Some(addr) = interface
            .address
            .as_ref()
            .and_then(nix::sys::socket::SockaddrStorage::as_sockaddr_in)
        {
            return Ok(Ipv4Addr::from(addr.ip()));
        }
    }
    Ok(Ipv4Addr::LOCALHOST)
}

#[cfg(not(target_os = "linux"))]
fn docker_bind_ip() -> Ipv4Addr {
    Ipv4Addr::LOCALHOST
}

pub(crate) struct DaemonRecordStore {
    daemon: DaemonState,
    record: Mutex<Option<DaemonRecord>>,
    published: AtomicBool,
}

impl DaemonRecordStore {
    pub(crate) fn new(daemon: DaemonState, record: DaemonRecord) -> Arc<Self> {
        Arc::new(Self {
            daemon,
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
            anyhow::bail!("daemon record has already been removed");
        };
        self.daemon.write_record(record)?;
        self.published.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn remove(&self) {
        let mut guard = self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let published = self.published.swap(false, Ordering::AcqRel);
        guard.take();
        if published && let Err(error) = self.daemon.remove_record() {
            warn!(
                %error,
                path = %self.daemon.record_file().display(),
                "failed to remove daemon record"
            );
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
    if let Ok(TaskEvent::Control) = events_rx.try_recv() {
        anyhow::bail!("control listener exited before readiness");
    }
    Ok(())
}

pub(crate) struct Daemon {
    context: DaemonContext,
    registry: Arc<MountTable>,
    inspector: Option<Arc<Inspector>>,
    record_store: Arc<DaemonRecordStore>,
    vfs: OnceLock<Arc<omnifs_vfs::VfsServer>>,
    bound_tcp: OnceLock<omnifs_vfs::Endpoint>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    events_tx: OnceLock<tokio::sync::mpsc::UnboundedSender<TaskEvent>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    socket_paths: Mutex<Vec<PathBuf>>,
}

impl Daemon {
    pub(crate) fn new(
        context: DaemonContext,
        registry: Arc<MountTable>,
        inspector: Option<Arc<Inspector>>,
        record_store: Arc<DaemonRecordStore>,
    ) -> Self {
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        Self {
            context,
            registry,
            inspector,
            record_store,
            vfs: OnceLock::new(),
            bound_tcp: OnceLock::new(),
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
    pub(crate) fn set_namespace(&self, namespace: Arc<omnifs_engine::TreeNamespace>) {
        let server = omnifs_vfs::VfsServer::new(namespace);
        let _ = self.vfs.set(server);
    }

    /// Own the daemon's complete serving lifetime. Startup binds both fixed
    /// namespace listeners and publishes the new record only after all
    /// required listeners are alive. The same method owns task joins,
    /// provider shutdown, record removal, and socket cleanup.
    pub(crate) async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let result = self.run_inner().await;
        let _ = self.shutdown_tx.send(true);
        self.stop_tasks().await;
        if let Some(vfs) = self.vfs.get() {
            vfs.shutdown().await;
        }
        self.registry.shutdown_all();
        self.record_store.remove();
        self.cleanup_sockets();
        result
    }

    async fn run_inner(self: &Arc<Self>) -> anyhow::Result<()> {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = self.events_tx.set(events_tx);
        let vfs = self.vfs.get().context("VFS server was not initialized")?;
        let listener_events = vfs.listener_events();
        let startup_gate = vfs.begin_startup();
        self.start_listeners(startup_gate)?;

        check_startup_events(&mut events_rx)?;
        // The VFS-owned startup gate keeps the bound control and namespace
        // tasks from serving or exiting until this durable publication succeeds.
        self.record_store.publish()?;
        vfs.mark_ready();
        anyhow::ensure!(
            vfs.ready(),
            "required namespace attach listener exited before readiness"
        );
        info!("namespace listeners ready");
        self.spawn_signal_task();
        self.supervise(&mut events_rx, listener_events).await
    }

    fn start_listeners(
        self: &Arc<Self>,
        startup_gate: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let control_socket = self.context.control_socket();
        let control_listener = self.context.bind_control_socket()?;
        self.track_socket(control_socket);
        let rt = tokio::runtime::Handle::current();
        self.spawn_control_unix(control_listener, &rt, startup_gate)?;
        let vfs = self.vfs.get().context("VFS server was not initialized")?;
        vfs.serve_unix(&self.context.attach_socket())
            .context("bind namespace Unix endpoint")?;
        let port = self.context.attach_port();
        #[cfg(target_os = "linux")]
        let bind_ip = docker_bind_ip()?;
        #[cfg(not(target_os = "linux"))]
        let bind_ip = docker_bind_ip();
        let endpoint = vfs.serve_tcp(bind_ip, port).with_context(|| {
            format!(
                "bind namespace TCP endpoint on port {port}; another process holds it. \
                     Set [filesystem] attach_port in workspace config to move it."
            )
        })?;
        let _ = self.bound_tcp.set(endpoint);
        Ok(())
    }

    async fn supervise(
        &self,
        events_rx: &mut tokio::sync::mpsc::UnboundedReceiver<TaskEvent>,
        mut listener_events: tokio::sync::broadcast::Receiver<omnifs_vfs::ListenerEvent>,
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
                    Some(TaskEvent::Control) => anyhow::bail!("control listener exited"),
                    None => anyhow::bail!("daemon task supervision channel closed"),
                },
                event = listener_events.recv() => match event {
                    Ok(omnifs_vfs::ListenerEvent::Exited { endpoint }) => {
                        anyhow::bail!("required namespace endpoint exited: {endpoint:?}");
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

    /// Serve the typed control protocol over the workspace-owned Unix socket.
    fn spawn_control_unix(
        self: &Arc<Self>,
        listener: std::os::unix::net::UnixListener,
        rt: &tokio::runtime::Handle,
        mut startup_gate: tokio::sync::watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(listener)?;
        info!("control socket listening (filesystem-permission auth)");
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
            let mut shutdown = daemon.shutdown_tx.subscribe();
            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_ok() && *shutdown.borrow() {
                            return;
                        }
                    }
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let connection_daemon = Arc::clone(&daemon);
                            daemon.track_task(tokio::spawn(async move {
                                if let Err(error) = handle_control_connection(connection_daemon, stream).await {
                                    warn!(%error, "control connection closed");
                                }
                            }));
                        }
                        Err(error) => {
                            warn!(%error, "control listener exited");
                            daemon.send_event(TaskEvent::Control);
                            return;
                        }
                    }
                }
            }
        });
        self.track_task(task);
        Ok(())
    }

    fn control_status(&self) -> DaemonStatus {
        let mut mounts = Vec::with_capacity(self.registry.mounts().len());
        for (name, loaded, runtime) in self.registry.selected_entries() {
            let spec = loaded.spec();
            mounts.push(MountInfo {
                provider_name: spec.provider.meta.name.to_string(),
                provider_id: spec.provider.id.to_string(),
                auth_health: runtime.and_then(|runtime| {
                    runtime
                        .auth_health()
                        .map(|health| api_credential_health_kind(&health))
                }),
                mount: name.to_string(),
            });
        }
        mounts.sort_by(|a, b| a.mount.cmp(&b.mount));
        let attach_tcp = self.bound_tcp.get().and_then(|endpoint| match endpoint {
            omnifs_vfs::Endpoint::Tcp { addr } => Some(*addr),
            omnifs_vfs::Endpoint::Unix { .. } => None,
        });
        let Some(vfs) = self.vfs.get() else {
            return self.context.status(false, attach_tcp, Vec::new(), mounts);
        };
        self.context
            .status(vfs.ready(), attach_tcp, vfs.attachments(), mounts)
    }

    fn trigger_shutdown(self: &Arc<Self>) {
        let _ = self.shutdown_tx.send(true);
    }

    async fn validate_offline(&self, revision: String) -> anyhow::Result<()> {
        let revision = Revision::new(revision).context("invalid offline validation revision")?;
        let snapshot = self.context.mount_snapshot(&revision);
        let table = Arc::clone(&self.registry);
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let desired = Registry::load(&snapshot)
                .with_context(|| format!("load mount snapshot {}", snapshot.display()))?;
            table
                .validate_offline(&desired)
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        })
        .await
        .context("offline projection validation task failed")??;
        Ok(())
    }
}

#[allow(clippy::too_many_lines)] // one exhaustive typed control-protocol dispatch boundary
async fn handle_control_connection(
    daemon: Arc<Daemon>,
    mut stream: UnixStream,
) -> anyhow::Result<()> {
    let line = match read_control_line(&mut stream).await {
        Ok(line) => line,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            let code = if error.to_string().contains("maximum size") {
                ControlErrorCode::LineTooLarge
            } else {
                ControlErrorCode::MalformedJson
            };
            write_control_reply(
                &mut stream,
                ControlReply::error(ControlError::new(code, error.to_string())),
            )
            .await?;
            return Ok(());
        },
        Err(error) => return Err(error.into()),
    };

    let value: serde_json::Value = match serde_json::from_slice(&line) {
        Ok(value) => value,
        Err(error) => {
            write_control_reply(
                &mut stream,
                ControlReply::error(ControlError::new(
                    ControlErrorCode::MalformedJson,
                    format!("malformed control request: {error}"),
                )),
            )
            .await?;
            return Ok(());
        },
    };
    let operation_name = value.get("operation").and_then(serde_json::Value::as_str);
    let known_operation = matches!(
        operation_name,
        Some("ready" | "status" | "shutdown" | "validate_offline" | "subscribe_inspector",)
    );
    if !known_operation {
        write_control_reply(
            &mut stream,
            ControlReply::error(ControlError::new(
                ControlErrorCode::UnknownOperation,
                "unknown control operation",
            )),
        )
        .await?;
        return Ok(());
    }

    let request: ControlRequest = match serde_json::from_value(value) {
        Ok(request) => request,
        Err(error) => {
            write_control_reply(
                &mut stream,
                ControlReply::error(ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    format!("invalid control request: {error}"),
                )),
            )
            .await?;
            return Ok(());
        },
    };
    if request.version != CONTROL_PROTOCOL_VERSION {
        write_control_reply(
            &mut stream,
            ControlReply::error(ControlError::new(
                ControlErrorCode::UnsupportedVersion,
                format!("unsupported control protocol version {}", request.version),
            )),
        )
        .await?;
        return Ok(());
    }

    match request.operation {
        ControlOperation::Ready => {
            let reply = if daemon.vfs.get().is_some_and(|vfs| vfs.ready()) {
                ControlReply::ready()
            } else {
                ControlReply::error(ControlError::new(
                    ControlErrorCode::NotReady,
                    "namespace listeners are not serving yet",
                ))
            };
            write_control_reply(&mut stream, reply).await?;
        },
        ControlOperation::Status => {
            write_control_reply(
                &mut stream,
                ControlReply {
                    version: CONTROL_PROTOCOL_VERSION,
                    outcome: ControlOutcome::Status(daemon.control_status()),
                },
            )
            .await?;
        },
        ControlOperation::Shutdown { stop_filesystems } => {
            let (detached, still_attached) = if stop_filesystems {
                if let Some(vfs) = daemon.vfs.get() {
                    let before = vfs.attachments().len();
                    vfs.stop_filesystems();
                    let still = vfs.drain_attachments(DRAIN_TIMEOUT).await;
                    (
                        before.saturating_sub(still.len()),
                        still
                            .into_iter()
                            .map(|identity| identity.to_string())
                            .collect(),
                    )
                } else {
                    (0, Vec::new())
                }
            } else {
                (0, Vec::new())
            };
            let result = write_control_reply(
                &mut stream,
                ControlReply {
                    version: CONTROL_PROTOCOL_VERSION,
                    outcome: ControlOutcome::Shutdown {
                        detached,
                        still_attached,
                    },
                },
            )
            .await;
            daemon.trigger_shutdown();
            result?;
        },
        ControlOperation::ValidateOffline { revision } => {
            let reply = match daemon.validate_offline(revision).await {
                Ok(()) => ControlReply {
                    version: CONTROL_PROTOCOL_VERSION,
                    outcome: ControlOutcome::OfflineValidated,
                },
                Err(error) => ControlReply::error(ControlError::new(
                    ControlErrorCode::OfflineValidationFailed,
                    error.to_string(),
                )),
            };
            write_control_reply(&mut stream, reply).await?;
        },
        ControlOperation::SubscribeInspector => {
            let Some(inspector) = daemon.inspector.clone() else {
                write_control_reply(
                    &mut stream,
                    ControlReply::error(ControlError::new(
                        ControlErrorCode::Internal,
                        "inspector stream disabled",
                    )),
                )
                .await?;
                return Ok(());
            };
            write_control_reply(
                &mut stream,
                ControlReply::inspector_ready(daemon.context.instance_id()),
            )
            .await?;
            let subscription = inspector.subscribe();
            for record in subscription.history {
                write_inspector_line(&mut stream, InspectorLine::Record((*record).clone())).await?;
            }
            let mut live_events = subscription.live;
            loop {
                match live_events.recv().await {
                    Ok(record) => {
                        write_inspector_line(&mut stream, InspectorLine::Record((*record).clone()))
                            .await?;
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        write_inspector_line(&mut stream, InspectorLine::Dropped { count }).await?;
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        },
    }
    Ok(())
}

async fn write_inspector_line(stream: &mut UnixStream, line: InspectorLine) -> anyhow::Result<()> {
    let line = serde_json::to_vec(&line).context("serialize inspector line")?;
    write_json_line(stream, line).await
}

async fn write_control_reply(stream: &mut UnixStream, reply: ControlReply) -> anyhow::Result<()> {
    let line = serde_json::to_vec(&reply).context("serialize control reply")?;
    write_json_line(stream, line).await
}

async fn write_json_line(stream: &mut UnixStream, mut line: Vec<u8>) -> anyhow::Result<()> {
    line.push(b'\n');
    anyhow::ensure!(
        line.len() <= CONTROL_MAX_LINE_BYTES,
        "control line exceeds the maximum size"
    );
    stream
        .write_all(&line)
        .await
        .context("write control line")?;
    stream.flush().await.context("flush control line")?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use omnifs_api::{
        CONTROL_MAX_LINE_BYTES, CONTROL_PROTOCOL_VERSION, ControlErrorCode, ControlOperation,
        ControlOutcome, ControlReply, ControlRequest,
    };
    use tokio::io::AsyncWriteExt as _;

    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn request(path: &std::path::Path, operation: ControlOperation) -> ControlReply {
        let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
        let request = ControlRequest {
            version: CONTROL_PROTOCOL_VERSION,
            operation,
        };
        let mut line = serde_json::to_vec(&request).unwrap();
        line.push(b'\n');
        stream.write_all(&line).await.unwrap();
        let line = super::read_control_line(&mut stream).await.unwrap();
        serde_json::from_slice(&line).unwrap()
    }

    async fn raw_request(path: &std::path::Path, line: Vec<u8>) -> ControlReply {
        let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
        stream.write_all(&line).await.unwrap();
        let line = super::read_control_line(&mut stream).await.unwrap();
        serde_json::from_slice(&line).unwrap()
    }

    fn test_daemon(dir: &tempfile::TempDir) -> Arc<super::Daemon> {
        let args = crate::daemon::app::DaemonArgs {
            mount_revision: omnifs_workspace::mounts::Revision::new("a".repeat(40)).unwrap(),
            mount_snapshot: dir.path().join("mounts"),
            offline: false,
        };
        std::fs::create_dir_all(&args.mount_snapshot).unwrap();
        let context = crate::daemon::context::DaemonContext::resolve(&args).unwrap();
        context.prepare_startup_dirs(false).unwrap();
        let desired = omnifs_workspace::mounts::Registry::load(&args.mount_snapshot).unwrap();
        let registry = Arc::new(
            omnifs_engine::MountTable::load(
                context.host(),
                &desired,
                &tokio::runtime::Handle::current(),
            )
            .unwrap(),
        );
        let daemon_record =
            super::DaemonRecordStore::new(context.daemon_state().clone(), context.daemon_record());
        Arc::new(super::Daemon::new(context, registry, None, daemon_record))
    }

    #[test]
    fn pre_ready_control_exit_blocks_publication() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        sender.send(super::TaskEvent::Control).unwrap();
        let error = super::check_startup_events(&mut receiver).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("control listener exited before readiness")
        );
    }

    #[test]
    fn attach_bind_is_never_unspecified() {
        #[cfg(target_os = "linux")]
        let bind_ip = super::docker_bind_ip().unwrap();
        #[cfg(not(target_os = "linux"))]
        let bind_ip = super::docker_bind_ip();
        assert!(!bind_ip.is_unspecified());
    }

    #[tokio::test]
    #[allow(unsafe_code)]
    async fn control_socket_reports_unconditional_tcp_endpoint_and_handshake_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let _env_guard = ENV_LOCK.lock().await;
        let home = std::fs::canonicalize(dir.path()).unwrap();
        unsafe {
            std::env::set_var("OMNIFS_HOME", &home);
        }

        let daemon = test_daemon(&dir);
        let rt = tokio::runtime::Handle::current();
        let namespace =
            omnifs_engine::TreeNamespace::online(Arc::clone(&daemon.registry), rt.clone());
        daemon.set_namespace(namespace);

        let vfs = daemon.vfs.get().unwrap();
        let startup_gate = vfs.begin_startup();
        daemon.start_listeners(startup_gate).unwrap();
        vfs.mark_ready();

        let control_socket = home.join("control.sock");

        assert!(matches!(
            request(&control_socket, ControlOperation::Ready)
                .await
                .outcome,
            ControlOutcome::Ready
        ));
        assert!(matches!(
            request(&control_socket, ControlOperation::Status)
                .await
                .outcome,
            ControlOutcome::Status(_)
        ));

        let status = match request(&control_socket, ControlOperation::Status)
            .await
            .outcome
        {
            ControlOutcome::Status(status) => status,
            outcome => panic!("unexpected status reply: {outcome:?}"),
        };
        let target = status.attach_tcp.expect("TCP endpoint");
        let attach_target = omnifs_vfs::AttachTarget::Tcp {
            addr: target.to_string(),
        };
        let identity = omnifs_core::fs::Spec::new(
            "control-test".parse().unwrap(),
            omnifs_core::fs::Protocol::Fuse,
            omnifs_core::fs::Runtime::Docker,
            std::path::PathBuf::from("/omnifs"),
        )
        .unwrap();
        let wire =
            omnifs_vfs::WireNamespace::attach(attach_target.clone(), identity.clone(), rt.clone())
                .await
                .unwrap();
        let wire2 = omnifs_vfs::WireNamespace::attach(attach_target, identity, rt.clone())
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            let reply = request(&control_socket, ControlOperation::Status).await;
            if let ControlOutcome::Status(status) = reply.outcome
                && status
                    .filesystems
                    .iter()
                    .any(|filesystem| filesystem.runtime() == omnifs_core::fs::Runtime::Docker)
            {
                break status;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert!(
            status
                .health
                .filesystems
                .message
                .contains("attached `control-test` (fuse) at /omnifs via docker")
        );

        drop(wire);
        drop(wire2);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            let reply = request(&control_socket, ControlOperation::Status).await;
            if let ControlOutcome::Status(status) = reply.outcome
                && status
                    .filesystems
                    .iter()
                    .all(|filesystem| filesystem.runtime() != omnifs_core::fs::Runtime::Docker)
            {
                break status;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert!(status.filesystems.is_empty());

        daemon.vfs.get().unwrap().shutdown().await;
        daemon.stop_tasks().await;
    }

    #[tokio::test]
    #[allow(unsafe_code)]
    async fn control_socket_rejects_malformed_and_oversized_lines() {
        let dir = tempfile::tempdir().unwrap();
        let _env_guard = ENV_LOCK.lock().await;
        let home = std::fs::canonicalize(dir.path()).unwrap();
        unsafe {
            std::env::set_var("OMNIFS_HOME", &home);
        }
        let daemon = test_daemon(&dir);
        let control_socket = dir.path().join("control.sock");
        let listener = std::os::unix::net::UnixListener::bind(&control_socket).unwrap();
        let (_gate_tx, gate_rx) = tokio::sync::watch::channel(true);
        daemon
            .spawn_control_unix(listener, &tokio::runtime::Handle::current(), gate_rx)
            .unwrap();

        let malformed = raw_request(&control_socket, b"{not-json}\n".to_vec()).await;
        assert!(matches!(
            malformed.outcome,
            ControlOutcome::Error(error) if error.code == ControlErrorCode::MalformedJson
        ));

        let unknown = raw_request(
            &control_socket,
            format!("{{\"version\":{CONTROL_PROTOCOL_VERSION},\"operation\":\"unknown\"}}\n")
                .into_bytes(),
        )
        .await;
        assert!(matches!(
            unknown.outcome,
            ControlOutcome::Error(error) if error.code == ControlErrorCode::UnknownOperation
        ));

        let invalid_offline_revision = raw_request(
            &control_socket,
            format!(
                "{{\"version\":{CONTROL_PROTOCOL_VERSION},\
                 \"operation\":\"validate_offline\",\"revision\":\"invalid\"}}\n"
            )
            .into_bytes(),
        )
        .await;
        assert!(matches!(
            invalid_offline_revision.outcome,
            ControlOutcome::Error(error)
                if error.code == ControlErrorCode::OfflineValidationFailed
        ));

        let oversized = raw_request(&control_socket, vec![b'x'; CONTROL_MAX_LINE_BYTES + 1]).await;
        assert!(matches!(
            oversized.outcome,
            ControlOutcome::Error(error) if error.code == ControlErrorCode::LineTooLarge
        ));
    }
}
