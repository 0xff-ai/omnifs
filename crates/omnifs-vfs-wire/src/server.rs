//! Server for the Omnifs VFS wire protocol.
//!
//! It adapts the engine-owned [`Namespace`] onto a byte stream without owning
//! any VFS semantics.
//!
//! [`VfsServer`] owns the attach listeners and every connection task. A listener
//! binds before its accept task is spawned, and the task reports one exit event
//! after it stops. Both transports serve the same namespace concurrently: a
//! connection dispatches every request onto the namespace on its own task, so
//! one slow op (a provider callout) never head-of-line-blocks the reads behind
//! it, and a background task forwards invalidation events as event frames.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use omnifs_api::{FrontendInfo, FrontendRuntime, FsType};
use omnifs_engine::{Namespace, NsEvent};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinSet;

use crate::frame::{Frame, KIND_EVENT, KIND_REQUEST, KIND_RESPONSE, read_frame, write_frame};
use crate::{FrontendIdentity, Handshake, PROTOCOL, WireError, WireRequest, WireResponse};

const ATTACH_TOKEN_BYTES: usize = 16;
const UDS_PATH_BYTE_LIMIT: usize = 100;

/// The listener path determines both runtime authority and authentication.
/// The guest identity in the handshake remains display-only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ListenerTarget {
    /// The fixed host-native Unix listener authenticated by filesystem mode.
    Local { path: PathBuf },
    /// The Docker runtime listener and its token-authenticated address.
    Tcp { addr: SocketAddr, token: String },
    /// The libkrun vsock-proxy listener and its token-authenticated socket.
    Vsock { socket_path: PathBuf, token: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerEvent {
    /// A listener stopped and its target is no longer live.
    Exited { target: ListenerTarget },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ListenerKind {
    Local,
    Tcp,
    Vsock,
}

impl ListenerTarget {
    fn kind(&self) -> ListenerKind {
        match self {
            Self::Local { .. } => ListenerKind::Local,
            Self::Tcp { .. } => ListenerKind::Tcp,
            Self::Vsock { .. } => ListenerKind::Vsock,
        }
    }

    fn token(&self) -> Option<&str> {
        match self {
            Self::Local { .. } => None,
            Self::Tcp { token, .. } | Self::Vsock { token, .. } => Some(token),
        }
    }

    fn path(&self) -> Option<&Path> {
        match self {
            Self::Local { path } => Some(path),
            Self::Tcp { .. } => None,
            Self::Vsock { socket_path, .. } => Some(socket_path),
        }
    }
}

type Connection = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct ListenerRecord {
    target: ListenerTarget,
    identity: Arc<()>,
    task: tokio::task::JoinHandle<()>,
}

struct VfsState {
    listeners: BTreeMap<ListenerKind, ListenerRecord>,
    ready: bool,
    readiness_enabled: bool,
    shutting_down: bool,
    startup_gate: Option<watch::Sender<bool>>,
}

#[derive(Debug, Clone)]
struct AttachedFrontend {
    kind: crate::FrontendKind,
    mount_point: PathBuf,
    runtime: FrontendRuntime,
}

impl AttachedFrontend {
    fn key(&self) -> AttachmentKey {
        AttachmentKey {
            runtime: match self.runtime {
                FrontendRuntime::Host => 0,
                FrontendRuntime::Docker => 1,
                FrontendRuntime::Libkrun => 2,
            },
            kind: match self.kind {
                crate::FrontendKind::Fuse => 0,
                crate::FrontendKind::Nfs => 1,
            },
            mount_point: self.mount_point.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AttachmentKey {
    runtime: u8,
    kind: u8,
    mount_point: PathBuf,
}

struct AttachedEntry {
    frontend: AttachedFrontend,
    connections: usize,
}

struct AttachmentState {
    next_attachment_id: u64,
    ids: BTreeMap<u64, AttachmentKey>,
    entries: BTreeMap<AttachmentKey, AttachedEntry>,
}

struct Attachments {
    state: Mutex<AttachmentState>,
}

impl Attachments {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(AttachmentState {
                next_attachment_id: 1,
                ids: BTreeMap::new(),
                entries: BTreeMap::new(),
            }),
        })
    }

    fn attached(&self, identity: &crate::FrontendIdentity, runtime: FrontendRuntime) -> u64 {
        let frontend = AttachedFrontend {
            kind: identity.kind,
            mount_point: identity.mount_point.clone(),
            runtime,
        };
        let key = frontend.key();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = state.next_attachment_id;
        state.next_attachment_id += 1;
        state.ids.insert(id, key.clone());
        state
            .entries
            .entry(key)
            .and_modify(|entry| entry.connections += 1)
            .or_insert(AttachedEntry {
                frontend,
                connections: 1,
            });
        id
    }

    fn detached(&self, id: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(key) = state.ids.remove(&id) else {
            return;
        };
        let remove = state.entries.get_mut(&key).is_some_and(|entry| {
            entry.connections -= 1;
            entry.connections == 0
        });
        if remove {
            state.entries.remove(&key);
        }
    }

    fn snapshot(&self) -> Vec<FrontendInfo> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .entries
            .values()
            .map(|entry| FrontendInfo {
                source: "wire".to_string(),
                fs_type: match entry.frontend.kind {
                    crate::FrontendKind::Fuse => FsType::Fuse,
                    crate::FrontendKind::Nfs => FsType::Nfs,
                },
                mount_point: entry.frontend.mount_point.clone(),
                runtime: entry.frontend.runtime,
            })
            .collect()
    }
}

/// Owns the namespace attach listeners, their connection tasks, attach-token
/// authority, live attachment snapshot, readiness, and shutdown.
pub struct VfsServer {
    namespace: Arc<dyn Namespace>,
    attachments: Arc<Attachments>,
    state: Mutex<VfsState>,
    connection_tx: mpsc::UnboundedSender<Connection>,
    connection_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    exit_tx: mpsc::UnboundedSender<(ListenerTarget, Arc<()>)>,
    event_tx: broadcast::Sender<ListenerEvent>,
    reaper_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl VfsServer {
    /// Construct one invocation-scoped listener and attachment owner.
    #[must_use]
    pub fn new(namespace: Arc<dyn Namespace>) -> Arc<Self> {
        let (connection_tx, mut connection_rx) = mpsc::unbounded_channel();
        let (exit_tx, mut exit_rx) = mpsc::unbounded_channel();
        let (event_tx, _) = broadcast::channel(16);
        let server = Arc::new(Self {
            namespace,
            attachments: Attachments::new(),
            state: Mutex::new(VfsState {
                listeners: BTreeMap::new(),
                ready: false,
                readiness_enabled: false,
                shutting_down: false,
                startup_gate: None,
            }),
            connection_tx,
            connection_task: Mutex::new(None),
            exit_tx,
            event_tx,
            reaper_task: Mutex::new(None),
        });

        let connection_task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    connection = connection_rx.recv() => match connection {
                        Some(connection) => { connections.spawn(connection); },
                        None => break,
                    },
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                }
            }
            connections.shutdown().await;
        });
        *server
            .connection_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(connection_task);

        let weak = Arc::downgrade(&server);
        let reaper_task = tokio::spawn(async move {
            while let Some((target, identity)) = exit_rx.recv().await {
                let Some(server) = weak.upgrade() else {
                    break;
                };
                let removed = {
                    let mut state = server
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.shutting_down {
                        false
                    } else if state.listeners.get(&target.kind()).is_some_and(|record| {
                        record.target == target && Arc::ptr_eq(&record.identity, &identity)
                    }) {
                        let record = state.listeners.remove(&target.kind());
                        state.ready = if state.readiness_enabled {
                            listener_set_ready(&state)
                        } else {
                            false
                        };
                        if let Some(path) = record.as_ref().and_then(|record| record.target.path())
                        {
                            unlink_socket(path);
                        }
                        true
                    } else {
                        false
                    }
                };
                if removed {
                    let _ = server.event_tx.send(ListenerEvent::Exited { target });
                }
            }
        });
        *server
            .reaper_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reaper_task);
        server
    }

    #[must_use]
    /// Subscribe to listener failure observations.
    pub fn listener_events(&self) -> broadcast::Receiver<ListenerEvent> {
        self.event_tx.subscribe()
    }

    #[must_use]
    /// Return the current deduplicated live attachment rows.
    pub fn attachments(&self) -> Vec<FrontendInfo> {
        self.attachments.snapshot()
    }

    #[must_use]
    /// Report whether all currently bound listeners passed readiness.
    pub fn ready(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ready
    }

    /// Mark the currently bound listeners ready after startup restoration.
    pub fn mark_ready(&self) {
        let startup_gate = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.readiness_enabled = true;
            state.ready = listener_set_ready(&state);
            state.startup_gate.clone()
        };
        if let Some(startup_gate) = startup_gate {
            let _ = startup_gate.send(true);
        }
    }

    /// Hold listener tasks behind one startup gate until the daemon has
    /// published its durable daemon record.
    pub fn begin_startup(&self) -> watch::Receiver<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(startup_gate) = &state.startup_gate {
            return startup_gate.subscribe();
        }
        let (startup_gate, receiver) = watch::channel(false);
        state.startup_gate = Some(startup_gate);
        receiver
    }

    /// Bind the fixed local UDS before starting its accept task.
    pub fn serve_local(self: &Arc<Self>, path: PathBuf) -> io::Result<ListenerTarget> {
        if let Some(target) = self.existing(ListenerKind::Local) {
            return Ok(target);
        }
        let listener = bind_unix(&path, "local attach socket")?;
        self.install(ListenerTarget::Local { path }, Listener::Unix(listener))
            .map(|(target, _)| target)
    }

    /// Bind or return the token-authenticated TCP listener for Docker runtime.
    pub fn ensure_tcp(
        self: &Arc<Self>,
        bind_addr: Ipv4Addr,
        port: u16,
        requested_token: Option<String>,
    ) -> io::Result<ListenerTarget> {
        self.ensure_tcp_with_status(bind_addr, port, requested_token)
            .map(|(target, _)| target)
    }

    /// Bind or return the TCP listener and report whether this call created it.
    /// Daemon persistence uses the ownership bit to roll back only a listener
    /// created by the failing control operation.
    pub fn ensure_tcp_with_status(
        self: &Arc<Self>,
        bind_addr: Ipv4Addr,
        port: u16,
        requested_token: Option<String>,
    ) -> io::Result<(ListenerTarget, bool)> {
        if let Some(target) = self.existing(ListenerKind::Tcp) {
            return Ok((target, false));
        }
        let token = requested_token.map_or_else(generate_attach_token, validate_attach_token)?;
        let std_listener = std::net::TcpListener::bind((bind_addr, port))?;
        std_listener.set_nonblocking(true)?;
        let addr = std_listener.local_addr()?;
        let listener = TcpListener::from_std(std_listener)?;
        self.install(ListenerTarget::Tcp { addr, token }, Listener::Tcp(listener))
    }

    /// Bind or return the token-authenticated UDS used by the vsock proxy.
    pub fn ensure_vsock(
        self: &Arc<Self>,
        path: &Path,
        requested_token: Option<String>,
    ) -> io::Result<ListenerTarget> {
        self.ensure_vsock_with_status(path, requested_token)
            .map(|(target, _)| target)
    }

    /// Bind or return the vsock listener and report whether this call created it.
    /// Daemon persistence uses the ownership bit to roll back only a listener
    /// created by the failing control operation.
    pub fn ensure_vsock_with_status(
        self: &Arc<Self>,
        path: &Path,
        requested_token: Option<String>,
    ) -> io::Result<(ListenerTarget, bool)> {
        if let Some(target) = self.existing(ListenerKind::Vsock) {
            return Ok((target, false));
        }
        let token = requested_token.map_or_else(generate_attach_token, validate_attach_token)?;
        let target = ListenerTarget::Vsock {
            socket_path: path.to_path_buf(),
            token,
        };
        let listener = bind_unix(path, "vsock attach socket")?;
        match self.install(target, Listener::Unix(listener)) {
            Ok(binding) => {
                if !binding.1 && binding.0.path() != Some(path) {
                    unlink_socket(path);
                }
                Ok(binding)
            },
            Err(error) => {
                unlink_socket(path);
                Err(error)
            },
        }
    }

    /// Remove one exact listener owned by this server.
    pub fn remove_listener(&self, target: &ListenerTarget) -> bool {
        let (task, path) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(record) = state.listeners.get(&target.kind()) else {
                return false;
            };
            if &record.target != target {
                return false;
            }
            let record = state
                .listeners
                .remove(&target.kind())
                .expect("listener existed immediately before removal");
            state.ready = if state.readiness_enabled {
                listener_set_ready(&state)
            } else {
                false
            };
            (record.task, record.target.path().map(PathBuf::from))
        };
        task.abort();
        if let Some(path) = path {
            unlink_socket(&path);
        }
        true
    }

    /// Stop listeners and connection tasks, then remove owned UDS paths.
    pub async fn shutdown(&self) {
        let (tasks, paths, connection_task, reaper_task) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.shutting_down = true;
            state.ready = false;
            let records = std::mem::take(&mut state.listeners);
            let paths = records
                .values()
                .filter_map(|record| record.target.path().map(PathBuf::from))
                .collect::<Vec<_>>();
            let tasks = records
                .into_values()
                .map(|record| record.task)
                .collect::<Vec<_>>();
            let connection_task = self
                .connection_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let reaper_task = self
                .reaper_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            (tasks, paths, connection_task, reaper_task)
        };
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        if let Some(task) = connection_task {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = reaper_task {
            task.abort();
            let _ = task.await;
        }
        for path in paths {
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(%error, path = %path.display(), "failed to remove attach socket");
            }
        }
    }

    fn existing(&self, kind: ListenerKind) -> Option<ListenerTarget> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .listeners
            .get(&kind)
            .is_some_and(|record| !record.task.is_finished())
        {
            return state
                .listeners
                .get(&kind)
                .map(|record| record.target.clone());
        }
        if let Some(record) = state.listeners.remove(&kind) {
            state.ready = if state.readiness_enabled {
                listener_set_ready(&state)
            } else {
                false
            };
            if let Some(path) = record.target.path() {
                unlink_socket(path);
            }
        }
        None
    }

    fn install(
        self: &Arc<Self>,
        target: ListenerTarget,
        listener: Listener,
    ) -> io::Result<(ListenerTarget, bool)> {
        let kind = target.kind();
        if let Some(existing) = self.existing(kind) {
            return Ok((existing, false));
        }
        let startup_gate = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .startup_gate
            .as_ref()
            .map(watch::Sender::subscribe);
        let target_for_task = target.clone();
        let identity = Arc::new(());
        let task_identity = Arc::clone(&identity);
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let namespace = Arc::clone(&self.namespace);
        let attachments = Arc::clone(&self.attachments);
        let connection_tx = self.connection_tx.clone();
        let exit_tx = self.exit_tx.clone();
        let runtime = match kind {
            ListenerKind::Local => FrontendRuntime::Host,
            ListenerKind::Tcp => FrontendRuntime::Docker,
            ListenerKind::Vsock => FrontendRuntime::Libkrun,
        };
        let task = tokio::spawn(async move {
            if start_rx.await.is_err() {
                return;
            }
            if let Some(mut startup_gate) = startup_gate {
                let cancelled = if *startup_gate.borrow() {
                    false
                } else {
                    startup_gate.changed().await.is_err() || !*startup_gate.borrow()
                };
                if cancelled {
                    return;
                }
            }
            accept_loop(
                listener,
                namespace,
                target_for_task.token().map(str::to_owned),
                runtime,
                attachments,
                connection_tx,
            )
            .await;
            let _ = exit_tx.send((target_for_task, task_identity));
        });
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.shutting_down {
            task.abort();
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "VFS server is shutting down",
            ));
        }
        state.listeners.insert(
            kind,
            ListenerRecord {
                target: target.clone(),
                identity,
                task,
            },
        );
        if state.readiness_enabled {
            state.ready = listener_set_ready(&state);
        }
        drop(state);
        let _ = start_tx.send(());
        Ok((target, true))
    }
}

enum Listener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

fn listener_set_ready(state: &VfsState) -> bool {
    !state.shutting_down
        && state
            .listeners
            .get(&ListenerKind::Local)
            .is_some_and(|record| !record.task.is_finished())
        && state
            .listeners
            .values()
            .all(|record| !record.task.is_finished())
}

fn unlink_socket(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %path.display(), "failed to remove stopped attach socket");
    }
}

async fn accept_loop(
    listener: Listener,
    namespace: Arc<dyn Namespace>,
    token: Option<String>,
    runtime: FrontendRuntime,
    attachments: Arc<Attachments>,
    connection_tx: mpsc::UnboundedSender<Connection>,
) {
    match listener {
        Listener::Unix(listener) => loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let namespace = Arc::clone(&namespace);
                    let token = token.clone();
                    let attachments = Arc::clone(&attachments);
                    if connection_tx
                        .send(Box::pin(async move {
                            if let Err(error) = serve_connection_with_registry(
                                namespace,
                                stream,
                                token.as_deref(),
                                Some((attachments, runtime)),
                            )
                            .await
                            {
                                tracing::debug!(%error, "wire: connection ended with a protocol error");
                            }
                        }))
                        .is_err()
                    {
                        break;
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "wire: unix attach listener stopped");
                    break;
                },
            }
        },
        Listener::Tcp(listener) => loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let namespace = Arc::clone(&namespace);
                    let token = token.clone();
                    let attachments = Arc::clone(&attachments);
                    if connection_tx
                        .send(Box::pin(async move {
                            if let Err(error) = serve_connection_with_registry(
                                namespace,
                                stream,
                                token.as_deref(),
                                Some((attachments, runtime)),
                            )
                            .await
                            {
                                tracing::debug!(%error, "wire: tcp connection ended with a protocol error");
                            }
                        }))
                        .is_err()
                    {
                        break;
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "wire: tcp attach listener stopped");
                    break;
                },
            }
        },
    }
}

fn generate_attach_token() -> io::Result<String> {
    let mut bytes = [0_u8; ATTACH_TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(hex::encode(bytes))
}

fn validate_attach_token(token: String) -> io::Result<String> {
    if token.len() != ATTACH_TOKEN_BYTES * 2
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "persisted attach token must be 32 lowercase hexadecimal characters",
        ));
    }
    Ok(token)
}

fn bind_unix(path: &Path, description: &str) -> io::Result<UnixListener> {
    use std::os::unix::ffi::OsStrExt as _;
    let len = path.as_os_str().as_bytes().len();
    if len >= UDS_PATH_BYTE_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "attach socket path {} is {len} bytes, at or beyond the {UDS_PATH_BYTE_LIMIT}-byte sockaddr_un budget",
                path.display()
            ),
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::remove_file(path)?,
        Ok(_) => match UnixStream::connect(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another daemon is serving {description} {}", path.display()),
                ));
            },
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(path)?;
            },
            Err(error) => return Err(error),
        },
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error);
            }
        },
    }
    let listener = UnixListener::bind(path)?;
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        drop(listener);
        std::fs::remove_file(path)?;
        return Err(error);
    }
    Ok(listener)
}

/// Serve one attached client over `stream` until it disconnects. `expected_token`
/// is `None` for a Unix-socket
/// listener (the field is ignored) and `Some(token)` for a TCP attach listener,
/// which rejects a Hello whose token does not match. Production listeners are
/// owned by [`VfsServer`]; this direct helper is retained for protocol tests.
///
/// Returns `Ok(())` on an orderly client disconnect and a [`WireError`] on a
/// protocol fault (an oversized frame, a malformed handshake, a version
/// mismatch, a bad token); a fault drops the connection.
pub async fn serve_connection<S>(
    namespace: Arc<dyn Namespace>,
    stream: S,
    expected_token: Option<&str>,
) -> Result<(), WireError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    serve_connection_with_registry(namespace, stream, expected_token, None).await
}

async fn serve_connection_with_registry<S>(
    namespace: Arc<dyn Namespace>,
    stream: S,
    expected_token: Option<&str>,
    attachment: Option<(Arc<Attachments>, FrontendRuntime)>,
) -> Result<(), WireError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    // A single writer task owns the write half; responses (from per-request
    // tasks) and events (from the forwarder) are serialized through its channel,
    // so frames never interleave on the wire.
    // Complete the handshake before subscribing to namespace events so Welcome
    // is always the first server frame on a new connection.
    let handshake_result = server_handshake(&mut reader, &mut writer, expected_token).await;
    let identity = match handshake_result {
        Ok(identity) => identity,
        Err(error) => {
            return Err(error);
        },
    };

    let _attach_guard = attachment.map(|(attachments, runtime)| AttachGuard {
        id: attachments.attached(&identity, runtime),
        attachments,
    });

    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Frame>();
    let mut events = namespace.subscribe();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        loop {
            tokio::select! {
                biased;
                frame = outbound_rx.recv() => {
                    let Some(frame) = frame else { break; };
                    let mut drained = 0;
                    while let Some(event) = events.try_recv() {
                        let Ok(body) = postcard::to_allocvec(&event) else { continue; };
                        if write_frame(&mut writer, &Frame::new(0, KIND_EVENT, body)).await.is_err() { return; }
                        drained += 1;
                        if drained >= 1024 {
                            let root = NsEvent::reset();
                            let Ok(body) = postcard::to_allocvec(&root) else { return; };
                            if write_frame(&mut writer, &Frame::new(0, KIND_EVENT, body)).await.is_err() { return; }
                            break;
                        }
                    }
                    if write_frame(&mut writer, &frame).await.is_err() { break; }
                }
                event = events.recv() => {
                    let Some(event) = event else { break; };
                    let Ok(body) = postcard::to_allocvec(&event) else { continue; };
                    if write_frame(&mut writer, &Frame::new(0, KIND_EVENT, body)).await.is_err() { break; }
                }
            }
        }
    });

    let read_result = read_loop(&mut reader, &namespace, &outbound_tx).await;

    // Dropping the last outbound sender lets the writer task drain and exit.
    drop(outbound_tx);
    let _ = writer_task.await;
    read_result
}

/// Read the client's `Hello`, check the protocol and (when `expected_token` is
/// set) the token, and answer with `Welcome` or `Rejected`. On success returns
/// the connecting frontend's identity.
async fn server_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    expected_token: Option<&str>,
) -> Result<FrontendIdentity, WireError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let frame = read_frame(reader)
        .await?
        .ok_or(WireError::HandshakeClosed)?;
    if frame.kind != KIND_REQUEST {
        return Err(WireError::HandshakeUnexpected { expected: "hello" });
    }
    let hello: Handshake = postcard::from_bytes(&frame.body)?;
    let Handshake::Hello {
        protocol,
        token,
        frontend,
    } = hello
    else {
        return Err(WireError::HandshakeUnexpected { expected: "hello" });
    };
    if protocol != PROTOCOL {
        let error = WireError::VersionMismatch {
            ours: PROTOCOL,
            theirs: protocol,
        };
        send_rejected(writer, error.to_string()).await?;
        return Err(error);
    }
    if let Some(expected) = expected_token {
        let presented = token.as_deref().unwrap_or_default();
        if !constant_time_eq::constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            send_rejected(writer, "attach token rejected".to_string()).await?;
            return Err(WireError::TokenRejected);
        }
    }
    let welcome = Handshake::Welcome { protocol: PROTOCOL };
    let body = postcard::to_allocvec(&welcome)?;
    write_frame(writer, &Frame::new(0, KIND_RESPONSE, body)).await?;
    Ok(frontend)
}

struct AttachGuard {
    attachments: Arc<Attachments>,
    id: u64,
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        self.attachments.detached(self.id);
    }
}

/// Queue a `Handshake::Rejected` frame naming `reason`, best-effort: the caller
/// is already on its way to returning an error regardless of whether the frame
/// lands (the writer task may already be gone).
async fn send_rejected<W>(writer: &mut W, reason: String) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    if let Ok(body) = postcard::to_allocvec(&Handshake::Rejected { reason }) {
        write_frame(writer, &Frame::new(0, KIND_RESPONSE, body)).await?;
    }
    Ok(())
}

/// The per-connection read loop: decode each request frame and dispatch it onto
/// the namespace on its own task. Returns when the client disconnects (`Ok`) or
/// sends a malformed/oversized frame (`Err`).
async fn read_loop<R>(
    reader: &mut R,
    namespace: &Arc<dyn Namespace>,
    outbound_tx: &mpsc::UnboundedSender<Frame>,
) -> Result<(), WireError>
where
    R: AsyncRead + Unpin,
{
    let mut requests = JoinSet::new();
    loop {
        let Some(frame) = read_frame(reader).await? else {
            return Ok(());
        };
        if frame.kind != KIND_REQUEST {
            return Err(WireError::Protocol(format!(
                "client sent a non-request frame of kind {}",
                frame.kind
            )));
        }
        let request: WireRequest = postcard::from_bytes(&frame.body)?;
        let request_id = frame.request_id;
        let namespace = Arc::clone(namespace);
        let outbound_tx = outbound_tx.clone();
        requests.spawn(async move {
            let response = dispatch(namespace.as_ref(), request).await;
            match postcard::to_allocvec(&response) {
                Ok(body) => {
                    let _ = outbound_tx.send(Frame::new(request_id, KIND_RESPONSE, body));
                },
                Err(error) => {
                    tracing::warn!(%error, "wire: failed to encode namespace response");
                },
            }
        });
    }
}

/// Run one request against the namespace, wrapping the answer in its
/// [`WireResponse`] variant.
async fn dispatch(namespace: &dyn Namespace, request: WireRequest) -> WireResponse {
    match request {
        WireRequest::Lookup { parent, name } => {
            WireResponse::Lookup(namespace.lookup(parent, &name).await)
        },
        WireRequest::Getattr { path } => WireResponse::Getattr(namespace.getattr(path).await),
        WireRequest::GetattrExact { path } => {
            WireResponse::GetattrExact(namespace.getattr_exact(path).await)
        },
        WireRequest::Readdir {
            path,
            cursor,
            budget,
        } => WireResponse::Readdir(
            namespace
                .readdir(path, cursor, usize::try_from(budget).unwrap_or(usize::MAX))
                .await,
        ),
        WireRequest::Read { path, offset, len } => {
            WireResponse::Read(namespace.read(path, offset, len).await)
        },
        WireRequest::Readlink { path } => WireResponse::Readlink(namespace.readlink(path).await),
    }
}
