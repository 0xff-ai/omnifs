//! The wire client: [`WireNamespace`] implements [`Namespace`] over a socket.
//!
//! One background manager task owns the connection. It multiplexes: each caller
//! request gets a fresh id and a oneshot reply slot; response frames are matched
//! back by id; event frames feed a local broadcast that [`WireNamespace::subscribe`]
//! taps. A disconnect fails every in-flight request with
//! [`NsError::Network`](omnifs_engine::NsError::Network) and reconnects with
//! backoff forever until the [`WireNamespace`] is dropped. A reconnect that lands
//! on a different daemon instance fires an [`AttachEvent::Reattached`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, FutureExt};
use omnifs_engine::{
    Attrs, DirCursor, DirPage, EventStream, Namespace, NodeAnswer, NodeId, NsError, NsEvent,
    ReadAnswer,
};
use tokio::net::UnixStream;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::sleep;

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
    /// The current server instance id, updated by the manager on every
    /// (re)connect. `Arc<Mutex<..>>` because the manager writes it while callers
    /// read it; the crate deps forbid `arc-swap`.
    instance_id: Arc<Mutex<String>>,
    /// Aborts the manager task when the namespace is dropped, ending the
    /// reconnect-forever loop.
    _manager: AbortOnDrop,
}

impl WireNamespace {
    /// Connect to the namespace socket, perform the handshake, and return a
    /// namespace multiplexed over the connection. Retries the initial connect
    /// with backoff up to a 30s deadline; a later disconnect reconnects forever.
    ///
    /// # Errors
    ///
    /// Fails when the socket cannot be reached within the deadline (naming the
    /// socket), or when the server speaks an incompatible protocol version.
    pub async fn attach(socket: PathBuf, rt: Handle) -> Result<Arc<Self>, WireError> {
        let deadline = Instant::now() + INITIAL_CONNECT_DEADLINE;
        let (connection, instance_id) = connect_with_backoff(&socket, Some(deadline)).await?;

        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel::<Outgoing>();
        let (events_tx, _) = broadcast::channel(EVENT_CAPACITY);
        let (attach_tx, _) = broadcast::channel(ATTACH_CAPACITY);
        let instance_slot = Arc::new(Mutex::new(instance_id.clone()));

        let manager = rt.spawn(manager_loop(ManagerState {
            socket,
            connection,
            instance: instance_id,
            instance_slot: Arc::clone(&instance_slot),
            outgoing_rx,
            events: events_tx.clone(),
            attach_events: attach_tx.clone(),
        }));

        Ok(Arc::new(Self {
            outgoing: outgoing_tx,
            events: events_tx,
            attach_events: attach_tx,
            instance_id: instance_slot,
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
            match self.call(WireRequest::Lookup { parent, name }).await? {
                WireResponse::Lookup(answer) => answer,
                _ => Err(variant_mismatch()),
            }
        }
        .boxed()
    }

    fn getattr(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        async move {
            match self.call(WireRequest::Getattr { node }).await? {
                WireResponse::Getattr(answer) => answer,
                _ => Err(variant_mismatch()),
            }
        }
        .boxed()
    }

    fn getattr_exact(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        async move {
            match self.call(WireRequest::GetattrExact { node }).await? {
                WireResponse::GetattrExact(answer) => answer,
                _ => Err(variant_mismatch()),
            }
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
            match self
                .call(WireRequest::Readdir {
                    node,
                    cursor,
                    budget: budget as u64,
                })
                .await?
            {
                WireResponse::Readdir(answer) => answer,
                _ => Err(variant_mismatch()),
            }
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
            match self.call(WireRequest::Read { node, offset, len }).await? {
                WireResponse::Read(answer) => answer,
                _ => Err(variant_mismatch()),
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

/// The manager's owned state, threaded into [`manager_loop`].
struct ManagerState {
    socket: PathBuf,
    connection: Connection,
    instance: String,
    instance_slot: Arc<Mutex<String>>,
    outgoing_rx: mpsc::UnboundedReceiver<Outgoing>,
    events: broadcast::Sender<NsEvent>,
    attach_events: broadcast::Sender<AttachEvent>,
}

/// The single task that owns the connection: it assigns request ids, tracks
/// pending replies, decodes inbound frames, and reconnects on disconnect.
async fn manager_loop(mut state: ManagerState) {
    let mut pending: HashMap<u64, oneshot::Sender<Result<WireResponse, NsError>>> = HashMap::new();
    let mut next_id: u64 = 1;

    loop {
        tokio::select! {
            // Inbound frames win over new requests so a disconnect is handled
            // before another request is queued onto a dead connection.
            biased;

            frame = state.connection.frame_rx.recv() => {
                if let Some(frame) = frame {
                    handle_inbound(&frame, &mut pending, &state.events);
                } else {
                    // The connection died: fail every in-flight request, then
                    // reconnect forever (aborted only by dropping the namespace).
                    for (_, reply) in pending.drain() {
                        let _ = reply.send(Err(NsError::Network));
                    }
                    match connect_with_backoff(&state.socket, None).await {
                        Ok((connection, new_instance)) => {
                            if new_instance != state.instance {
                                let _ = state.attach_events.send(AttachEvent::Reattached {
                                    old_instance: state.instance.clone(),
                                    new_instance: new_instance.clone(),
                                });
                            }
                            state.instance.clone_from(&new_instance);
                            *state
                                .instance_slot
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = new_instance;
                            state.connection = connection;
                        },
                        Err(error) => {
                            tracing::warn!(%error, "wire: gave up reconnecting; namespace is offline");
                            return;
                        },
                    }
                }
            }

            outgoing = state.outgoing_rx.recv() => {
                let Some(Outgoing { request, reply }) = outgoing else {
                    // The namespace was dropped: no more callers, stop.
                    return;
                };
                let id = next_id;
                next_id = next_id.checked_add(1).unwrap_or(1);
                match postcard::to_allocvec(&request) {
                    Ok(body) => {
                        pending.insert(id, reply);
                        if state
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

/// Route one inbound frame: a response completes its pending caller; an event
/// re-broadcasts locally.
fn handle_inbound(
    frame: &Frame,
    pending: &mut HashMap<u64, oneshot::Sender<Result<WireResponse, NsError>>>,
    events: &broadcast::Sender<NsEvent>,
) {
    match frame.kind {
        KIND_RESPONSE => {
            if let Some(reply) = pending.remove(&frame.request_id) {
                let answer = postcard::from_bytes::<WireResponse>(&frame.body).map_err(|error| {
                    NsError::Internal {
                        message: format!("wire: decode response failed: {error}"),
                    }
                });
                let _ = reply.send(answer);
            }
        },
        KIND_EVENT => {
            if let Ok(event) = postcard::from_bytes::<NsEvent>(&frame.body) {
                let _ = events.send(event);
            }
        },
        other => {
            tracing::debug!(kind = other, "wire: ignoring an unknown inbound frame kind");
        },
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

/// Retry [`connect_once`] with backoff. With a `deadline`, a transient failure
/// past the deadline surfaces as [`WireError::ConnectTimeout`] naming the socket;
/// without one, transient failures retry forever. A non-retriable failure (a
/// version mismatch) returns immediately.
async fn connect_with_backoff(
    socket: &Path,
    deadline: Option<Instant>,
) -> Result<(Connection, String), WireError> {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match connect_once(socket).await {
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
                        socket: socket.to_path_buf(),
                        source,
                    });
                }
                sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            },
        }
    }
}

/// Connect once: open the socket, spawn the reader/writer pumps, and complete
/// the `Hello`/`Welcome` handshake. Returns the connection and the server's
/// instance id.
async fn connect_once(socket: &Path) -> Result<(Connection, String), WireError> {
    let stream = UnixStream::connect(socket).await?;
    let (mut read_half, mut write_half) = stream.into_split();

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

    // Handshake: send Hello, expect Welcome as the first inbound frame.
    let hello = postcard::to_allocvec(&Handshake::Hello { protocol: PROTOCOL })?;
    frame_tx
        .send(Frame::new(0, KIND_REQUEST, hello))
        .map_err(|_| WireError::HandshakeClosed)?;
    let welcome_frame = frame_rx.recv().await.ok_or(WireError::HandshakeClosed)?;
    let welcome: Handshake = postcard::from_bytes(&welcome_frame.body)?;
    let Handshake::Welcome {
        protocol,
        instance_id,
    } = welcome
    else {
        reader.abort();
        writer.abort();
        return Err(WireError::HandshakeUnexpected {
            expected: "welcome",
        });
    };
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
}

impl WireError {
    /// Whether retrying the connect can plausibly succeed. A refused socket or a
    /// mid-handshake close is transient; a version mismatch or a decode fault is
    /// not (the server is up but incompatible).
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
