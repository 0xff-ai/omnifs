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

use omnifs_api::{FrontendDelivery, FrontendInfo, FsType};
use omnifs_engine::Namespace;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;

use crate::frame::{Frame, KIND_EVENT, KIND_REQUEST, KIND_RESPONSE, read_frame, write_frame};
use crate::{FrontendIdentity, Handshake, PROTOCOL, WireError, WireRequest, WireResponse};

const ATTACH_TOKEN_BYTES: usize = 16;
const UDS_PATH_BYTE_LIMIT: usize = 100;

/// The listener path determines both delivery authority and authentication.
/// The guest identity in the handshake remains display-only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ListenerTarget {
    /// The fixed host-native Unix listener authenticated by filesystem mode.
    Local { path: PathBuf },
    /// The Docker delivery listener and its token-authenticated address.
    Tcp { addr: SocketAddr, token: String },
    /// The krunkit vsock-proxy listener and its token-authenticated socket.
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
}

#[derive(Debug, Clone)]
struct AttachedFrontend {
    kind: crate::FrontendKind,
    mount_point: PathBuf,
    delivery: FrontendDelivery,
}

impl AttachedFrontend {
    fn key(&self) -> AttachmentKey {
        AttachmentKey {
            delivery: match self.delivery {
                FrontendDelivery::Local => 0,
                FrontendDelivery::Docker => 1,
                FrontendDelivery::Krunkit => 2,
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
    delivery: u8,
    kind: u8,
    mount_point: PathBuf,
}

struct AttachedEntry {
    frontend: AttachedFrontend,
    connections: usize,
}

struct AttachmentState {
    next_id: u64,
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
                next_id: 1,
                ids: BTreeMap::new(),
                entries: BTreeMap::new(),
            }),
        })
    }

    fn attached(&self, identity: &crate::FrontendIdentity, delivery: FrontendDelivery) -> u64 {
        let frontend = AttachedFrontend {
            kind: identity.kind,
            mount_point: identity.mount_point.clone(),
            delivery,
        };
        let key = frontend.key();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = state.next_id;
        state.next_id += 1;
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
                delivery: entry.frontend.delivery,
            })
            .collect()
    }
}

/// Owns the namespace attach listeners, their connection tasks, attach-token
/// authority, live attachment snapshot, readiness, and shutdown.
pub struct VfsServer {
    namespace: Arc<dyn Namespace>,
    instance_id: String,
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
    pub fn new(namespace: Arc<dyn Namespace>, instance_id: String) -> Arc<Self> {
        let (connection_tx, mut connection_rx) = mpsc::unbounded_channel();
        let (exit_tx, mut exit_rx) = mpsc::unbounded_channel();
        let (event_tx, _) = broadcast::channel(16);
        let server = Arc::new(Self {
            namespace,
            instance_id,
            attachments: Attachments::new(),
            state: Mutex::new(VfsState {
                listeners: BTreeMap::new(),
                ready: false,
                readiness_enabled: false,
                shutting_down: false,
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
                        state.ready = false;
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
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.readiness_enabled = true;
        state.ready = listener_set_ready(&state);
    }

    /// Bind the fixed local UDS before starting its accept task.
    pub fn serve_local(self: &Arc<Self>, path: PathBuf) -> io::Result<ListenerTarget> {
        if let Some(target) = self.existing(ListenerKind::Local) {
            return Ok(target);
        }
        let listener = bind_unix(&path, "local attach socket")?;
        self.install(ListenerTarget::Local { path }, Listener::Unix(listener))
    }

    /// Bind or return the token-authenticated TCP listener for Docker delivery.
    pub fn ensure_tcp(
        self: &Arc<Self>,
        bind_addr: Ipv4Addr,
        port: u16,
        requested_token: Option<String>,
    ) -> io::Result<ListenerTarget> {
        if let Some(target) = self.existing(ListenerKind::Tcp) {
            return Ok(target);
        }
        let std_listener = std::net::TcpListener::bind((bind_addr, port))?;
        std_listener.set_nonblocking(true)?;
        let addr = std_listener.local_addr()?;
        let token = requested_token.map_or_else(generate_attach_token, validate_attach_token)?;
        let listener = TcpListener::from_std(std_listener)?;
        self.install(ListenerTarget::Tcp { addr, token }, Listener::Tcp(listener))
    }

    /// Bind or return the token-authenticated UDS used by the vsock proxy.
    pub fn ensure_vsock(
        self: &Arc<Self>,
        path: PathBuf,
        requested_token: Option<String>,
    ) -> io::Result<ListenerTarget> {
        if let Some(target) = self.existing(ListenerKind::Vsock) {
            return Ok(target);
        }
        let listener = bind_unix(&path, "vsock attach socket")?;
        let token = requested_token.map_or_else(generate_attach_token, validate_attach_token)?;
        self.install(
            ListenerTarget::Vsock {
                socket_path: path,
                token,
            },
            Listener::Unix(listener),
        )
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
            state.ready = false;
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
    ) -> io::Result<ListenerTarget> {
        let kind = target.kind();
        if let Some(existing) = self.existing(kind) {
            return Ok(existing);
        }
        let target_for_task = target.clone();
        let identity = Arc::new(());
        let task_identity = Arc::clone(&identity);
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let namespace = Arc::clone(&self.namespace);
        let instance_id = self.instance_id.clone();
        let attachments = Arc::clone(&self.attachments);
        let connection_tx = self.connection_tx.clone();
        let exit_tx = self.exit_tx.clone();
        let delivery = match kind {
            ListenerKind::Local => FrontendDelivery::Local,
            ListenerKind::Tcp => FrontendDelivery::Docker,
            ListenerKind::Vsock => FrontendDelivery::Krunkit,
        };
        let task = tokio::spawn(async move {
            if start_rx.await.is_err() {
                return;
            }
            accept_loop(
                listener,
                namespace,
                instance_id,
                target_for_task.token().map(str::to_owned),
                delivery,
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
        Ok(target)
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
    instance_id: String,
    token: Option<String>,
    delivery: FrontendDelivery,
    attachments: Arc<Attachments>,
    connection_tx: mpsc::UnboundedSender<Connection>,
) {
    match listener {
        Listener::Unix(listener) => loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let namespace = Arc::clone(&namespace);
                    let instance_id = instance_id.clone();
                    let token = token.clone();
                    let attachments = Arc::clone(&attachments);
                    if connection_tx
                        .send(Box::pin(async move {
                            if let Err(error) = serve_connection_with_registry(
                                namespace,
                                stream,
                                instance_id,
                                token.as_deref(),
                                Some((attachments, delivery)),
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
                    let instance_id = instance_id.clone();
                    let token = token.clone();
                    let attachments = Arc::clone(&attachments);
                    if connection_tx
                        .send(Box::pin(async move {
                            if let Err(error) = serve_connection_with_registry(
                                namespace,
                                stream,
                                instance_id,
                                token.as_deref(),
                                Some((attachments, delivery)),
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
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
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
    if path.exists() {
        match UnixStream::connect(path) {
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
                std::fs::remove_file(path)?
            },
            Err(error) => return Err(error),
        }
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serve one attached client over `stream` until it disconnects. `instance_id`
/// is the daemon's per-start id, reported in the handshake so the client can
/// detect a restart on reconnect. `expected_token` is `None` for a Unix-socket
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
    instance_id: String,
    expected_token: Option<&str>,
) -> Result<(), WireError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    serve_connection_with_registry(namespace, stream, instance_id, expected_token, None).await
}

async fn serve_connection_with_registry<S>(
    namespace: Arc<dyn Namespace>,
    stream: S,
    instance_id: String,
    expected_token: Option<&str>,
    attachment: Option<(Arc<Attachments>, FrontendDelivery)>,
) -> Result<(), WireError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut reader, writer) = tokio::io::split(stream);

    // A single writer task owns the write half; responses (from per-request
    // tasks) and events (from the forwarder) are serialized through its channel,
    // so frames never interleave on the wire.
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Frame>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(frame) = outbound_rx.recv().await {
            if write_frame(&mut writer, &frame).await.is_err() {
                break;
            }
        }
    });

    // Handshake: read the client's Hello, validate, answer with Welcome or
    // Rejected. A version mismatch or a bad token is a clean, named error that
    // drops the connection.
    let handshake_result =
        server_handshake(&mut reader, &outbound_tx, &instance_id, expected_token).await;
    let identity = match handshake_result {
        Ok(identity) => identity,
        Err(error) => {
            // A rejection queues a `Handshake::Rejected` frame on `outbound_tx`
            // before returning; drop the sender and let the writer task drain
            // that frame and exit on its own, rather than aborting it and
            // racing the flush (the same drain-on-drop pattern the end of this
            // function uses).
            drop(outbound_tx);
            let _ = writer_task.await;
            return Err(error);
        },
    };

    let _attach_guard = attachment.map(|(attachments, delivery)| AttachGuard {
        id: attachments.attached(&identity, delivery),
        attachments,
    });

    // Forward namespace invalidation events as event frames for the connection's
    // lifetime. Aborted when the read loop ends.
    let event_task = {
        let namespace = Arc::clone(&namespace);
        let outbound_tx = outbound_tx.clone();
        tokio::spawn(async move {
            let mut events = namespace.subscribe();
            while let Some(event) = events.recv().await {
                match postcard::to_allocvec(&event) {
                    Ok(body) => {
                        if outbound_tx.send(Frame::new(0, KIND_EVENT, body)).is_err() {
                            break;
                        }
                    },
                    Err(error) => {
                        tracing::warn!(%error, "wire: failed to encode namespace event");
                    },
                }
            }
        })
    };

    let read_result = read_loop(&mut reader, &namespace, &outbound_tx).await;

    event_task.abort();
    // Dropping the last outbound sender lets the writer task drain and exit.
    drop(outbound_tx);
    let _ = writer_task.await;
    read_result
}

/// Read the client's `Hello`, check the protocol and (when `expected_token` is
/// set) the token, and answer with `Welcome` or `Rejected`. On success returns
/// the connecting frontend's identity.
async fn server_handshake<R>(
    reader: &mut R,
    outbound_tx: &mpsc::UnboundedSender<Frame>,
    instance_id: &str,
    expected_token: Option<&str>,
) -> Result<FrontendIdentity, WireError>
where
    R: AsyncRead + Unpin,
{
    let frame = read_frame(reader)
        .await?
        .ok_or(WireError::HandshakeClosed)?;
    if frame.kind != KIND_REQUEST {
        return Err(WireError::HandshakeUnexpected { expected: "hello" });
    }
    let hello: Handshake = match postcard::from_bytes(&frame.body) {
        Ok(hello) => hello,
        Err(error) => {
            if let Ok(LegacyHandshake::Hello { protocol, token }) =
                postcard::from_bytes(&frame.body)
            {
                let _ = token;
                let error = WireError::VersionMismatch {
                    ours: PROTOCOL,
                    theirs: protocol,
                };
                send_rejected(outbound_tx, error.to_string());
                return Err(error);
            }
            return Err(error.into());
        },
    };
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
        send_rejected(outbound_tx, error.to_string());
        return Err(error);
    }
    if let Some(expected) = expected_token {
        let presented = token.as_deref().unwrap_or_default();
        if !constant_time_eq::constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            send_rejected(outbound_tx, "attach token rejected".to_string());
            return Err(WireError::TokenRejected);
        }
    }
    let welcome = Handshake::Welcome {
        protocol: PROTOCOL,
        instance_id: instance_id.to_string(),
    };
    let body = postcard::to_allocvec(&welcome)?;
    // The writer task owns the socket; a send failure means it already exited.
    outbound_tx
        .send(Frame::new(0, KIND_RESPONSE, body))
        .map_err(|_| WireError::HandshakeClosed)?;
    Ok(frontend)
}

/// The v2 client Hello, decoded only to return a useful rejection after v3
/// added [`FrontendIdentity`]. It is never accepted as a current handshake.
#[derive(Deserialize)]
enum LegacyHandshake {
    Hello {
        protocol: u32,
        token: Option<String>,
    },
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
fn send_rejected(outbound_tx: &mpsc::UnboundedSender<Frame>, reason: String) {
    if let Ok(body) = postcard::to_allocvec(&Handshake::Rejected { reason }) {
        let _ = outbound_tx.send(Frame::new(0, KIND_RESPONSE, body));
    }
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
        WireRequest::Getattr { node } => WireResponse::Getattr(namespace.getattr(node).await),
        WireRequest::GetattrExact { node } => {
            WireResponse::GetattrExact(namespace.getattr_exact(node).await)
        },
        WireRequest::Readdir {
            node,
            cursor,
            budget,
        } => WireResponse::Readdir(
            namespace
                .readdir(node, cursor, usize::try_from(budget).unwrap_or(usize::MAX))
                .await,
        ),
        WireRequest::Read { node, offset, len } => {
            WireResponse::Read(namespace.read(node, offset, len).await)
        },
        WireRequest::Readlink { node } => WireResponse::Readlink(namespace.readlink(node).await),
    }
}
