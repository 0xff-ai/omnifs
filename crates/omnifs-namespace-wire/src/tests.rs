//! Wire round-trip, multiplexing, event, and fault tests.
//!
//! The server-side tests drive [`serve_connection`] over a `tokio::io::duplex`
//! pipe with a frame-level client, so no socket is involved. One end-to-end test
//! runs a real [`WireNamespace`] over a `UnixListener` in a tempdir.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt};
use omnifs_engine::{
    Attrs, DirCursor, DirEntry, DirPage, EventStream, Namespace, NodeAnswer, NodeId, NsEntryKind,
    NsError, NsEvent, ReadAnswer, ReadStyle, StabilityClass,
};
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::broadcast;

use crate::frame::{
    Frame, KIND_EVENT, KIND_REQUEST, KIND_RESPONSE, MAX_FRAME, read_frame, write_frame,
};
use crate::{
    Handshake, PROTOCOL, WireError, WireNamespace, WireRequest, WireResponse, serve_connection,
    serve_listener,
};

// ---------------------------------------------------------------------------
// Stub namespace
// ---------------------------------------------------------------------------

/// A canned [`Namespace`]. `read` sleeps for `offset` milliseconds and echoes the
/// offset back so a caller can prove out-of-order matching; `readlink` always
/// fails, exercising server-side error propagation.
struct StubNamespace {
    events: broadcast::Sender<NsEvent>,
}

impl StubNamespace {
    fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(64);
        Arc::new(Self { events })
    }
}

fn file_attrs(size: u64) -> Attrs {
    Attrs {
        kind: NsEntryKind::File,
        size,
        ttl: Duration::ZERO,
        change: 0,
        direct_io: false,
        stability: StabilityClass::Stable,
        read_style: ReadStyle::Whole,
    }
}

impl Namespace for StubNamespace {
    fn lookup<'a>(
        &'a self,
        _parent: NodeId,
        name: &'a str,
    ) -> BoxFuture<'a, Result<NodeAnswer, NsError>> {
        let name = name.to_string();
        async move {
            Ok(NodeAnswer {
                node: NodeId(if name == "message" { 42 } else { 7 }),
                attrs: file_attrs(13),
                kind: NsEntryKind::File,
            })
        }
        .boxed()
    }

    fn getattr(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        async move { Ok(file_attrs(node.0)) }.boxed()
    }

    fn getattr_exact(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        async move { Ok(file_attrs(node.0 * 2)) }.boxed()
    }

    fn readdir(
        &self,
        _node: NodeId,
        _cursor: DirCursor,
        _budget: usize,
    ) -> BoxFuture<'_, Result<DirPage, NsError>> {
        async move {
            Ok(DirPage {
                entries: vec![DirEntry {
                    name: "child".to_string(),
                    node: NodeId(99),
                    attrs: file_attrs(1),
                    kind: NsEntryKind::File,
                }],
                next: None,
            })
        }
        .boxed()
    }

    fn read(
        &self,
        _node: NodeId,
        offset: u64,
        _len: u32,
    ) -> BoxFuture<'_, Result<ReadAnswer, NsError>> {
        async move {
            // The offset doubles as a per-request delay so responses complete out
            // of request order; echo it so the caller can verify id matching.
            tokio::time::sleep(Duration::from_millis(offset)).await;
            Ok(ReadAnswer {
                bytes: offset.to_le_bytes().to_vec(),
                eof: true,
                attrs: file_attrs(8),
            })
        }
        .boxed()
    }

    fn readlink(&self, _node: NodeId) -> BoxFuture<'_, Result<PathBuf, NsError>> {
        async move { Err(NsError::Invalid) }.boxed()
    }

    fn subscribe(&self) -> EventStream {
        EventStream::from_broadcast(self.events.subscribe())
    }
}

// ---------------------------------------------------------------------------
// Frame-level client helpers over a duplex
// ---------------------------------------------------------------------------

/// Perform the client side of the handshake, returning the server's instance id.
async fn client_handshake(io: &mut DuplexStream, protocol: u32) -> Result<String, WireError> {
    let hello = postcard::to_allocvec(&Handshake::Hello { protocol }).unwrap();
    write_frame(io, &Frame::new(0, KIND_REQUEST, hello)).await?;
    let welcome = read_frame(io).await?.expect("welcome frame");
    match postcard::from_bytes::<Handshake>(&welcome.body).unwrap() {
        Handshake::Welcome { instance_id, .. } => Ok(instance_id),
        Handshake::Hello { .. } => panic!("server sent a hello"),
    }
}

async fn send_request(io: &mut DuplexStream, request_id: u64, request: &WireRequest) {
    let body = postcard::to_allocvec(request).unwrap();
    write_frame(io, &Frame::new(request_id, KIND_REQUEST, body))
        .await
        .expect("send request");
}

async fn recv_response(io: &mut DuplexStream) -> (u64, WireResponse) {
    let frame = read_frame(io).await.expect("read").expect("frame");
    assert_eq!(frame.kind, KIND_RESPONSE, "expected a response frame");
    (frame.request_id, postcard::from_bytes(&frame.body).unwrap())
}

/// Spawn a server over the server half of a fresh duplex; return the client half
/// and the server's join handle.
fn serve_over_duplex(
    namespace: Arc<dyn Namespace>,
) -> (DuplexStream, tokio::task::JoinHandle<Result<(), WireError>>) {
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    let handle = tokio::spawn(serve_connection(
        namespace,
        server_io,
        "inst-server".to_string(),
    ));
    (client_io, handle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn round_trips_every_request_variant() {
    let stub = StubNamespace::new();
    let (mut io, _server) = serve_over_duplex(stub);
    let instance = client_handshake(&mut io, PROTOCOL).await.unwrap();
    assert_eq!(instance, "inst-server");

    send_request(
        &mut io,
        1,
        &WireRequest::Lookup {
            parent: NodeId::ROOT,
            name: "message".to_string(),
        },
    )
    .await;
    let (id, resp) = recv_response(&mut io).await;
    assert_eq!(id, 1);
    match resp {
        WireResponse::Lookup(Ok(answer)) => assert_eq!(answer.node, NodeId(42)),
        other => panic!("unexpected {other:?}"),
    }

    send_request(&mut io, 2, &WireRequest::Getattr { node: NodeId(5) }).await;
    match recv_response(&mut io).await {
        (2, WireResponse::Getattr(Ok(attrs))) => assert_eq!(attrs.size, 5),
        other => panic!("unexpected {other:?}"),
    }

    send_request(&mut io, 3, &WireRequest::GetattrExact { node: NodeId(5) }).await;
    match recv_response(&mut io).await {
        (3, WireResponse::GetattrExact(Ok(attrs))) => assert_eq!(attrs.size, 10),
        other => panic!("unexpected {other:?}"),
    }

    send_request(
        &mut io,
        4,
        &WireRequest::Readdir {
            node: NodeId::ROOT,
            cursor: DirCursor::start(),
            budget: 0,
        },
    )
    .await;
    match recv_response(&mut io).await {
        (4, WireResponse::Readdir(Ok(page))) => {
            assert_eq!(page.entries.len(), 1);
            assert_eq!(page.entries[0].name, "child");
        },
        other => panic!("unexpected {other:?}"),
    }

    send_request(
        &mut io,
        5,
        &WireRequest::Read {
            node: NodeId(1),
            offset: 0,
            len: 8,
        },
    )
    .await;
    match recv_response(&mut io).await {
        (5, WireResponse::Read(Ok(answer))) => assert!(answer.eof),
        other => panic!("unexpected {other:?}"),
    }

    send_request(&mut io, 6, &WireRequest::Readlink { node: NodeId(1) }).await;
    match recv_response(&mut io).await {
        (6, WireResponse::Readlink(Err(NsError::Invalid))) => {},
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_requests_answered_out_of_order() {
    let stub = StubNamespace::new();
    let (mut io, _server) = serve_over_duplex(stub);
    client_handshake(&mut io, PROTOCOL).await.unwrap();

    // Request ids 1,2,3 with delays 150,80,20 ms: completions arrive 3,2,1.
    let plan = [(1_u64, 150_u64), (2, 80), (3, 20)];
    for (id, offset) in plan {
        send_request(
            &mut io,
            id,
            &WireRequest::Read {
                node: NodeId(1),
                offset,
                len: 8,
            },
        )
        .await;
    }

    let mut seen = std::collections::HashMap::new();
    for _ in 0..plan.len() {
        let (id, resp) = recv_response(&mut io).await;
        match resp {
            WireResponse::Read(Ok(answer)) => {
                let echoed = u64::from_le_bytes(answer.bytes.try_into().unwrap());
                seen.insert(id, echoed);
            },
            other => panic!("unexpected {other:?}"),
        }
    }
    // Every request's echoed offset matches the one issued under its id.
    for (id, offset) in plan {
        assert_eq!(seen.get(&id), Some(&offset), "id {id} mismatched");
    }
}

#[tokio::test]
async fn server_pushes_events() {
    let stub = StubNamespace::new();
    let events = stub.events.clone();
    let (mut io, _server) = serve_over_duplex(stub);
    client_handshake(&mut io, PROTOCOL).await.unwrap();

    // The event forwarder subscribes right after the handshake; wait for it, then
    // push one event and read it off the wire.
    while events.receiver_count() == 0 {
        tokio::task::yield_now().await;
    }
    let pushed = NsEvent::InvalidateSubtree {
        node: NodeId(77),
        epoch: omnifs_engine::Epoch(9),
    };
    events.send(pushed.clone()).unwrap();

    let frame = read_frame(&mut io).await.unwrap().expect("event frame");
    assert_eq!(frame.kind, KIND_EVENT);
    assert_eq!(frame.request_id, 0);
    let event: NsEvent = postcard::from_bytes(&frame.body).unwrap();
    assert_eq!(event, pushed);
}

#[tokio::test]
async fn oversized_frame_is_rejected() {
    let stub = StubNamespace::new();
    let (mut io, server) = serve_over_duplex(stub);
    client_handshake(&mut io, PROTOCOL).await.unwrap();

    // Write only an oversized length header; the server rejects before reading a
    // body and drops the connection.
    io.write_u32_le(MAX_FRAME + 1).await.unwrap();
    io.flush().await.unwrap();

    match server.await.unwrap() {
        Err(WireError::FrameTooLarge { len }) => assert_eq!(len, MAX_FRAME + 1),
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn handshake_version_mismatch_is_rejected() {
    let stub = StubNamespace::new();
    let (mut io, server) = serve_over_duplex(stub);
    // The client offers a version the server does not speak.
    let hello = postcard::to_allocvec(&Handshake::Hello { protocol: 999 }).unwrap();
    write_frame(&mut io, &Frame::new(0, KIND_REQUEST, hello))
        .await
        .unwrap();

    match server.await.unwrap() {
        Err(WireError::VersionMismatch { ours, theirs }) => {
            assert_eq!(ours, PROTOCOL);
            assert_eq!(theirs, 999);
        },
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn server_side_nserror_propagates() {
    let stub = StubNamespace::new();
    let (mut io, _server) = serve_over_duplex(stub);
    client_handshake(&mut io, PROTOCOL).await.unwrap();

    send_request(&mut io, 1, &WireRequest::Readlink { node: NodeId(1) }).await;
    match recv_response(&mut io).await {
        (1, WireResponse::Readlink(Err(NsError::Invalid))) => {},
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[tokio::test]
async fn unix_listener_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let stub = StubNamespace::new();
    tokio::spawn(serve_listener(stub, listener, "inst-e2e".to_string()));

    let namespace = WireNamespace::attach(socket, tokio::runtime::Handle::current())
        .await
        .expect("attach");
    assert_eq!(namespace.instance_id(), "inst-e2e");

    let answer = namespace.lookup(NodeId::ROOT, "message").await.unwrap();
    assert_eq!(answer.node, NodeId(42));

    let attrs = namespace.getattr(NodeId(5)).await.unwrap();
    assert_eq!(attrs.size, 5);

    let read = namespace.read(NodeId(1), 0, 8).await.unwrap();
    assert!(read.eof);

    let err = namespace.readlink(NodeId(1)).await.unwrap_err();
    assert_eq!(err, NsError::Invalid);
}
