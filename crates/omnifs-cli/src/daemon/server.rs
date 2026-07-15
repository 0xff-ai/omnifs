//! Typed local control protocol server.

use anyhow::Context as _;
use omnifs_api::events::InspectorLine;
use omnifs_api::{
    CONTROL_MAX_LINE_BYTES, CONTROL_PROTOCOL_VERSION, ControlError, ControlErrorCode,
    ControlOperation, ControlOutcome, ControlReply, ControlRequest, CredentialHealth, DaemonStatus,
    MountInfo,
};
use omnifs_engine::{Inspector, MountRuntimes};
use omnifs_workspace::daemon_record::{AttachRecord, DaemonRecord};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::{info, warn};

use super::context::DaemonContext;
use omnifs_vfs_wire::ListenerTarget;

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

pub(crate) struct DaemonRecordStore {
    path: PathBuf,
    record: Mutex<Option<DaemonRecord>>,
    published: AtomicBool,
}

impl DaemonRecordStore {
    pub(crate) fn new(path: PathBuf, record: DaemonRecord) -> Arc<Self> {
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
            anyhow::bail!("daemon record has already been removed");
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
            anyhow::bail!("daemon record has already been removed")
        };
        let previous = record.clone();
        record.set_attach(target);
        if self.published.load(Ordering::Acquire)
            && let Err(error) = record.write(&self.path)
        {
            *record = previous;
            return Err(error).with_context(|| {
                format!(
                    "persist attach listener in daemon record {}",
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
            anyhow::bail!("daemon record has already been removed")
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
                    "persist removed attach listener in daemon record {}",
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
        if published && let Err(error) = DaemonRecord::remove(&self.path) {
            warn!(%error, path = %self.path.display(), "failed to remove daemon record");
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
            TaskEvent::Control => anyhow::bail!("control listener exited before readiness"),
        }
    }
    Ok(())
}

pub(crate) struct Daemon {
    context: DaemonContext,
    registry: Arc<MountRuntimes>,
    inspector: Option<Arc<Inspector>>,
    daemon_record: Arc<DaemonRecordStore>,
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
        daemon_record: Arc<DaemonRecordStore>,
    ) -> Self {
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        Self {
            context,
            registry,
            inspector,
            daemon_record,
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
    pub(crate) fn set_namespace(&self, namespace: Arc<omnifs_engine::TreeNamespace>) {
        let server = omnifs_vfs_wire::VfsServer::new(namespace);
        let _ = self.vfs.set(server);
    }

    fn ensure_attach_tcp(
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
        if let Err(error) = self.daemon_record.set_attach(record) {
            if newly_bound {
                vfs.remove_listener(&target);
            }
            return Err(error);
        }
        Ok(AttachOutcome::Bound(target))
    }

    fn ensure_attach_uds(&self) -> anyhow::Result<AttachOutcome> {
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
        if let Err(error) = self.daemon_record.set_attach(record) {
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
    pub(crate) async fn run(self: Arc<Self>, previous: Option<DaemonRecord>) -> anyhow::Result<()> {
        let result = self.run_inner(previous).await;
        let _ = self.shutdown_tx.send(true);
        self.stop_tasks().await;
        if let Some(vfs) = self.vfs.get() {
            vfs.shutdown().await;
        }
        self.registry.shutdown_all();
        self.daemon_record.remove();
        self.cleanup_sockets();
        result
    }

    async fn run_inner(self: &Arc<Self>, previous: Option<DaemonRecord>) -> anyhow::Result<()> {
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
        self.daemon_record.publish()?;
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
        previous: Option<&DaemonRecord>,
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
                    Some(TaskEvent::Control) => anyhow::bail!("control listener exited"),
                    None => anyhow::bail!("daemon task supervision channel closed"),
                },
                event = listener_events.recv() => match event {
                    Ok(omnifs_vfs_wire::ListenerEvent::Exited { target }) => {
                        if matches!(target, omnifs_vfs_wire::ListenerTarget::Local { .. }) {
                            anyhow::bail!("local namespace listener exited");
                        }
                        self.daemon_record.remove_attach(&attach_record(&target)?)?;
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

    fn trigger_shutdown(self: &Arc<Self>) {
        let _ = self.shutdown_tx.send(true);
    }
}

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
        Some(
            "ready" | "status" | "shutdown" | "attach_tcp" | "attach_vsock" | "subscribe_inspector"
        )
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
        ControlOperation::Shutdown => {
            daemon.trigger_shutdown();
            write_control_reply(
                &mut stream,
                ControlReply {
                    version: CONTROL_PROTOCOL_VERSION,
                    outcome: ControlOutcome::Shutdown,
                },
            )
            .await?;
        },
        ControlOperation::AttachTcp { bind_ip } => {
            let reply = match AttachBindAddr::requested(bind_ip)
                .and_then(|bind_addr| daemon.ensure_attach_tcp(bind_addr, 0))
            {
                Ok(AttachOutcome::Bound(ListenerTarget::Tcp { addr, token })) => ControlReply {
                    version: CONTROL_PROTOCOL_VERSION,
                    outcome: ControlOutcome::AttachTcp(omnifs_api::TcpAttachTarget {
                        addr: addr.to_string(),
                        token,
                    }),
                },
                Ok(AttachOutcome::Bound(_)) => ControlReply::error(ControlError::new(
                    ControlErrorCode::Internal,
                    "unexpected TCP attach target",
                )),
                Ok(AttachOutcome::NamespaceNotReady) => ControlReply::error(ControlError::new(
                    ControlErrorCode::NotReady,
                    "namespace listeners are not serving yet",
                )),
                Err(error) => ControlReply::error(ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    error.to_string(),
                )),
            };
            write_control_reply(&mut stream, reply).await?;
        },
        ControlOperation::AttachVsock => {
            let reply = match daemon.ensure_attach_uds() {
                Ok(AttachOutcome::Bound(ListenerTarget::Vsock { socket_path, token })) => {
                    ControlReply {
                        version: CONTROL_PROTOCOL_VERSION,
                        outcome: ControlOutcome::AttachVsock(omnifs_api::VsockAttachTarget {
                            socket_path,
                            token,
                        }),
                    }
                },
                Ok(AttachOutcome::Bound(_)) => ControlReply::error(ControlError::new(
                    ControlErrorCode::Internal,
                    "unexpected vsock attach target",
                )),
                Ok(AttachOutcome::NamespaceNotReady) => ControlReply::error(ControlError::new(
                    ControlErrorCode::NotReady,
                    "namespace listeners are not serving yet",
                )),
                Err(error) => ControlReply::error(ControlError::new(
                    ControlErrorCode::Internal,
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
            write_control_reply(&mut stream, ControlReply::inspector_ready()).await?;
            let subscription = inspector.subscribe();
            for record in subscription.history {
                write_inspector_line(&mut stream, InspectorLine::Record((*record).clone())).await?;
            }
            let mut live = subscription.live;
            loop {
                match live.recv().await {
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

async fn read_control_line<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut line = Vec::with_capacity(256);
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            if line.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "control connection closed before a line was received",
                ));
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "control line is missing its newline terminator",
            ));
        }
        if let Some(end) = chunk[..read].iter().position(|byte| *byte == b'\n') {
            let line_bytes = end + 1;
            if line.len() + line_bytes > CONTROL_MAX_LINE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "control line exceeds the maximum size",
                ));
            }
            line.extend_from_slice(&chunk[..line_bytes]);
            return Ok(line);
        }
        if line.len() + read > CONTROL_MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "control line exceeds the maximum size",
            ));
        }
        line.extend_from_slice(&chunk[..read]);
    }
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
    use std::sync::{Arc, Mutex};

    use omnifs_api::{
        CONTROL_MAX_LINE_BYTES, CONTROL_PROTOCOL_VERSION, ControlErrorCode, ControlOperation,
        ControlOutcome, ControlReply, ControlRequest,
    };
    use tokio::io::AsyncWriteExt as _;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
            attach_tcp: None,
        };
        std::fs::create_dir_all(&args.mount_snapshot).unwrap();
        let context = crate::daemon::context::DaemonContext::resolve(&args).unwrap();
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
        let daemon_record =
            super::DaemonRecordStore::new(context.daemon_record_file(), context.daemon_record());
        Arc::new(super::Daemon::new(context, registry, None, daemon_record))
    }

    #[test]
    fn daemon_record_store_fences_late_updates_after_removal() {
        use omnifs_workspace::daemon_record::{AttachRecord, DaemonRecord, Endpoint};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        let store = super::DaemonRecordStore::new(
            path.clone(),
            DaemonRecord::new(
                omnifs_workspace::mounts::Revision::new("a".repeat(40)).unwrap(),
                Endpoint::Unix {
                    path: dir.path().join("control.sock"),
                },
                1,
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
        assert_eq!(DaemonRecord::read(&path).unwrap().unwrap().attach.len(), 2);

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
        let recovered = DaemonRecord::read(&path).unwrap().unwrap();
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
                .contains("control listener exited before readiness")
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

    #[tokio::test]
    #[allow(unsafe_code)]
    async fn control_socket_dispatches_ready_status_attach_and_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let _env_guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let home = std::fs::canonicalize(dir.path()).unwrap();
        unsafe {
            std::env::set_var("OMNIFS_HOME", &home);
        }

        let daemon = test_daemon(&dir);
        let rt = tokio::runtime::Handle::current();
        let namespace = omnifs_engine::TreeNamespace::new(Arc::clone(&daemon.registry), rt.clone());
        daemon.set_namespace(namespace);

        let vfs = daemon.vfs.get().unwrap();
        let local_socket = dir.path().join("local.sock");
        vfs.serve_local(local_socket).unwrap();
        vfs.mark_ready();

        let control_socket = dir.path().join("control.sock");
        let listener = std::os::unix::net::UnixListener::bind(&control_socket).unwrap();
        let (_gate_tx, gate_rx) = tokio::sync::watch::channel(true);
        daemon.spawn_control_unix(listener, &rt, gate_rx).unwrap();

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

        let target = match request(
            &control_socket,
            ControlOperation::AttachTcp { bind_ip: None },
        )
        .await
        .outcome
        {
            ControlOutcome::AttachTcp(target) => target,
            outcome => panic!("unexpected attach reply: {outcome:?}"),
        };
        let attach_target = omnifs_vfs_wire::AttachTarget::Tcp {
            addr: target.addr,
            token: target.token,
        };
        let identity = omnifs_vfs_wire::FrontendIdentity {
            kind: omnifs_vfs_wire::FrontendKind::Fuse,
            mount_point: std::path::PathBuf::from("/guest/omnifs"),
        };
        let wire =
            omnifs_vfs_wire::WireNamespace::attach(attach_target.clone(), identity, rt.clone())
                .await
                .unwrap();
        let wire2 = omnifs_vfs_wire::WireNamespace::attach(
            attach_target,
            omnifs_vfs_wire::FrontendIdentity {
                kind: omnifs_vfs_wire::FrontendKind::Fuse,
                mount_point: std::path::PathBuf::from("/guest/omnifs"),
            },
            rt.clone(),
        )
        .await
        .unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            let reply = request(&control_socket, ControlOperation::Status).await;
            if let ControlOutcome::Status(status) = reply.outcome
                && status
                    .frontends
                    .iter()
                    .any(|frontend| frontend.runtime == omnifs_api::FrontendRuntime::Docker)
            {
                break status;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert!(
            status
                .health
                .subsystem(omnifs_api::DaemonSubsystem::Frontend)
                .unwrap()
                .message
                .contains("attached fuse at /guest/omnifs via docker")
        );

        drop(wire);
        drop(wire2);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            let reply = request(&control_socket, ControlOperation::Status).await;
            if let ControlOutcome::Status(status) = reply.outcome
                && status
                    .frontends
                    .iter()
                    .all(|frontend| frontend.runtime != omnifs_api::FrontendRuntime::Docker)
            {
                break status;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert!(status.frontends.is_empty());

        assert!(matches!(
            request(&control_socket, ControlOperation::Shutdown)
                .await
                .outcome,
            ControlOutcome::Shutdown
        ));
    }

    #[tokio::test]
    #[allow(unsafe_code)]
    async fn control_socket_rejects_malformed_and_oversized_lines() {
        let dir = tempfile::tempdir().unwrap();
        let _env_guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            br#"{"version":1,"operation":"unknown"}
"#
            .to_vec(),
        )
        .await;
        assert!(matches!(
            unknown.outcome,
            ControlOutcome::Error(error) if error.code == ControlErrorCode::UnknownOperation
        ));

        let oversized = raw_request(&control_socket, vec![b'x'; CONTROL_MAX_LINE_BYTES + 1]).await;
        assert!(matches!(
            oversized.outcome,
            ControlOutcome::Error(error) if error.code == ControlErrorCode::LineTooLarge
        ));
    }
}
