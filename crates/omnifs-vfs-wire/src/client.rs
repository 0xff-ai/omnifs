//! Client for the Omnifs VFS wire protocol.
//!
//! [`WireNamespace`] implements the engine-owned [`Namespace`] over a socket.
//!
//! One background manager task owns the connection. It multiplexes: each caller
//! request gets a fresh id and a oneshot reply slot; response frames are matched
//! back by id; event frames feed a local broadcast that [`WireNamespace::subscribe`]
//! taps. A disconnect fails every in-flight request with
//! [`NsError::Network`](omnifs_engine::NsError::Network) and reconnects with
//! backoff forever until the [`WireNamespace`] is dropped. A reconnect that lands
//! on a different daemon instance fires an [`AttachEvent::Reattached`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, FutureExt};
use omnifs_engine::{
    Attrs, DirCursor, DirPage, EventStream, Namespace, NodeAnswer, NodeId, NsError, NsEvent,
    ReadAnswer,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::sleep;

use crate::cache::{WINDOW_BYTES, WireCache, window_start};
use crate::frame::{Frame, KIND_EVENT, KIND_REQUEST, KIND_RESPONSE, read_frame, write_frame};
use crate::{AttachEvent, Handshake, PROTOCOL, WireError, WireRequest, WireResponse};

/// Initial-connect deadline for [`WireNamespace::attach`]. A socket that never
/// answers within this window fails the attach with the socket named, rather
/// than hanging the frontend runner forever.
const INITIAL_CONNECT_DEADLINE: Duration = Duration::from_secs(30);
/// First reconnect backoff, doubling up to [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_millis(50);
/// Backoff ceiling for reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(2);
/// Local invalidation-event broadcast capacity. A slow subscriber that lags this
/// far re-syncs on the next event (the engine `EventStream` drops lag errors).
const EVENT_CAPACITY: usize = 1024;
/// Attach-event broadcast capacity. Reattach events are rare; a small ring is
/// plenty.
const ATTACH_CAPACITY: usize = 16;

/// Where a [`WireNamespace`] dials the daemon it attaches to.
///
/// `Unix` is the host-native path: auth is filesystem permissions on the
/// socket, so no token is sent. `Tcp` is the Docker Desktop path: the
/// containerized frontend cannot share a host Unix socket into the Linux VM it
/// runs in, so it dials TCP instead and proves itself with the daemon's
/// per-instance attach token. `addr` is a `host:port` string rather than a
/// pre-resolved `SocketAddr` because the Docker-hosted frontend dials the
/// `host.docker.internal` name Docker injects into the container's DNS, not a
/// literal address the CLI could resolve ahead of time; `TcpStream::connect`
/// resolves it same as any other socket address type.
///
/// `Vsock` is the krunkit-on-macOS path: the guest VM has no shared host Unix
/// socket and no Docker-style loopback either, but krunkit gives it a virtio
/// socket device, so it dials host CID 2 (`VMADDR_CID_HOST`) on `port` instead.
/// krunkit proxies that vsock connection onto a host Unix socket
/// (`--device virtio-vsock,port=N,socketURL=<path>,listen`), and every
/// connection krunkit forwards looks like the same trusted local peer to that
/// socket, so `token` proves the guest's identity the same way it does over
/// TCP. The dial itself only builds on Linux (the guest OS); on any other
/// target it fails at attach time with a named, non-retriable error rather
/// than being a compile-time option.
#[derive(Debug, Clone)]
pub enum AttachTarget {
    Unix(PathBuf),
    Tcp { addr: String, token: String },
    Vsock { port: u32, token: String },
}

impl AttachTarget {
    /// Resolve the explicit `--attach <socket>` when given, otherwise the target
    /// named by `OMNIFS_ATTACH_ADDR`/`OMNIFS_ATTACH_TOKEN`. Neither present is a
    /// hard error: there is no default to fall back to silently.
    pub fn resolve(attach: Option<PathBuf>) -> Result<Self, AttachTargetError> {
        if let Some(socket) = attach {
            return Ok(Self::Unix(socket));
        }
        Self::from_env(
            std::env::var(omnifs_api::OMNIFS_ATTACH_ADDR_ENV).ok(),
            std::env::var(omnifs_api::OMNIFS_ATTACH_TOKEN_ENV).ok(),
        )
    }

    /// Parse the env-driven target from explicit values so validation remains
    /// testable without mutating process environment.
    ///
    /// `addr` is `vsock:<port>` for a krunkit guest or `host:port` for TCP. TCP
    /// targets remain unresolved because `host.docker.internal` exists only in
    /// the frontend container's DNS and cannot be resolved by the host CLI.
    fn from_env(addr: Option<String>, token: Option<String>) -> Result<Self, AttachTargetError> {
        let addr = addr.ok_or(AttachTargetError::Missing {
            env: omnifs_api::OMNIFS_ATTACH_ADDR_ENV,
        })?;
        let token = token.ok_or(AttachTargetError::MissingToken {
            addr_env: omnifs_api::OMNIFS_ATTACH_ADDR_ENV,
            token_env: omnifs_api::OMNIFS_ATTACH_TOKEN_ENV,
        })?;
        if let Some(port) = addr.strip_prefix("vsock:") {
            let port: u32 = port
                .parse()
                .map_err(|source| AttachTargetError::InvalidVsockPort {
                    env: omnifs_api::OMNIFS_ATTACH_ADDR_ENV,
                    addr: addr.clone(),
                    source,
                })?;
            return Ok(Self::Vsock { port, token });
        }
        if addr
            .rsplit_once(':')
            .is_none_or(|(_, port)| port.parse::<u16>().is_err())
        {
            return Err(AttachTargetError::InvalidAddr {
                env: omnifs_api::OMNIFS_ATTACH_ADDR_ENV,
                addr,
            });
        }
        Ok(Self::Tcp { addr, token })
    }

    /// Connect with backoff. With a `deadline`, a transient failure past the
    /// deadline surfaces as [`WireError::ConnectTimeout`]; without one,
    /// transient failures retry forever.
    async fn connect_with_backoff(
        &self,
        deadline: Option<Instant>,
    ) -> Result<(Connection, String), WireError> {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            match self.connect_once().await {
                Ok(value) => return Ok(value),
                Err(error) if !error.is_retriable() => return Err(error),
                Err(error) => {
                    if let Some(deadline) = deadline
                        && Instant::now() >= deadline
                    {
                        let source = match error {
                            WireError::Io(io) => io,
                            other => std::io::Error::other(other.to_string()),
                        };
                        return Err(WireError::ConnectTimeout {
                            target: self.to_string(),
                            source,
                        });
                    }
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                },
            }
        }
    }

    /// Connect once, spawn the reader/writer pumps, and complete the handshake.
    /// Vsock is Linux-only because the krunkit guest is Linux; other targets
    /// fail without entering the reconnect loop.
    async fn connect_once(&self) -> Result<(Connection, String), WireError> {
        match self {
            Self::Unix(path) => {
                let stream = UnixStream::connect(path).await?;
                handshake_over(stream, None).await
            },
            Self::Tcp { addr, token } => {
                let stream = TcpStream::connect(addr.as_str()).await?;
                handshake_over(stream, Some(token.clone())).await
            },
            Self::Vsock { port, token } => {
                #[cfg(target_os = "linux")]
                {
                    let addr = tokio_vsock::VsockAddr::new(tokio_vsock::VMADDR_CID_HOST, *port);
                    let stream = tokio_vsock::VsockStream::connect(addr).await?;
                    handshake_over(stream, Some(token.clone())).await
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = (port, token);
                    Err(WireError::VsockUnsupported)
                }
            },
        }
    }
}

impl std::fmt::Display for AttachTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "{}", path.display()),
            Self::Tcp { addr, .. } => write!(f, "{addr}"),
            Self::Vsock { port, .. } => write!(f, "vsock:{port}"),
        }
    }
}

/// Failure resolving an [`AttachTarget`] from `--attach` or the
/// `OMNIFS_ATTACH_ADDR`/`OMNIFS_ATTACH_TOKEN` env vars, before any connection
/// is attempted.
#[derive(Debug, thiserror::Error)]
pub enum AttachTargetError {
    #[error("neither --attach nor {env} is set; the frontend runner needs one attach target")]
    Missing { env: &'static str },
    #[error("{addr_env} is set but {token_env} is not")]
    MissingToken {
        addr_env: &'static str,
        token_env: &'static str,
    },
    #[error("{env} `{addr}` is not a `host:port` address")]
    InvalidAddr { env: &'static str, addr: String },
    #[error("{env} `{addr}` has an invalid vsock port")]
    InvalidVsockPort {
        env: &'static str,
        addr: String,
        #[source]
        source: std::num::ParseIntError,
    },
}

/// One caller request queued to the manager, with the slot its answer returns on.
struct Outgoing {
    request: WireRequest,
    reply: oneshot::Sender<Result<WireResponse, NsError>>,
}

/// A [`Namespace`] backed by a wire connection to a daemon-served socket.
pub struct WireNamespace {
    outgoing: mpsc::UnboundedSender<Outgoing>,
    events: broadcast::Sender<NsEvent>,
    attach_events: broadcast::Sender<AttachEvent>,
    /// The client-side batching cache: an answer memo and read windows, both
    /// keyed off the engine-decided [`Attrs::ttl`]. Shared with the manager task,
    /// which drops a node's cached state when an invalidation event names it.
    cache: Arc<WireCache>,
    /// The current server instance id, updated by the manager on every
    /// (re)connect. `Arc<Mutex<..>>` because the manager writes it while callers
    /// read it; the crate deps forbid `arc-swap`.
    instance_id: Arc<Mutex<String>>,
    /// Aborts the manager task when the namespace is dropped, ending the
    /// reconnect-forever loop.
    _manager: AbortOnDrop,
}

impl WireNamespace {
    /// Connect to the namespace target, perform the handshake, and return a
    /// namespace multiplexed over the connection. Retries the initial connect
    /// with backoff up to a 30s deadline; a later disconnect reconnects forever.
    ///
    /// # Errors
    ///
    /// Fails when the target cannot be reached within the deadline (naming it),
    /// when the server speaks an incompatible protocol version, or (`Tcp`) when
    /// the attach token is rejected.
    pub async fn attach(target: AttachTarget, rt: Handle) -> Result<Arc<Self>, WireError> {
        let deadline = Instant::now() + INITIAL_CONNECT_DEADLINE;
        let (connection, instance_id) = target.connect_with_backoff(Some(deadline)).await?;

        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel::<Outgoing>();
        let (events_tx, _) = broadcast::channel(EVENT_CAPACITY);
        let (attach_tx, _) = broadcast::channel(ATTACH_CAPACITY);
        let instance_slot = Arc::new(Mutex::new(instance_id.clone()));
        let cache = Arc::new(WireCache::new());

        let manager = rt.spawn(
            ManagerState {
                target,
                connection,
                instance: instance_id,
                instance_slot: Arc::clone(&instance_slot),
                outgoing_rx,
                events: events_tx.clone(),
                attach_events: attach_tx.clone(),
                cache: Arc::clone(&cache),
            }
            .run(),
        );

        Ok(Arc::new(Self {
            outgoing: outgoing_tx,
            events: events_tx,
            attach_events: attach_tx,
            instance_id: instance_slot,
            cache,
            _manager: AbortOnDrop(manager),
        }))
    }

    /// The current server instance id. Changes when a reconnect lands on a
    /// restarted daemon (see [`AttachEvent::Reattached`]).
    #[must_use]
    pub fn instance_id(&self) -> String {
        self.instance_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Subscribe to [`AttachEvent`]s. `Reattached` fires when a reconnect lands
    /// on a different daemon instance than before; a plain reconnect fires
    /// nothing.
    #[must_use]
    pub fn subscribe_attach_events(&self) -> broadcast::Receiver<AttachEvent> {
        self.attach_events.subscribe()
    }

    /// Issue one request and await its answer. A closed manager (the connection
    /// gave up, or the namespace is dropping) surfaces as [`NsError::Network`].
    async fn call(&self, request: WireRequest) -> Result<WireResponse, NsError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.outgoing
            .send(Outgoing {
                request,
                reply: reply_tx,
            })
            .map_err(|_| NsError::Network)?;
        reply_rx.await.map_err(|_| NsError::Network)?
    }

    /// A read that goes straight to the server, bypassing the window cache. Used
    /// for the pass-through paths (a `ttl == 0` node, a large read, or a
    /// concurrent read while a window fetch is already in flight) and for the
    /// window fetch itself.
    async fn read_passthrough(
        &self,
        node: NodeId,
        offset: u64,
        len: u32,
    ) -> Result<ReadAnswer, NsError> {
        match self.call(WireRequest::Read { node, offset, len }).await? {
            WireResponse::Read(answer) => answer,
            _ => Err(variant_mismatch()),
        }
    }
}

/// A [`WireResponse`] whose variant did not match the request it answers. A
/// well-behaved server never produces this; it guards a corrupt peer.
fn variant_mismatch() -> NsError {
    NsError::Internal {
        message: "wire: response variant did not match the request".to_string(),
    }
}

impl Namespace for WireNamespace {
    fn lookup<'a>(
        &'a self,
        parent: NodeId,
        name: &'a str,
    ) -> BoxFuture<'a, Result<NodeAnswer, NsError>> {
        let name = name.to_string();
        async move {
            // A memoized lookup (only ever a ttl>0 answer) serves without a hop.
            if let Some(answer) = self.cache.lookup(parent, &name) {
                return Ok(answer);
            }
            let answer = match self
                .call(WireRequest::Lookup {
                    parent,
                    name: name.clone(),
                })
                .await?
            {
                WireResponse::Lookup(answer) => answer?,
                _ => return Err(variant_mismatch()),
            };
            self.cache.put_lookup(parent, &name, &answer);
            Ok(answer)
        }
        .boxed()
    }

    fn getattr(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        async move {
            if let Some(attrs) = self.cache.attrs(node) {
                return Ok(attrs);
            }
            let attrs = match self.call(WireRequest::Getattr { node }).await? {
                WireResponse::Getattr(answer) => answer?,
                _ => return Err(variant_mismatch()),
            };
            self.cache.put_attrs(node, &attrs);
            Ok(attrs)
        }
        .boxed()
    }

    fn getattr_exact(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        async move {
            // A ttl>0 memo entry already carries the exact, stable size that
            // `getattr_exact` would otherwise probe for, so it serves both.
            if let Some(attrs) = self.cache.attrs(node) {
                return Ok(attrs);
            }
            let attrs = match self.call(WireRequest::GetattrExact { node }).await? {
                WireResponse::GetattrExact(answer) => answer?,
                _ => return Err(variant_mismatch()),
            };
            self.cache.put_attrs(node, &attrs);
            Ok(attrs)
        }
        .boxed()
    }

    fn readdir(
        &self,
        node: NodeId,
        cursor: DirCursor,
        budget: usize,
    ) -> BoxFuture<'_, Result<DirPage, NsError>> {
        async move {
            // Directory pages are never cached (pagination cursors carry resume
            // state), but every ttl>0 child seeds the answer memo so the walk's
            // per-child stat chatter resolves locally.
            let page = match self
                .call(WireRequest::Readdir {
                    node,
                    cursor,
                    budget: budget as u64,
                })
                .await?
            {
                WireResponse::Readdir(answer) => answer?,
                _ => return Err(variant_mismatch()),
            };
            self.cache.seed_dir_entries(node, &page.entries);
            Ok(page)
        }
        .boxed()
    }

    fn read(
        &self,
        node: NodeId,
        offset: u64,
        len: u32,
    ) -> BoxFuture<'_, Result<ReadAnswer, NsError>> {
        async move {
            // Only a small read on a stable, exact-size (ttl>0) node windows;
            // everything else passes through byte-for-byte identical.
            let Some(size) = self.cache.known_size(node) else {
                return self.read_passthrough(node, offset, len).await;
            };
            if u64::from(len) >= WINDOW_BYTES {
                return self.read_passthrough(node, offset, len).await;
            }
            if let Some(answer) = self.cache.window_slice(node, offset, len, size) {
                return Ok(answer);
            }
            // Miss: claim the sole window fetch for this node, or, if one is
            // already outstanding, read straight through to avoid a duplicate.
            if !self.cache.try_begin_window(node) {
                return self.read_passthrough(node, offset, len).await;
            }
            // A concurrent fetch may have filled the window between the miss and
            // the claim; if so, release the claim and serve it.
            if let Some(answer) = self.cache.window_slice(node, offset, len, size) {
                self.cache.abort_window(node);
                return Ok(answer);
            }
            let start = window_start(offset);
            let want = u64::from(len)
                .max(WINDOW_BYTES)
                .min(size.saturating_sub(start));
            let win_len = u32::try_from(want).unwrap_or(u32::MAX);
            match self.read_passthrough(node, start, win_len).await {
                Ok(window) => Ok(self
                    .cache
                    .finish_window(node, start, window, offset, len, size)),
                Err(error) => {
                    self.cache.abort_window(node);
                    Err(error)
                },
            }
        }
        .boxed()
    }

    fn readlink(&self, node: NodeId) -> BoxFuture<'_, Result<PathBuf, NsError>> {
        async move {
            match self.call(WireRequest::Readlink { node }).await? {
                WireResponse::Readlink(answer) => answer,
                _ => Err(variant_mismatch()),
            }
        }
        .boxed()
    }

    fn subscribe(&self) -> EventStream {
        EventStream::from_broadcast(self.events.subscribe())
    }
}

// ---------------------------------------------------------------------------
// The connection manager
// ---------------------------------------------------------------------------

/// The manager's owned connection and cache state.
struct ManagerState {
    target: AttachTarget,
    connection: Connection,
    instance: String,
    instance_slot: Arc<Mutex<String>>,
    outgoing_rx: mpsc::UnboundedReceiver<Outgoing>,
    events: broadcast::Sender<NsEvent>,
    attach_events: broadcast::Sender<AttachEvent>,
    cache: Arc<WireCache>,
}

impl ManagerState {
    /// Assign request ids, track pending replies, decode inbound frames, and
    /// reconnect after disconnects.
    async fn run(mut self) {
        let mut pending: HashMap<u64, oneshot::Sender<Result<WireResponse, NsError>>> =
            HashMap::new();
        let mut next_id: u64 = 1;

        loop {
            tokio::select! {
                // Inbound frames win over new requests so a disconnect is handled
                // before another request is queued onto a dead connection.
                biased;

                frame = self.connection.frame_rx.recv() => {
                    if let Some(frame) = frame {
                        self.handle_inbound(&frame, &mut pending);
                    } else {
                        // The connection died: fail every in-flight request, then
                        // reconnect forever (aborted only by dropping the namespace).
                        for (_, reply) in pending.drain() {
                            let _ = reply.send(Err(NsError::Network));
                        }
                        match self.target.connect_with_backoff(None).await {
                            Ok((connection, new_instance)) => {
                                if new_instance != self.instance {
                                    // A restarted daemon renumbered every NodeId, so
                                    // every memoized answer is stale; drop the cache
                                    // before serving answers from the new instance.
                                    self.cache.clear();
                                    let _ = self.attach_events.send(AttachEvent::Reattached {
                                        old_instance: self.instance.clone(),
                                        new_instance: new_instance.clone(),
                                    });
                                }
                                self.instance.clone_from(&new_instance);
                                *self
                                    .instance_slot
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) = new_instance;
                                self.connection = connection;
                            },
                            Err(error) => {
                                tracing::warn!(%error, "wire: gave up reconnecting; namespace is offline");
                                return;
                            },
                        }
                    }
                }

                outgoing = self.outgoing_rx.recv() => {
                    let Some(Outgoing { request, reply }) = outgoing else {
                        // The namespace was dropped: no more callers, stop.
                        return;
                    };
                    let id = next_id;
                    next_id = next_id.checked_add(1).unwrap_or(1);
                    match postcard::to_allocvec(&request) {
                        Ok(body) => {
                            pending.insert(id, reply);
                            if self
                                .connection
                                .frame_tx
                                .send(Frame::new(id, KIND_REQUEST, body))
                                .is_err()
                                && let Some(reply) = pending.remove(&id)
                            {
                                // The writer is gone; the frame_rx `None` branch will
                                // reconnect. Fail this request now.
                                let _ = reply.send(Err(NsError::Network));
                            }
                        },
                        Err(error) => {
                            let _ = reply.send(Err(NsError::Internal {
                                message: format!("wire: request encode failed: {error}"),
                            }));
                        },
                    }
                }
            }
        }
    }

    /// Route a response to its caller or apply and re-broadcast an event.
    fn handle_inbound(
        &self,
        frame: &Frame,
        pending: &mut HashMap<u64, oneshot::Sender<Result<WireResponse, NsError>>>,
    ) {
        match frame.kind {
            KIND_RESPONSE => {
                if let Some(reply) = pending.remove(&frame.request_id) {
                    let answer =
                        postcard::from_bytes::<WireResponse>(&frame.body).map_err(|error| {
                            NsError::Internal {
                                message: format!("wire: decode response failed: {error}"),
                            }
                        });
                    let _ = reply.send(answer);
                }
            },
            KIND_EVENT => {
                if let Ok(event) = postcard::from_bytes::<NsEvent>(&frame.body) {
                    self.cache.apply_event(&event);
                    let _ = self.events.send(event);
                }
            },
            other => {
                tracing::debug!(kind = other, "wire: ignoring an unknown inbound frame kind");
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Connection establishment
// ---------------------------------------------------------------------------

/// A live connection: the frame channels plus the reader/writer tasks that pump
/// them. Dropping it aborts both tasks.
struct Connection {
    frame_tx: mpsc::UnboundedSender<Frame>,
    frame_rx: mpsc::UnboundedReceiver<Frame>,
    reader: tokio::task::JoinHandle<()>,
    writer: tokio::task::JoinHandle<()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

/// Spawn the reader/writer pumps over `stream` and complete the handshake,
/// sending `token` in the Hello (`None` for a Unix socket, `Some` for TCP).
/// Generic over the stream type so both transports share one handshake path.
async fn handshake_over<S>(
    stream: S,
    token: Option<String>,
) -> Result<(Connection, String), WireError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut read_half, mut write_half) = tokio::io::split(stream);

    let (frame_tx, mut writer_rx) = mpsc::unbounded_channel::<Frame>();
    let (reader_tx, mut frame_rx) = mpsc::unbounded_channel::<Frame>();

    let writer = tokio::spawn(async move {
        while let Some(frame) = writer_rx.recv().await {
            if write_frame(&mut write_half, &frame).await.is_err() {
                break;
            }
        }
    });
    let reader = tokio::spawn(async move {
        while let Ok(Some(frame)) = read_frame(&mut read_half).await {
            if reader_tx.send(frame).is_err() {
                break;
            }
        }
    });

    // Handshake: send Hello, expect Welcome or Rejected as the first inbound
    // frame.
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol: PROTOCOL,
        token,
    })?;
    frame_tx
        .send(Frame::new(0, KIND_REQUEST, hello))
        .map_err(|_| WireError::HandshakeClosed)?;
    let welcome_frame = frame_rx.recv().await.ok_or(WireError::HandshakeClosed)?;
    let welcome: Handshake = postcard::from_bytes(&welcome_frame.body)?;
    match welcome {
        Handshake::Welcome {
            protocol,
            instance_id,
        } => {
            if protocol != PROTOCOL {
                reader.abort();
                writer.abort();
                return Err(WireError::VersionMismatch {
                    ours: PROTOCOL,
                    theirs: protocol,
                });
            }
            Ok((
                Connection {
                    frame_tx,
                    frame_rx,
                    reader,
                    writer,
                },
                instance_id,
            ))
        },
        Handshake::Rejected { reason } => {
            reader.abort();
            writer.abort();
            Err(WireError::Rejected(reason))
        },
        Handshake::Hello { .. } => {
            reader.abort();
            writer.abort();
            Err(WireError::HandshakeUnexpected {
                expected: "welcome",
            })
        },
    }
}

impl WireError {
    /// Whether retrying the connect can plausibly succeed. A refused socket or a
    /// mid-handshake close is transient; a version mismatch, a rejected token,
    /// or a decode fault is not (the server is up but refuses this client).
    fn is_retriable(&self) -> bool {
        matches!(self, WireError::Io(_) | WireError::HandshakeClosed)
    }
}

/// Aborts the wrapped task on drop, so a dropped [`WireNamespace`] ends its
/// reconnect-forever manager.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod attach_target_tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn attach_prefers_explicit_unix_socket() {
        let target = AttachTarget::resolve(Some(PathBuf::from("/tmp/x.sock"))).unwrap();
        assert!(matches!(target, AttachTarget::Unix(path) if path == Path::new("/tmp/x.sock")));
    }

    #[test]
    fn attach_falls_back_to_tcp_env_vars() {
        let target = AttachTarget::from_env(
            Some("host.docker.internal:54321".to_string()),
            Some("secret".to_string()),
        )
        .unwrap();
        match target {
            AttachTarget::Tcp { addr, token } => {
                assert_eq!(addr, "host.docker.internal:54321");
                assert_eq!(token, "secret");
            },
            other => panic!("expected a tcp target, got {other:?}"),
        }
    }

    #[test]
    fn attach_env_requires_both_addr_and_token() {
        AttachTarget::from_env(None, None).expect_err("neither var set must fail");
        AttachTarget::from_env(Some("host.docker.internal:1".to_string()), None)
            .expect_err("addr without token must fail");
        AttachTarget::from_env(None, Some("secret".to_string()))
            .expect_err("token without addr must fail");
    }

    #[test]
    fn attach_env_rejects_a_portless_address() {
        AttachTarget::from_env(
            Some("host.docker.internal".to_string()),
            Some("secret".to_string()),
        )
        .expect_err("an address with no port must fail");
    }

    #[test]
    fn attach_falls_back_to_vsock_env_vars() {
        let target =
            AttachTarget::from_env(Some("vsock:9000".to_string()), Some("secret".to_string()))
                .unwrap();
        match target {
            AttachTarget::Vsock { port, token } => {
                assert_eq!(port, 9000);
                assert_eq!(token, "secret");
            },
            other => panic!("expected a vsock target, got {other:?}"),
        }
    }

    #[test]
    fn attach_env_rejects_vsock_with_no_port() {
        AttachTarget::from_env(Some("vsock:".to_string()), Some("secret".to_string()))
            .expect_err("a vsock address with no port must fail");
    }

    #[test]
    fn attach_env_rejects_vsock_with_a_bad_port() {
        AttachTarget::from_env(
            Some("vsock:not-a-port".to_string()),
            Some("secret".to_string()),
        )
        .expect_err("a non-numeric vsock port must fail");
        AttachTarget::from_env(
            Some("vsock:99999999999".to_string()),
            Some("secret".to_string()),
        )
        .expect_err("a vsock port that overflows u32 must fail");
    }

    #[test]
    fn attach_vsock_takes_precedence_over_a_host_literally_named_vsock() {
        // `vsock:8080` is ambiguous between "a host named vsock on port 8080"
        // and the vsock transport; the grammar resolves it to vsock, since
        // there is no other way to address the vsock transport at all, while a
        // host named `vsock` is a name a caller could always change.
        let target =
            AttachTarget::from_env(Some("vsock:8080".to_string()), Some("secret".to_string()))
                .unwrap();
        assert!(matches!(target, AttachTarget::Vsock { port: 8080, .. }));
    }
}
