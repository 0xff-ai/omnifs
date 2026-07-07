//! The wire server: it adapts a [`Namespace`] onto a byte stream.
//!
//! [`serve_connection`] runs one attached client; [`serve_listener`] accepts
//! clients on a Unix socket and serves each concurrently. A connection dispatches
//! every request onto the namespace on its own task, so one slow op (a provider
//! callout) never head-of-line-blocks the reads behind it, and a background task
//! forwards the namespace's invalidation events as event frames.

use std::sync::Arc;

use omnifs_engine::Namespace;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use crate::frame::{Frame, KIND_EVENT, KIND_REQUEST, KIND_RESPONSE, read_frame, write_frame};
use crate::{Handshake, PROTOCOL, WireError, WireRequest, WireResponse};

/// Serve one attached client over `stream` until it disconnects. `instance_id`
/// is the daemon's per-start id, reported in the handshake so the client can
/// detect a restart on reconnect.
///
/// Returns `Ok(())` on an orderly client disconnect and a [`WireError`] on a
/// protocol fault (an oversized frame, a malformed handshake, a version
/// mismatch); a fault drops the connection.
pub async fn serve_connection<S>(
    namespace: Arc<dyn Namespace>,
    stream: S,
    instance_id: String,
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

    // Handshake: read the client's Hello, validate, answer with Welcome. A
    // version mismatch is a clean error that drops the connection.
    let handshake_result = server_handshake(&mut reader, &outbound_tx, &instance_id).await;
    if let Err(error) = handshake_result {
        writer_task.abort();
        return Err(error);
    }

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
pub async fn serve_listener(
    namespace: Arc<dyn Namespace>,
    listener: UnixListener,
    instance_id: String,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let namespace = Arc::clone(&namespace);
                let instance_id = instance_id.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(namespace, stream, instance_id).await {
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

/// Read the client's `Hello`, check the protocol, and answer with `Welcome`.
async fn server_handshake<R>(
    reader: &mut R,
    outbound_tx: &mpsc::UnboundedSender<Frame>,
    instance_id: &str,
) -> Result<(), WireError>
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
    let Handshake::Hello { protocol } = hello else {
        return Err(WireError::HandshakeUnexpected { expected: "hello" });
    };
    if protocol != PROTOCOL {
        return Err(WireError::VersionMismatch {
            ours: PROTOCOL,
            theirs: protocol,
        });
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
