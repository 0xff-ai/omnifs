//! Server for the Omnifs VFS wire protocol.
//!
//! It adapts the engine-owned [`Namespace`] onto a byte stream without owning
//! any VFS semantics.
//!
//! [`serve_connection`] runs one attached client; [`serve_listener`] accepts
//! clients on a Unix socket, optionally checking a per-instance attach token
//! same as [`serve_listener_tcp`]'s TCP loopback listener does (`None` for the
//! plain host-native attach socket, whose whole auth is filesystem
//! permissions; `Some` for the krunkit vsock-proxy path, where krunkit
//! terminates every guest vsock dial on the socket as the same local peer, so
//! filesystem permissions alone cannot distinguish callers). Both serve the
//! same namespace concurrently: a connection dispatches every request onto the
//! namespace on its own task, so one slow op (a provider callout) never
//! head-of-line-blocks the reads behind it, and a background task forwards the
//! namespace's invalidation events as event frames.

use std::sync::Arc;

use omnifs_engine::Namespace;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;

use crate::frame::{Frame, KIND_EVENT, KIND_REQUEST, KIND_RESPONSE, read_frame, write_frame};
use crate::{
    AttachObserver, FrontendIdentity, Handshake, PROTOCOL, WireError, WireRequest, WireResponse,
};

/// Serve one attached client over `stream` until it disconnects. `instance_id`
/// is the daemon's per-start id, reported in the handshake so the client can
/// detect a restart on reconnect. `expected_token` is `None` for a Unix-socket
/// listener (the field is ignored) and `Some(token)` for a TCP attach listener,
/// which rejects a Hello whose token does not match. `observer`, when
/// present, is notified once the handshake succeeds and again when this
/// connection ends, for any reason (see [`AttachObserver`]).
///
/// Returns `Ok(())` on an orderly client disconnect and a [`WireError`] on a
/// protocol fault (an oversized frame, a malformed handshake, a version
/// mismatch, a bad token); a fault drops the connection.
pub async fn serve_connection<S>(
    namespace: Arc<dyn Namespace>,
    stream: S,
    instance_id: String,
    expected_token: Option<&str>,
    observer: Option<Arc<dyn AttachObserver>>,
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

    // Registers this connection with the observer (if any) and unregisters it
    // on drop, no matter how this function returns: an orderly disconnect, a
    // protocol fault below, or a panic unwinding through this scope. Held for
    // the rest of the function so it outlives the event forwarder and read
    // loop.
    let _attach_guard = observer.as_ref().map(|observer| AttachGuard {
        id: observer.attached(&identity),
        observer: Arc::clone(observer),
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

/// Accept and serve connections on `listener` until it errors. Each connection
/// is served on its own task, so a stalled client cannot block new attaches.
/// `token`: `None` when filesystem permissions on the socket are this
/// listener's whole auth (every connection's Hello token is ignored, the
/// plain host-native attach socket's shape); `Some` when the connecting peer
/// identity is not trustworthy on its own and every Hello must match it,
/// checked exactly like [`serve_listener_tcp`]'s (the krunkit vsock-proxy
/// path's shape, where krunkit terminates every guest vsock dial on this
/// socket as the same local peer). `observer`, when present, is shared by
/// every connection this listener accepts.
pub async fn serve_listener(
    namespace: Arc<dyn Namespace>,
    listener: UnixListener,
    instance_id: String,
    token: Option<String>,
    observer: Option<Arc<dyn AttachObserver>>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let namespace = Arc::clone(&namespace);
                let instance_id = instance_id.clone();
                let token = token.clone();
                let observer = observer.clone();
                tokio::spawn(async move {
                    if let Err(error) =
                        serve_connection(namespace, stream, instance_id, token.as_deref(), observer)
                            .await
                    {
                        tracing::debug!(%error, "wire: connection ended with a protocol error");
                    }
                });
            },
            Err(error) => {
                tracing::warn!(%error, "wire: accept failed; the attach listener is stopping");
                break;
            },
        }
    }
}

/// Accept and serve connections on a TCP loopback `listener` until it errors,
/// same shape as [`serve_listener`]. This is the Docker Desktop path: a
/// containerized frontend cannot share a host Unix socket into the Linux VM it
/// runs in, so it dials TCP instead and proves itself with `token` (the
/// listener's only auth) in every connection's Hello. `observer`, when
/// present, is shared by every connection this listener accepts.
pub async fn serve_listener_tcp(
    namespace: Arc<dyn Namespace>,
    listener: TcpListener,
    instance_id: String,
    token: String,
    observer: Option<Arc<dyn AttachObserver>>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let namespace = Arc::clone(&namespace);
                let instance_id = instance_id.clone();
                let token = token.clone();
                let observer = observer.clone();
                tokio::spawn(async move {
                    if let Err(error) =
                        serve_connection(namespace, stream, instance_id, Some(&token), observer)
                            .await
                    {
                        tracing::debug!(%error, "wire: tcp connection ended with a protocol error");
                    }
                });
            },
            Err(error) => {
                tracing::warn!(%error, "wire: accept failed; the tcp attach listener is stopping");
                break;
            },
        }
    }
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

/// Fires [`AttachObserver::detached`] exactly once when the connection this
/// guard was constructed for ends, including via an unwind (a panic
/// propagating through [`serve_connection`]), so the registry can never keep
/// an entry alive for a connection that has actually gone away.
struct AttachGuard {
    observer: Arc<dyn AttachObserver>,
    id: u64,
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        self.observer.detached(self.id);
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
        tokio::spawn(async move {
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
