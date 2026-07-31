//! Server for the Omnifs VFS wire protocol.
//!
//! It adapts the engine-owned [`ServingNamespace`] onto a byte stream without
//! owning any VFS semantics.
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
use std::num::NonZeroU16;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{NamespaceEpoch, NamespaceLease, ServingNamespace};
use omnifs_core::{AttachmentSpec, ResourceName};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::frame::{
    Frame, KIND_CONTROL, KIND_EVENT, KIND_HEARTBEAT, KIND_REQUEST, KIND_RESPONSE, read_frame,
    write_frame,
};
use crate::{Handshake, PROTOCOL, ServerControl, WireError, WireReply, WireRequest, WireResponse};

const UDS_PATH_BYTE_LIMIT: usize = 100;
const CONNECTION_QUEUE_CAPACITY: usize = 128;
const MAX_ACTIVE_CONNECTIONS: usize = 128;
const LISTENER_EXIT_QUEUE_CAPACITY: usize = 16;
const OUTBOUND_QUEUE_CAPACITY: usize = 256;
const MAX_IN_FLIGHT_REQUESTS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Endpoint {
    Unix { path: PathBuf },
    Tcp { addr: SocketAddr },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerEvent {
    /// A required endpoint stopped and is no longer live.
    Exited { endpoint: Endpoint },
}

/// One live VFS session, keyed by its desired attachment name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub attachment: ResourceName,
    pub spec: AttachmentSpec,
    pub runtime_instance: String,
}

impl Endpoint {
    fn path(&self) -> Option<&Path> {
        match self {
            Self::Unix { path } => Some(path),
            Self::Tcp { .. } => None,
        }
    }
}

type Connection = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct ListenerRecord {
    endpoint: Endpoint,
    identity: Arc<()>,
    task: tokio::task::JoinHandle<()>,
}

struct VfsState {
    listeners: BTreeMap<Endpoint, ListenerRecord>,
    ready: bool,
    readiness_enabled: bool,
    shutting_down: bool,
    startup_gate: Option<watch::Sender<bool>>,
}

struct SessionEntry {
    spec: AttachmentSpec,
    runtime_instance: String,
    connections: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SessionKey(ResourceName);

struct SessionConnection {
    key: SessionKey,
    control: watch::Sender<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionRegistryPhase {
    Running,
    Draining,
    ShuttingDown,
}

struct SessionState {
    next_session_id: u64,
    connections: BTreeMap<u64, SessionConnection>,
    entries: BTreeMap<SessionKey, SessionEntry>,
    replacements: BTreeMap<SessionKey, Session>,
    phase: SessionRegistryPhase,
}

struct Sessions {
    state: Mutex<SessionState>,
    changed: watch::Sender<usize>,
}

impl Sessions {
    fn new() -> Arc<Self> {
        let (changed, _) = watch::channel(0);
        Arc::new(Self {
            state: Mutex::new(SessionState {
                next_session_id: 1,
                connections: BTreeMap::new(),
                entries: BTreeMap::new(),
                replacements: BTreeMap::new(),
                phase: SessionRegistryPhase::Running,
            }),
            changed,
        })
    }

    fn connected(
        &self,
        attachment: ResourceName,
        spec: &AttachmentSpec,
        runtime_instance: &str,
        control: watch::Sender<bool>,
    ) -> Result<u64, String> {
        let key = SessionKey(attachment);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.phase != SessionRegistryPhase::Running {
            return Err("daemon is draining and is not accepting VFS sessions".to_owned());
        }
        if let Some(existing) = state.entries.get(&key) {
            if existing.spec != *spec {
                return Err(format!(
                    "attachment `{}` already has a VFS session with a different exact spec",
                    key.0
                ));
            }
            if existing.runtime_instance != runtime_instance {
                return Err(format!(
                    "attachment `{}` already has a VFS session for a different runtime instance; supervisor replacement approval is required",
                    key.0
                ));
            }
        } else if let Some(approved) = state.replacements.get(&key) {
            let requested = Session {
                attachment: key.0.clone(),
                spec: spec.clone(),
                runtime_instance: runtime_instance.to_owned(),
            };
            if approved != &requested {
                return Err(format!(
                    "attachment `{}` has a supervisor-approved replacement for a different exact runtime identity",
                    key.0
                ));
            }
            state.replacements.remove(&key);
        }
        let id = state.next_session_id;
        state.next_session_id += 1;
        state.connections.insert(
            id,
            SessionConnection {
                key: key.clone(),
                control,
            },
        );
        state
            .entries
            .entry(key.clone())
            .and_modify(|entry| entry.connections += 1)
            .or_insert(SessionEntry {
                spec: spec.clone(),
                runtime_instance: runtime_instance.to_owned(),
                connections: 1,
            });
        Self::publish_locked(&mut state, &self.changed);
        tracing::debug!(
            attachment = %key.0,
            runtime_instance,
            connections = state.entries.get(&key).map_or(0, |entry| entry.connections),
            "wire: VFS session connected"
        );
        Ok(id)
    }

    fn disconnected(&self, id: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(connection) = state.connections.remove(&id) else {
            return;
        };
        let key = connection.key;
        let remove = state.entries.get_mut(&key).is_some_and(|entry| {
            entry.connections -= 1;
            entry.connections == 0
        });
        if remove {
            state.entries.remove(&key);
        }
        Self::publish_locked(&mut state, &self.changed);
        tracing::debug!(
            attachment = %key.0,
            connections = state.entries.get(&key).map_or(0, |entry| entry.connections),
            "wire: VFS session disconnected"
        );
    }

    fn begin_replacement(&self, previous: &Session, replacement: &Session) -> Result<(), String> {
        if previous.attachment != replacement.attachment || previous.spec != replacement.spec {
            return Err(
                "VFS session replacement must retain the attachment name and exact spec".to_owned(),
            );
        }
        if previous.runtime_instance == replacement.runtime_instance {
            return Err("VFS session replacement requires a new runtime instance".to_owned());
        }
        let key = SessionKey(previous.attachment.clone());
        let controls = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(current) = state.entries.get(&key) else {
                return Err(format!(
                    "attachment `{}` has no live VFS session to replace",
                    previous.attachment
                ));
            };
            if current.spec != previous.spec
                || current.runtime_instance != previous.runtime_instance
            {
                return Err(format!(
                    "attachment `{}` changed before VFS session replacement was approved",
                    previous.attachment
                ));
            }
            state.replacements.insert(key.clone(), replacement.clone());
            state
                .connections
                .values()
                .filter(|connection| connection.key == key)
                .map(|connection| connection.control.clone())
                .collect::<Vec<_>>()
        };
        for control in controls {
            control.send_replace(true);
        }
        Ok(())
    }

    fn snapshot(&self) -> Vec<Session> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .entries
            .iter()
            .map(|(key, entry)| Session {
                attachment: key.0.clone(),
                spec: entry.spec.clone(),
                runtime_instance: entry.runtime_instance.clone(),
            })
            .collect()
    }

    fn stop_filesystems(&self) {
        let controls = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.phase == SessionRegistryPhase::ShuttingDown {
                return;
            }
            state.phase = SessionRegistryPhase::Draining;
            Self::publish_locked(&mut state, &self.changed);
            state
                .connections
                .values()
                .map(|connection| connection.control.clone())
                .collect::<Vec<_>>()
        };
        for control in controls {
            control.send_replace(true);
        }
    }

    async fn drain(&self, timeout: Duration) -> Vec<Session> {
        let deadline = Instant::now() + timeout;
        let mut changed = self.changed.subscribe();
        loop {
            let remaining = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.entries.is_empty() {
                    return Vec::new();
                }
                deadline.saturating_duration_since(Instant::now())
            };
            if remaining.is_zero()
                || tokio::time::timeout(remaining, changed.changed())
                    .await
                    .is_err()
            {
                return self.identities();
            }
        }
    }

    fn identities(&self) -> Vec<Session> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .iter()
            .map(|(key, entry)| Session {
                attachment: key.0.clone(),
                spec: entry.spec.clone(),
                runtime_instance: entry.runtime_instance.clone(),
            })
            .collect()
    }

    fn shut_down(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.phase = SessionRegistryPhase::ShuttingDown;
        Self::publish_locked(&mut state, &self.changed);
    }

    fn publish_locked(state: &mut SessionState, changed: &watch::Sender<usize>) {
        changed.send_replace(state.connections.len());
    }
}

/// Owns the namespace attach listeners, their connection tasks, live session
/// snapshot, readiness, and shutdown.
pub struct VfsServer {
    namespace: Arc<dyn ServingNamespace>,
    sessions: Arc<Sessions>,
    state: Mutex<VfsState>,
    connection_tx: mpsc::Sender<Connection>,
    connection_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    exit_tx: mpsc::Sender<(Endpoint, Arc<()>)>,
    event_tx: broadcast::Sender<ListenerEvent>,
    reaper_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl VfsServer {
    /// Construct one invocation-scoped listener and session owner.
    #[must_use]
    pub fn new(namespace: Arc<dyn ServingNamespace>) -> Arc<Self> {
        let (connection_tx, mut connection_rx) = mpsc::channel(CONNECTION_QUEUE_CAPACITY);
        let (exit_tx, mut exit_rx) = mpsc::channel(LISTENER_EXIT_QUEUE_CAPACITY);
        let (event_tx, _) = broadcast::channel(16);
        let server = Arc::new(Self {
            namespace,
            sessions: Sessions::new(),
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
                    connection = connection_rx.recv(), if connections.len() < MAX_ACTIVE_CONNECTIONS => match connection {
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
            while let Some((endpoint, identity)) = exit_rx.recv().await {
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
                    } else if state
                        .listeners
                        .get(&endpoint)
                        .is_some_and(|record| Arc::ptr_eq(&record.identity, &identity))
                    {
                        let record = state.listeners.remove(&endpoint);
                        // Every installed endpoint is required. Once one
                        // exits, this daemon lifetime cannot become ready
                        // again without rebuilding the complete listener set.
                        state.ready = false;
                        if let Some(path) =
                            record.as_ref().and_then(|record| record.endpoint.path())
                        {
                            unlink_socket(path);
                        }
                        true
                    } else {
                        false
                    }
                };
                if removed {
                    let _ = server.event_tx.send(ListenerEvent::Exited { endpoint });
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
    /// Subscribe to listener failure events.
    pub fn listener_events(&self) -> broadcast::Receiver<ListenerEvent> {
        self.event_tx.subscribe()
    }

    #[must_use]
    /// Return the current deduplicated live VFS sessions without waiting.
    pub fn sessions(&self) -> Vec<Session> {
        self.sessions.snapshot()
    }

    /// Subscribe to session-registry changes. Receivers only carry a monotonic
    /// notification value; callers read [`Self::sessions`] for a snapshot.
    pub fn session_changes(&self) -> watch::Receiver<usize> {
        self.sessions.changed.subscribe()
    }

    /// Wait for an exact session identity. A caller can use this after a
    /// supervisor-approved launch without polling or racing registration.
    pub async fn wait_for_session(&self, expected: &Session, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut changed = self.session_changes();
        loop {
            if self.sessions().iter().any(|actual| actual == expected) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero()
                || tokio::time::timeout(remaining, changed.changed())
                    .await
                    .is_err()
            {
                return false;
            }
        }
    }

    /// Approve one supervisor-owned replacement. The server sends `Stop` to
    /// the exact old runtime and admits the exact new runtime only after the
    /// old session disconnects. No other instance can take the name during
    /// that fence.
    pub fn begin_session_replacement(
        &self,
        previous: &Session,
        replacement: &Session,
    ) -> Result<(), String> {
        self.sessions.begin_replacement(previous, replacement)
    }

    /// Stop admitting sessions and push a stop command to every live
    /// connection.
    pub fn stop_sessions(&self) {
        self.sessions.stop_filesystems();
    }

    /// Wait until every session has disconnected or `timeout` expires.
    pub async fn drain_sessions(&self, timeout: Duration) -> Vec<Session> {
        self.sessions.drain(timeout).await
    }

    #[must_use]
    /// Report whether all currently bound listeners passed readiness.
    pub fn ready(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ready
    }

    /// Mark the currently bound fixed listeners ready after startup.
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

    /// Bind one Unix endpoint before starting its accept task.
    pub fn serve_unix(self: &Arc<Self>, path: &Path) -> io::Result<Endpoint> {
        let endpoint = Endpoint::Unix {
            path: path.to_path_buf(),
        };
        if let Some(endpoint) = self.existing(&endpoint) {
            return Ok(endpoint);
        }
        let listener = bind_unix(path, "local attach socket")?;
        self.install(endpoint, Listener::Unix(listener))
    }

    /// Bind one TCP endpoint before starting its accept task.
    pub fn serve_tcp(
        self: &Arc<Self>,
        bind_addr: Ipv4Addr,
        port: NonZeroU16,
    ) -> io::Result<Endpoint> {
        let addr = SocketAddr::from((bind_addr, port.get()));
        let endpoint = Endpoint::Tcp { addr };
        if let Some(endpoint) = self.existing(&endpoint) {
            return Ok(endpoint);
        }
        let std_listener = std::net::TcpListener::bind(addr)?;
        std_listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(std_listener)?;
        self.install(endpoint, Listener::Tcp(listener))
    }

    /// Stop listeners and connection tasks, then remove owned UDS paths.
    pub async fn shutdown(&self) {
        self.sessions.shut_down();
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
                .filter_map(|record| record.endpoint.path().map(PathBuf::from))
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

    fn existing(&self, endpoint: &Endpoint) -> Option<Endpoint> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .listeners
            .get(endpoint)
            .is_some_and(|record| !record.task.is_finished())
        {
            return Some(endpoint.clone());
        }
        if let Some(record) = state.listeners.remove(endpoint) {
            state.ready = false;
            if let Some(path) = record.endpoint.path() {
                unlink_socket(path);
            }
        }
        None
    }

    fn install(self: &Arc<Self>, endpoint: Endpoint, listener: Listener) -> io::Result<Endpoint> {
        if let Some(existing) = self.existing(&endpoint) {
            return Ok(existing);
        }
        let startup_gate = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .startup_gate
            .as_ref()
            .map(watch::Sender::subscribe);
        let endpoint_for_task = endpoint.clone();
        let identity = Arc::new(());
        let task_identity = Arc::clone(&identity);
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let namespace = Arc::clone(&self.namespace);
        let sessions = Arc::clone(&self.sessions);
        let connection_tx = self.connection_tx.clone();
        let exit_tx = self.exit_tx.clone();
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
            accept_loop(listener, namespace, sessions, connection_tx).await;
            let _ = exit_tx.send((endpoint_for_task, task_identity)).await;
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
            endpoint.clone(),
            ListenerRecord {
                endpoint: endpoint.clone(),
                identity,
                task,
            },
        );
        if state.readiness_enabled {
            state.ready = listener_set_ready(&state);
        }
        drop(state);
        let _ = start_tx.send(());
        Ok(endpoint)
    }
}

enum Listener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

fn listener_set_ready(state: &VfsState) -> bool {
    !state.shutting_down
        && !state.listeners.is_empty()
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
    namespace: Arc<dyn ServingNamespace>,
    sessions: Arc<Sessions>,
    connection_tx: mpsc::Sender<Connection>,
) {
    match listener {
        Listener::Unix(listener) => loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let namespace = Arc::clone(&namespace);
                    let sessions = Arc::clone(&sessions);
                    if connection_tx
                        .send(Box::pin(async move {
                            if let Err(error) = serve_connection_with_registry(
                                namespace,
                                stream,
                                Some(sessions),
                            )
                            .await
                            {
                                tracing::debug!(%error, "wire: connection ended with a protocol error");
                            }
                        }))
                        .await
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
                    let sessions = Arc::clone(&sessions);
                    if connection_tx
                        .send(Box::pin(async move {
                            if let Err(error) = serve_connection_with_registry(
                                namespace,
                                stream,
                                Some(sessions),
                            )
                            .await
                            {
                                tracing::debug!(%error, "wire: tcp connection ended with a protocol error");
                            }
                        }))
                        .await
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

/// Serve one attached client over `stream` until it disconnects. Production
/// listeners are owned by [`VfsServer`]; this direct helper is retained for
/// protocol tests.
///
/// Returns `Ok(())` on an orderly client disconnect and a [`WireError`] on a
/// protocol fault (an oversized frame, a malformed handshake, or a version
/// mismatch); a fault drops the connection.
pub async fn serve_connection<S>(
    namespace: Arc<dyn ServingNamespace>,
    stream: S,
) -> Result<(), WireError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    serve_connection_with_registry(namespace, stream, None).await
}

async fn serve_connection_with_registry<S>(
    namespace: Arc<dyn ServingNamespace>,
    stream: S,
    sessions: Option<Arc<Sessions>>,
) -> Result<(), WireError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Frame>(OUTBOUND_QUEUE_CAPACITY);
    let (control_tx, mut control_rx) = watch::channel(false);
    let hello = read_hello(&mut reader, &mut writer).await?;
    let mut events = namespace.subscribe();
    let session_guard = if let Some(sessions) = sessions {
        let id = match sessions.connected(
            hello.attachment,
            &hello.spec,
            &hello.runtime_instance,
            control_tx.clone(),
        ) {
            Ok(id) => id,
            Err(reason) => {
                send_rejected(&mut writer, reason.clone()).await?;
                return Err(WireError::Rejected(reason));
            },
        };
        Some(SessionGuard { sessions, id })
    } else {
        None
    };
    send_welcome(&mut writer, events.initial_epoch()).await?;

    // A single writer task owns the write half; responses (from per-request
    // tasks), events, and server controls are serialized through its channel,
    // so frames never interleave on the wire. Registration and Welcome occur
    // before the task starts, preserving handshake ordering.
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        loop {
            tokio::select! {
                biased;
                changed = control_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if !*control_rx.borrow_and_update() {
                        continue;
                    }
                    let Ok(body) = postcard::to_allocvec(&ServerControl::Stop) else { return; };
                    if write_frame(&mut writer, &Frame::new(0, KIND_CONTROL, body)).await.is_err() {
                        return;
                    }
                }
                frame = outbound_rx.recv() => {
                    let Some(frame) = frame else { break; };
                    let mut drained = 0;
                    while let Some(event) = events.try_recv() {
                        let Ok(body) = postcard::to_allocvec(&event) else { continue; };
                        if write_frame(&mut writer, &Frame::new(0, KIND_EVENT, body)).await.is_err() { return; }
                        drained += 1;
                        if drained >= 1024 {
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

    // The session registry also holds an outbound sender. Abort the writer
    // before dropping the guard so disconnect cannot form a sender/guard
    // lifetime cycle that leaves the session registered forever.
    drop(outbound_tx);
    writer_task.abort();
    let _ = writer_task.await;
    drop(session_guard);
    read_result
}

/// Read the client's `Hello` and check the protocol. The caller performs
/// session admission before sending `Welcome`.
struct AttachHello {
    attachment: ResourceName,
    spec: AttachmentSpec,
    runtime_instance: String,
}

async fn read_hello<R, W>(reader: &mut R, writer: &mut W) -> Result<AttachHello, WireError>
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
        attachment,
        spec,
        runtime_instance,
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
    let runtime_instance = match omnifs_core::RuntimeInstanceId::new(runtime_instance) {
        Ok(runtime_instance) => runtime_instance.into_string(),
        Err(error) => {
            let error = WireError::Protocol(error.to_string());
            send_rejected(writer, error.to_string()).await?;
            return Err(error);
        },
    };
    Ok(AttachHello {
        attachment,
        spec,
        runtime_instance,
    })
}

async fn send_welcome<W>(writer: &mut W, epoch: NamespaceEpoch) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    let welcome = Handshake::Welcome {
        protocol: PROTOCOL,
        epoch,
    };
    let body = postcard::to_allocvec(&welcome)?;
    write_frame(writer, &Frame::new(0, KIND_RESPONSE, body)).await?;
    Ok(())
}

struct SessionGuard {
    sessions: Arc<Sessions>,
    id: u64,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.sessions.disconnected(self.id);
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
    namespace: &Arc<dyn ServingNamespace>,
    outbound_tx: &mpsc::Sender<Frame>,
) -> Result<(), WireError>
where
    R: AsyncRead + Unpin,
{
    let mut requests = JoinSet::new();
    loop {
        while requests.try_join_next().is_some() {}
        if requests.len() >= MAX_IN_FLIGHT_REQUESTS {
            let _ = requests.join_next().await;
            continue;
        }
        let Some(frame) = read_frame(reader).await? else {
            return Ok(());
        };
        if frame.kind == KIND_HEARTBEAT {
            let _ = outbound_tx.try_send(Frame::new(0, KIND_HEARTBEAT, Vec::new()));
            continue;
        }
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
            let body = match namespace.acquire() {
                Ok(lease) => {
                    let reply = dispatch(&lease, request).await;
                    postcard::to_allocvec(&reply)
                },
                Err(error) => postcard::to_allocvec(&WireReply {
                    epoch: namespace.current_epoch(),
                    response: request.error_response(error),
                }),
            };
            match body {
                Ok(body) => {
                    let _ = outbound_tx
                        .send(Frame::new(request_id, KIND_RESPONSE, body))
                        .await;
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
async fn dispatch(lease: &NamespaceLease, request: WireRequest) -> WireReply {
    let epoch = lease.epoch();
    let namespace = lease.namespace();
    let response = match request {
        WireRequest::Lookup { parent, name } => {
            WireResponse::Lookup(lease.run(namespace.lookup(parent, &name)).await)
        },
        WireRequest::Getattr { path } => {
            WireResponse::Getattr(lease.run(namespace.getattr(path)).await)
        },
        WireRequest::GetattrExact { path } => {
            WireResponse::GetattrExact(lease.run(namespace.getattr_exact(path)).await)
        },
        WireRequest::Readdir {
            path,
            cursor,
            budget,
        } => WireResponse::Readdir(
            lease
                .run(namespace.readdir(path, cursor, usize::try_from(budget).unwrap_or(usize::MAX)))
                .await,
        ),
        WireRequest::Read { path, offset, len } => {
            WireResponse::Read(lease.run(namespace.read(path, offset, len)).await)
        },
        WireRequest::Readlink { path } => {
            WireResponse::Readlink(lease.run(namespace.readlink(path)).await)
        },
    };
    WireReply { epoch, response }
}
