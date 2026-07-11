//! Wire round-trip, multiplexing, event, and fault tests.
//!
//! The server-side tests drive [`serve_connection`] over a `tokio::io::duplex`
//! pipe with a frame-level client, so no socket is involved. One end-to-end test
//! runs a real [`WireNamespace`] over a `UnixListener` in a tempdir.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt};
use omnifs_engine::{
    Attrs, DirCursor, DirEntry, DirPage, Epoch, EventStream, Namespace, NodeAnswer, NodeId,
    NsEntryKind, NsError, NsEvent, ReadAnswer, ReadStyle, StabilityClass,
};
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::broadcast;

use crate::frame::{
    Frame, KIND_EVENT, KIND_REQUEST, KIND_RESPONSE, MAX_FRAME, read_frame, write_frame,
};
use crate::{
    AttachObserver, AttachTarget, FrontendIdentity, FrontendKind, Handshake, PROTOCOL, WireError,
    WireNamespace, WireRequest, WireResponse, serve_connection, serve_listener, serve_listener_tcp,
};

/// A canned identity for tests that don't care about the specific value, only
/// that a `Hello` carries one.
fn test_identity() -> FrontendIdentity {
    FrontendIdentity {
        kind: FrontendKind::Fuse,
        mount_point: PathBuf::from("/mnt/test"),
    }
}

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

/// Perform the client side of the handshake, returning the server's instance id
/// on `Welcome` or the rejection reason (as an error string) on `Rejected`.
async fn client_handshake_with_token(
    io: &mut DuplexStream,
    protocol: u32,
    token: Option<String>,
) -> Result<String, String> {
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol,
        token,
        frontend: test_identity(),
    })
    .unwrap();
    write_frame(io, &Frame::new(0, KIND_REQUEST, hello))
        .await
        .map_err(|error| error.to_string())?;
    let welcome = read_frame(io)
        .await
        .map_err(|error| error.to_string())?
        .expect("welcome frame");
    match postcard::from_bytes::<Handshake>(&welcome.body).unwrap() {
        Handshake::Welcome { instance_id, .. } => Ok(instance_id),
        Handshake::Rejected { reason } => Err(reason),
        Handshake::Hello { .. } => panic!("server sent a hello"),
    }
}

/// Perform the client side of the handshake with no token, returning the
/// server's instance id.
async fn client_handshake(io: &mut DuplexStream, protocol: u32) -> Result<String, WireError> {
    client_handshake_with_token(io, protocol, None)
        .await
        .map_err(WireError::Rejected)
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

/// Spawn a server over the server half of a fresh duplex, with no expected
/// token (mirroring a Unix-socket listener); return the client half and the
/// server's join handle.
fn serve_over_duplex(
    namespace: Arc<dyn Namespace>,
) -> (DuplexStream, tokio::task::JoinHandle<Result<(), WireError>>) {
    serve_over_duplex_with_token(namespace, None)
}

/// Like [`serve_over_duplex`], but with an expected attach token (mirroring a
/// TCP attach listener). `expected_token: None` still serves (a Unix-socket
/// listener never checks it), so the same helper covers both transports.
fn serve_over_duplex_with_token(
    namespace: Arc<dyn Namespace>,
    expected_token: Option<&'static str>,
) -> (DuplexStream, tokio::task::JoinHandle<Result<(), WireError>>) {
    serve_over_duplex_with(namespace, expected_token, None)
}

/// Like [`serve_over_duplex_with_token`], additionally taking the
/// [`AttachObserver`] the connection reports through.
fn serve_over_duplex_with(
    namespace: Arc<dyn Namespace>,
    expected_token: Option<&'static str>,
    observer: Option<Arc<dyn AttachObserver>>,
) -> (DuplexStream, tokio::task::JoinHandle<Result<(), WireError>>) {
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    let handle = tokio::spawn(serve_connection(
        namespace,
        server_io,
        "inst-server".to_string(),
        expected_token,
        observer,
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
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol: 999,
        token: None,
        frontend: test_identity(),
    })
    .unwrap();
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

/// A v2 (or lower) client, predating [`crate::FrontendIdentity`] in `Hello`, is
/// rejected outright by a v3 server. The client-visible error names the
/// mismatch instead of leaving an ambiguous closed connection.
#[tokio::test]
async fn old_client_is_rejected_outright() {
    let stub = StubNamespace::new();
    let (mut io, server) = serve_over_duplex(stub);
    assert_eq!(PROTOCOL, 3, "this test assumes the v2-rejection bump to v3");

    let client_result = client_handshake_with_token(&mut io, 2, None).await;
    assert_eq!(
        client_result,
        Err("protocol version mismatch: this build speaks 3, the peer speaks 2".to_string())
    );

    match server.await.unwrap() {
        Err(WireError::VersionMismatch { ours: 3, theirs: 2 }) => {},
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn tcp_style_token_is_accepted_when_it_matches() {
    let stub = StubNamespace::new();
    let (mut io, _server) = serve_over_duplex_with_token(stub, Some("right-token"));

    let instance = client_handshake_with_token(&mut io, PROTOCOL, Some("right-token".to_string()))
        .await
        .unwrap();
    assert_eq!(instance, "inst-server");
}

#[tokio::test]
async fn tcp_style_wrong_token_is_rejected() {
    let stub = StubNamespace::new();
    let (mut io, server) = serve_over_duplex_with_token(stub, Some("right-token"));

    let client_result =
        client_handshake_with_token(&mut io, PROTOCOL, Some("wrong-token".to_string())).await;
    assert_eq!(client_result, Err("attach token rejected".to_string()));

    match server.await.unwrap() {
        Err(WireError::TokenRejected) => {},
        other => panic!("expected TokenRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn tcp_style_missing_token_is_rejected() {
    let stub = StubNamespace::new();
    let (mut io, server) = serve_over_duplex_with_token(stub, Some("right-token"));

    let client_result = client_handshake_with_token(&mut io, PROTOCOL, None).await;
    assert_eq!(client_result, Err("attach token rejected".to_string()));

    match server.await.unwrap() {
        Err(WireError::TokenRejected) => {},
        other => panic!("expected TokenRejected, got {other:?}"),
    }
}

/// A Unix-socket listener (`expected_token: None`) ignores whatever the client
/// sends in `token`, matching or not.
#[tokio::test]
async fn unix_style_listener_ignores_any_token() {
    let stub = StubNamespace::new();
    let (mut io, _server) = serve_over_duplex(stub);

    let instance = client_handshake_with_token(&mut io, PROTOCOL, Some("whatever".to_string()))
        .await
        .unwrap();
    assert_eq!(instance, "inst-server");
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
    tokio::spawn(serve_listener(
        stub,
        listener,
        "inst-e2e".to_string(),
        None,
        None,
    ));

    let namespace = WireNamespace::attach(
        AttachTarget::Unix(socket),
        test_identity(),
        tokio::runtime::Handle::current(),
    )
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

/// The Docker Desktop path end to end: a real TCP loopback listener, a real
/// [`WireNamespace`] dialing it with the matching attach token.
#[tokio::test]
async fn tcp_listener_end_to_end() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stub = StubNamespace::new();
    tokio::spawn(serve_listener_tcp(
        stub,
        listener,
        "inst-tcp-e2e".to_string(),
        "secret-token".to_string(),
        None,
    ));

    let namespace = WireNamespace::attach(
        AttachTarget::Tcp {
            addr: addr.to_string(),
            token: "secret-token".to_string(),
        },
        test_identity(),
        tokio::runtime::Handle::current(),
    )
    .await
    .expect("attach");
    assert_eq!(namespace.instance_id(), "inst-tcp-e2e");

    let answer = namespace.lookup(NodeId::ROOT, "message").await.unwrap();
    assert_eq!(answer.node, NodeId(42));
}

#[tokio::test]
async fn tcp_listener_rejects_wrong_token() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stub = StubNamespace::new();
    tokio::spawn(serve_listener_tcp(
        stub,
        listener,
        "inst-tcp-reject".to_string(),
        "secret-token".to_string(),
        None,
    ));

    let result = WireNamespace::attach(
        AttachTarget::Tcp {
            addr: addr.to_string(),
            token: "wrong-token".to_string(),
        },
        test_identity(),
        tokio::runtime::Handle::current(),
    )
    .await;
    match result {
        Err(WireError::Rejected(_)) => {},
        Ok(_) => panic!("a wrong token must be rejected, not accepted"),
        Err(other) => panic!("expected Rejected, got {other:?}"),
    }
}

/// The krunkit vsock-proxy path's host-side shape: a real `UnixListener` served
/// by [`serve_listener`] with `Some(token)`, so a connecting peer must present
/// it exactly like [`serve_listener_tcp`]'s TCP listener does. Driven with the
/// raw frame helpers (not `WireNamespace::attach`/`AttachTarget::Unix`, which
/// by design never sends a token) since production reaches this socket through
/// krunkit's vsock proxy, not a bare Unix dial.
#[tokio::test]
async fn unix_listener_with_token_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let stub = StubNamespace::new();
    tokio::spawn(serve_listener(
        stub,
        listener,
        "inst-uds-token".to_string(),
        Some("secret-token".to_string()),
        None,
    ));

    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol: PROTOCOL,
        token: Some("secret-token".to_string()),
        frontend: test_identity(),
    })
    .unwrap();
    write_frame(&mut stream, &Frame::new(0, KIND_REQUEST, hello))
        .await
        .unwrap();
    let welcome = read_frame(&mut stream)
        .await
        .unwrap()
        .expect("welcome frame");
    match postcard::from_bytes::<Handshake>(&welcome.body).unwrap() {
        Handshake::Welcome { instance_id, .. } => assert_eq!(instance_id, "inst-uds-token"),
        other => panic!("expected Welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn unix_listener_with_token_rejects_wrong_token() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let stub = StubNamespace::new();
    tokio::spawn(serve_listener(
        stub,
        listener,
        "inst-uds-reject".to_string(),
        Some("secret-token".to_string()),
        None,
    ));

    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol: PROTOCOL,
        token: Some("wrong-token".to_string()),
        frontend: test_identity(),
    })
    .unwrap();
    write_frame(&mut stream, &Frame::new(0, KIND_REQUEST, hello))
        .await
        .unwrap();
    let response = read_frame(&mut stream)
        .await
        .unwrap()
        .expect("response frame");
    match postcard::from_bytes::<Handshake>(&response.body).unwrap() {
        Handshake::Rejected { .. } => {},
        other => panic!("expected Rejected, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Attach lifecycle observer
// ---------------------------------------------------------------------------

/// A counting [`AttachObserver`]: records every identity `attached` reported
/// and every id `detached` reported, so a test can assert on both halves of
/// the lifecycle independently.
#[derive(Default)]
struct RecordingObserver {
    next_id: AtomicU64,
    attached: Mutex<Vec<(u64, FrontendIdentity)>>,
    detached: Mutex<Vec<u64>>,
}

impl RecordingObserver {
    fn attached_count(&self) -> usize {
        self.attached.lock().unwrap().len()
    }

    fn detached_count(&self) -> usize {
        self.detached.lock().unwrap().len()
    }
}

impl AttachObserver for RecordingObserver {
    fn attached(&self, identity: &FrontendIdentity) -> u64 {
        // Ids start at 1 so a test can distinguish "never attached" (0) from a
        // real assigned id.
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.attached.lock().unwrap().push((id, identity.clone()));
        id
    }

    fn detached(&self, id: u64) {
        self.detached.lock().unwrap().push(id);
    }
}

/// A successful v3 handshake reports the connecting frontend's identity to the
/// server's [`AttachObserver`] verbatim, and an orderly disconnect reports the
/// matching `detached(id)`.
#[tokio::test]
async fn handshake_identity_reaches_the_attach_observer() {
    let stub = StubNamespace::new();
    let observer = Arc::new(RecordingObserver::default());
    let (mut io, server) = serve_over_duplex_with(
        stub,
        None,
        Some(Arc::clone(&observer) as Arc<dyn AttachObserver>),
    );

    let identity = FrontendIdentity {
        kind: FrontendKind::Nfs,
        mount_point: PathBuf::from("/guest/omnifs"),
    };
    let hello = postcard::to_allocvec(&Handshake::Hello {
        protocol: PROTOCOL,
        token: None,
        frontend: identity.clone(),
    })
    .unwrap();
    write_frame(&mut io, &Frame::new(0, KIND_REQUEST, hello))
        .await
        .unwrap();
    let welcome = read_frame(&mut io).await.unwrap().expect("welcome frame");
    match postcard::from_bytes::<Handshake>(&welcome.body).unwrap() {
        Handshake::Welcome { .. } => {},
        other => panic!("expected Welcome, got {other:?}"),
    }

    assert_eq!(observer.attached_count(), 1);
    let (id, reported) = observer.attached.lock().unwrap()[0].clone();
    assert_eq!(reported, identity);
    assert_eq!(
        observer.detached_count(),
        0,
        "not detached before disconnect"
    );

    // An orderly disconnect: drop the client half, closing the pipe.
    drop(io);
    server.await.unwrap().unwrap();

    assert_eq!(observer.detached_count(), 1);
    assert_eq!(observer.detached.lock().unwrap()[0], id);
}

/// The [`AttachObserver::detached`] drop guard fires even when the serve task
/// is torn down abnormally (aborted mid-flight, standing in for a panic
/// unwinding through [`serve_connection`]) rather than returning normally, so
/// the registry can never leak an entry for a connection that is actually
/// gone.
#[tokio::test]
async fn detach_fires_via_drop_guard_on_abnormal_termination() {
    let stub = StubNamespace::new();
    let observer = Arc::new(RecordingObserver::default());
    let (mut io, server) = serve_over_duplex_with(
        stub,
        None,
        Some(Arc::clone(&observer) as Arc<dyn AttachObserver>),
    );

    client_handshake(&mut io, PROTOCOL).await.unwrap();
    // Wait for the observer to see the attach before aborting; `client_handshake`
    // only proves the client saw `Welcome`, which the server sends before
    // calling `attached`.
    for _ in 0..100 {
        if observer.attached_count() == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(observer.attached_count(), 1);
    assert_eq!(observer.detached_count(), 0);

    // Abort the serve task outright instead of closing the connection in the
    // ordinary way: tokio drops the in-flight future in place, running every
    // local's `Drop` impl (including `AttachGuard`'s) without the function
    // ever reaching its own return statement.
    server.abort();
    let outcome = server.await;
    assert!(outcome.is_err() && outcome.unwrap_err().is_cancelled());

    assert_eq!(observer.detached_count(), 1);
}

/// The manager's reconnect-forever loop, exercised over a real TCP socket:
/// answer the handshake once as `inst-a`, sever the connection (a stand-in for
/// the daemon dying), then answer a second dial to the same address as
/// `inst-b`. The client must reconnect on its own and fire `Reattached`.
///
/// The server side is hand-rolled with the frame primitives instead of
/// [`serve_listener_tcp`] so the test can close the connection deterministically
/// right after the handshake; `serve_connection`'s own background tasks
/// (writer, event forwarder) would otherwise keep the socket's write half open
/// past an abort of the top-level task.
#[tokio::test]
async fn tcp_reconnect_fires_reattached_on_new_instance() {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
    let rt = tokio::runtime::Handle::current();
    let token = "secret-token".to_string();

    let attach_target = AttachTarget::Tcp {
        addr: addr.to_string(),
        token: token.clone(),
    };
    let attach_task = rt.spawn(WireNamespace::attach(
        attach_target,
        test_identity(),
        rt.clone(),
    ));

    // Establish the initial instance, check the presented token, then drop the
    // stream outright to sever the connection.
    {
        let (mut stream, _) = listener.accept().await.unwrap();
        let hello_frame = read_frame(&mut stream).await.unwrap().expect("hello frame");
        let Handshake::Hello {
            token: presented, ..
        } = postcard::from_bytes(&hello_frame.body).unwrap()
        else {
            panic!("expected a hello frame");
        };
        assert_eq!(presented.as_deref(), Some(token.as_str()));
        let welcome = postcard::to_allocvec(&Handshake::Welcome {
            protocol: PROTOCOL,
            instance_id: "inst-a".to_string(),
        })
        .unwrap();
        write_frame(&mut stream, &Frame::new(0, KIND_RESPONSE, welcome))
            .await
            .unwrap();
        // Dropping `stream` here closes the socket.
    }

    let ns = attach_task.await.unwrap().expect("initial attach");
    assert_eq!(ns.instance_id(), "inst-a");
    let mut attach_events = ns.subscribe_attach_events();

    // The manager reconnects to the same address on its own. Identify the new
    // connection as "inst-b" and keep it open.
    let (mut stream_b, _) = listener.accept().await.unwrap();
    let hello_frame = read_frame(&mut stream_b)
        .await
        .unwrap()
        .expect("second hello frame");
    let Handshake::Hello { .. } = postcard::from_bytes(&hello_frame.body).unwrap() else {
        panic!("expected a hello frame");
    };
    let welcome = postcard::to_allocvec(&Handshake::Welcome {
        protocol: PROTOCOL,
        instance_id: "inst-b".to_string(),
    })
    .unwrap();
    write_frame(&mut stream_b, &Frame::new(0, KIND_RESPONSE, welcome))
        .await
        .unwrap();

    let event = tokio::time::timeout(Duration::from_secs(5), attach_events.recv())
        .await
        .expect("reattach event must fire within the timeout")
        .unwrap();
    assert_eq!(
        event,
        crate::AttachEvent::Reattached {
            old_instance: "inst-a".to_string(),
            new_instance: "inst-b".to_string(),
        }
    );
    assert_eq!(ns.instance_id(), "inst-b");
    drop(stream_b);
}

// ===========================================================================
// Client-side cache: answer memo and read windows
// ===========================================================================
//
// These tests run a real `WireNamespace` (the memo and read windows live in the
// client) over a `UnixListener`-served counting stub. Each stub method bumps a
// per-op call counter, so a counter equals the number of wire round-trips: a
// memoized answer or a windowed read leaves it unchanged.

/// Effectively-infinite TTL, mirroring the engine's stable-entry policy.
const STUB_TTL_STATIC: Duration = Duration::from_secs(u32::MAX as u64);
/// A stable, exact-size ranged file (ttl > 0): windows and the memo engage.
const STABLE_NODE: NodeId = NodeId(100);
/// A stable child a directory listing names.
const CHILD_NODE: NodeId = NodeId(101);
/// A live file (ttl == 0): never memoized, never windowed.
const LIVE_NODE: NodeId = NodeId(200);
/// The node a stub `lookup` resolves to.
const LOOKUP_CHILD: NodeId = NodeId(300);
/// The directory parent used by the readdir and lookup tests.
const PARENT: NodeId = NodeId(50);

/// A deterministic byte at absolute file position `i`, so a windowed slice can be
/// checked against a read-through reference.
fn pat(i: u64) -> u8 {
    (i % 251) as u8
}

fn stable_attrs(size: u64) -> Attrs {
    Attrs {
        kind: NsEntryKind::File,
        size,
        ttl: STUB_TTL_STATIC,
        change: 0,
        direct_io: false,
        stability: StabilityClass::Stable,
        read_style: ReadStyle::Whole,
    }
}

fn live_attrs() -> Attrs {
    Attrs {
        kind: NsEntryKind::File,
        size: 1,
        ttl: Duration::ZERO,
        change: 0,
        direct_io: false,
        stability: StabilityClass::Live,
        read_style: ReadStyle::Ranged,
    }
}

/// A `Namespace` that counts every wire op and serves a stable file, a live file,
/// a directory child, and a lookup target with the TTLs the cache keys off.
struct MemoStub {
    events: broadcast::Sender<NsEvent>,
    stable_size: u64,
    getattr_calls: AtomicUsize,
    getattr_exact_calls: AtomicUsize,
    read_calls: AtomicUsize,
    lookup_calls: AtomicUsize,
    readdir_calls: AtomicUsize,
}

impl MemoStub {
    fn new(stable_size: u64) -> Arc<Self> {
        let (events, _) = broadcast::channel(64);
        Arc::new(Self {
            events,
            stable_size,
            getattr_calls: AtomicUsize::new(0),
            getattr_exact_calls: AtomicUsize::new(0),
            read_calls: AtomicUsize::new(0),
            lookup_calls: AtomicUsize::new(0),
            readdir_calls: AtomicUsize::new(0),
        })
    }

    /// Attrs for a stat of `node`: the stable file reports its full size, the live
    /// node reports a ttl == 0 answer, every other node a small stable file.
    fn attrs_of(&self, node: NodeId) -> Attrs {
        if node == LIVE_NODE {
            live_attrs()
        } else if node == STABLE_NODE {
            stable_attrs(self.stable_size)
        } else {
            stable_attrs(13)
        }
    }
}

impl Namespace for MemoStub {
    fn lookup<'a>(
        &'a self,
        _parent: NodeId,
        _name: &'a str,
    ) -> BoxFuture<'a, Result<NodeAnswer, NsError>> {
        self.lookup_calls.fetch_add(1, Ordering::SeqCst);
        async move {
            Ok(NodeAnswer {
                node: LOOKUP_CHILD,
                attrs: stable_attrs(13),
                kind: NsEntryKind::File,
            })
        }
        .boxed()
    }

    fn getattr(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        self.getattr_calls.fetch_add(1, Ordering::SeqCst);
        let attrs = self.attrs_of(node);
        async move { Ok(attrs) }.boxed()
    }

    fn getattr_exact(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        self.getattr_exact_calls.fetch_add(1, Ordering::SeqCst);
        let attrs = self.attrs_of(node);
        async move { Ok(attrs) }.boxed()
    }

    fn readdir(
        &self,
        _node: NodeId,
        _cursor: DirCursor,
        _budget: usize,
    ) -> BoxFuture<'_, Result<DirPage, NsError>> {
        self.readdir_calls.fetch_add(1, Ordering::SeqCst);
        async move {
            Ok(DirPage {
                entries: vec![DirEntry {
                    name: "child".to_string(),
                    node: CHILD_NODE,
                    attrs: stable_attrs(13),
                    kind: NsEntryKind::File,
                }],
                next: None,
            })
        }
        .boxed()
    }

    fn read(
        &self,
        node: NodeId,
        offset: u64,
        len: u32,
    ) -> BoxFuture<'_, Result<ReadAnswer, NsError>> {
        self.read_calls.fetch_add(1, Ordering::SeqCst);
        let stable_size = self.stable_size;
        async move {
            if node == LIVE_NODE {
                let bytes: Vec<u8> = (0..u64::from(len)).map(|k| pat(offset + k)).collect();
                return Ok(ReadAnswer {
                    bytes,
                    eof: false,
                    attrs: live_attrs(),
                });
            }
            let avail = stable_size.saturating_sub(offset);
            let take = u64::from(len).min(avail);
            let bytes: Vec<u8> = (0..take).map(|k| pat(offset + k)).collect();
            let eof = offset + take >= stable_size;
            Ok(ReadAnswer {
                bytes,
                eof,
                attrs: stable_attrs(stable_size),
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

/// Attach a real `WireNamespace` to a listener serving `stub`. The returned
/// tempdir owns the socket path and must outlive the namespace.
async fn attach_stub(stub: Arc<dyn Namespace>) -> (Arc<WireNamespace>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ns.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve_listener(
        stub,
        listener,
        "memo-inst".to_string(),
        None,
        None,
    ));
    let ns = WireNamespace::attach(
        AttachTarget::Unix(socket),
        test_identity(),
        tokio::runtime::Handle::current(),
    )
    .await
    .expect("attach");
    (ns, dir)
}

/// Push `event` from the server's stub and wait until the client has observed it.
/// The client applies the event to its cache before re-broadcasting it, so a
/// subscriber that receives it is guaranteed the memo is already pruned.
async fn push_and_settle(
    stub_events: &broadcast::Sender<NsEvent>,
    ns: &WireNamespace,
    event: NsEvent,
) {
    let mut sub = ns.subscribe();
    // The server's event forwarder subscribes at connection setup; wait for it so
    // the broadcast is not dropped for want of a receiver.
    while stub_events.receiver_count() == 0 {
        tokio::task::yield_now().await;
    }
    stub_events.send(event.clone()).unwrap();
    loop {
        match sub.recv().await {
            Some(got) if got == event => break,
            Some(_) => {},
            None => panic!("client event stream closed before the event arrived"),
        }
    }
}

#[tokio::test]
async fn ttl_zero_answers_never_served_from_memo() {
    let stub = MemoStub::new(1);
    let (ns, _dir) = attach_stub(Arc::clone(&stub) as Arc<dyn Namespace>).await;

    ns.getattr(LIVE_NODE).await.unwrap();
    ns.getattr(LIVE_NODE).await.unwrap();
    assert_eq!(
        stub.getattr_calls.load(Ordering::SeqCst),
        2,
        "a live (ttl == 0) node must round-trip every getattr"
    );
}

#[tokio::test]
async fn readdir_seed_serves_getattr_until_invalidated() {
    let stub = MemoStub::new(13);
    let (ns, _dir) = attach_stub(Arc::clone(&stub) as Arc<dyn Namespace>).await;

    let page = ns.readdir(PARENT, DirCursor::start(), 0).await.unwrap();
    assert_eq!(page.entries[0].node, CHILD_NODE);

    // The readdir carried the child's attrs, so a stat resolves from the memo.
    ns.getattr(CHILD_NODE).await.unwrap();
    assert_eq!(
        stub.getattr_calls.load(Ordering::SeqCst),
        0,
        "getattr must be served from the readdir seed"
    );
    // getattr_exact shares the same per-node memo (a ttl > 0 entry already carries
    // the exact size a probe would learn).
    ns.getattr_exact(CHILD_NODE).await.unwrap();
    assert_eq!(
        stub.getattr_exact_calls.load(Ordering::SeqCst),
        0,
        "getattr_exact must be served from the readdir seed"
    );

    push_and_settle(
        &stub.events,
        &ns,
        NsEvent::InvalidateSubtree {
            node: CHILD_NODE,
            epoch: Epoch(1),
        },
    )
    .await;

    ns.getattr(CHILD_NODE).await.unwrap();
    assert_eq!(
        stub.getattr_calls.load(Ordering::SeqCst),
        1,
        "an invalidation must force the next getattr to round-trip"
    );
}

#[tokio::test]
async fn lookup_memo_honors_parent_named_events() {
    let stub = MemoStub::new(13);
    let (ns, _dir) = attach_stub(Arc::clone(&stub) as Arc<dyn Namespace>).await;

    let first = ns.lookup(PARENT, "child").await.unwrap();
    let second = ns.lookup(PARENT, "child").await.unwrap();
    assert_eq!(first.node, second.node);
    assert_eq!(
        stub.lookup_calls.load(Ordering::SeqCst),
        1,
        "the second lookup must be served from the memo"
    );

    // The event names the parent, not the child: the (parent, name) entry drops.
    push_and_settle(
        &stub.events,
        &ns,
        NsEvent::InvalidateSubtree {
            node: PARENT,
            epoch: Epoch(1),
        },
    )
    .await;

    ns.lookup(PARENT, "child").await.unwrap();
    assert_eq!(
        stub.lookup_calls.load(Ordering::SeqCst),
        2,
        "a parent-named event must drop the lookup memo"
    );
}

#[tokio::test]
async fn read_windows_batch_sequential_reads() {
    let size = 8 * 1024 * 1024u64;
    let chunk = 128 * 1024u32;
    let stub = MemoStub::new(size);
    let (ns, _dir) = attach_stub(Arc::clone(&stub) as Arc<dyn Namespace>).await;

    // Seed the node's exact size so the read path knows it is windowable.
    assert_eq!(ns.getattr(STABLE_NODE).await.unwrap().size, size);

    let before = stub.read_calls.load(Ordering::SeqCst);
    let mut offset = 0u64;
    while offset < size {
        let answer = ns.read(STABLE_NODE, offset, chunk).await.unwrap();
        let expected_len =
            usize::try_from(u64::from(chunk).min(size - offset)).expect("chunk fits usize");
        assert_eq!(answer.bytes.len(), expected_len);
        for (k, byte) in answer.bytes.iter().enumerate() {
            assert_eq!(
                *byte,
                pat(offset + k as u64),
                "byte mismatch at {offset}+{k}"
            );
        }
        assert_eq!(answer.eof, offset + answer.bytes.len() as u64 >= size);
        offset += answer.bytes.len() as u64;
    }
    let windows = stub.read_calls.load(Ordering::SeqCst) - before;
    assert_eq!(windows, 4, "8 MiB in 2 MiB windows is exactly 4 wire reads");

    // The window drops on the node's event: re-seed the size, then the next read
    // must refetch rather than serve the stale window.
    push_and_settle(
        &stub.events,
        &ns,
        NsEvent::InvalidateSubtree {
            node: STABLE_NODE,
            epoch: Epoch(1),
        },
    )
    .await;
    ns.getattr(STABLE_NODE).await.unwrap();
    let before = stub.read_calls.load(Ordering::SeqCst);
    ns.read(STABLE_NODE, 0, chunk).await.unwrap();
    assert_eq!(
        stub.read_calls.load(Ordering::SeqCst) - before,
        1,
        "the window must have dropped on the event, forcing a refetch"
    );
}

#[tokio::test]
async fn ttl_zero_reads_never_window() {
    let stub = MemoStub::new(8 * 1024 * 1024);
    let (ns, _dir) = attach_stub(Arc::clone(&stub) as Arc<dyn Namespace>).await;

    let chunk = 128 * 1024u32;
    for i in 0..4u64 {
        ns.read(LIVE_NODE, i * u64::from(chunk), chunk)
            .await
            .unwrap();
    }
    assert_eq!(
        stub.read_calls.load(Ordering::SeqCst),
        4,
        "a ttl == 0 node reads straight through, one wire read per read"
    );
}

#[tokio::test]
async fn concurrent_reads_do_not_deadlock_or_double_fetch() {
    let size = 8 * 1024 * 1024u64;
    let chunk = 128 * 1024u32;
    let stub = MemoStub::new(size);
    let (ns, _dir) = attach_stub(Arc::clone(&stub) as Arc<dyn Namespace>).await;
    ns.getattr(STABLE_NODE).await.unwrap();

    let before = stub.read_calls.load(Ordering::SeqCst);
    let ns1 = Arc::clone(&ns);
    let ns2 = Arc::clone(&ns);
    // Two tasks read into the same first window at once.
    let t1 = tokio::spawn(async move { ns1.read(STABLE_NODE, 0, chunk).await });
    let t2 = tokio::spawn(async move { ns2.read(STABLE_NODE, u64::from(chunk), chunk).await });
    let r1 = t1.await.unwrap().unwrap();
    let r2 = t2.await.unwrap().unwrap();
    assert_eq!(r1.bytes[0], pat(0));
    assert_eq!(r2.bytes[0], pat(u64::from(chunk)));

    // At most one window fetch plus one pass-through: never one fetch per reader.
    // The chosen semantics: while a window fetch is in flight for a node, a
    // concurrent read on that node passes through directly rather than blocking.
    let reads = stub.read_calls.load(Ordering::SeqCst) - before;
    assert!(reads <= 2, "expected at most 2 wire reads, got {reads}");

    // A later read now hits the stored window with no wire read.
    let before = stub.read_calls.load(Ordering::SeqCst);
    ns.read(STABLE_NODE, 2 * u64::from(chunk), chunk)
        .await
        .unwrap();
    assert_eq!(stub.read_calls.load(Ordering::SeqCst) - before, 0);
}

// ---------------------------------------------------------------------------
// Inspector trace propagation across the wire
// ---------------------------------------------------------------------------
//
// A real, engine-backed `TreeNamespace` (over the in-tree `test_provider`),
// served over a real TCP loopback listener and dialed by a real
// `WireNamespace` - the same out-of-process topology the Docker-hosted FUSE
// frontend uses in production. `TreeNamespace` is the sole trace-minting
// authority (see `omnifs_engine::namespace`'s inspector-tracing section), so
// a wire-relayed op needs no protocol change to produce engine-side spans:
// the server-side dispatch already calls straight into `TreeNamespace`.

mod trace_propagation {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use omnifs_api::events::InspectorEvent;
    use omnifs_engine::{Namespace, NodeId, TreeNamespace};
    use tokio::runtime::Handle;

    use crate::{AttachTarget, FrontendIdentity, FrontendKind, WireNamespace, serve_listener_tcp};

    /// Drain up to `max` records from `live` within a generous per-record
    /// timeout, returning what arrived. Bounded so a missing event fails the
    /// test instead of hanging it.
    async fn drain(
        live: &mut tokio::sync::broadcast::Receiver<Arc<omnifs_api::events::InspectorRecord>>,
        max: usize,
    ) -> Vec<Arc<omnifs_api::events::InspectorRecord>> {
        let mut records = Vec::new();
        for _ in 0..max {
            match tokio::time::timeout(Duration::from_secs(5), live.recv()).await {
                Ok(Ok(record)) => records.push(record),
                _ => break,
            }
        }
        records
    }

    /// A read served through a wire-attached namespace produces engine-side
    /// trace records spanning the whole causal chain: the wire dispatch's
    /// namespace request events (`FuseStart`/`FuseEnd`) and the provider
    /// callout underneath them (`ProviderStart`/`ProviderEnd`), all tagged with
    /// the one id `TreeNamespace` minted for this call.
    #[tokio::test(flavor = "multi_thread")]
    #[allow(unsafe_code)] // env::remove_var requires unsafe; see SAFETY below.
    async fn wire_relayed_read_produces_engine_side_trace_records() {
        // SAFETY: cargo-nextest isolates each test into its own process, and
        // this runs before any other task in the process could read the var
        // concurrently.
        unsafe {
            std::env::remove_var("OMNIFS_INSPECTOR");
        }
        let sink =
            omnifs_engine::init_global_from_env().expect("inspector sink enabled by default");
        let mut live = sink.subscribe().live;

        let engine = omnifs_itest::make_engine();
        let harness = omnifs_itest::make_runtime(&engine);
        let runtime = Arc::new(harness.runtime);
        let tree_ns =
            TreeNamespace::single("test".to_string(), Arc::clone(&runtime), Handle::current());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_listener_tcp(
            tree_ns,
            listener,
            "inst-trace".to_string(),
            "secret".to_string(),
            None,
        ));

        let client = WireNamespace::attach(
            AttachTarget::Tcp {
                addr: addr.to_string(),
                token: "secret".to_string(),
            },
            FrontendIdentity {
                kind: FrontendKind::Fuse,
                mount_point: PathBuf::from("/mnt/trace-test"),
            },
            Handle::current(),
        )
        .await
        .expect("attach");

        // Mirrors what a frontend does to serve `cat /hello/message`: resolve
        // through two lookups, then read the whole (fresh, uncached) file, so
        // the read triggers a real provider callout underneath.
        let hello = client.lookup(NodeId::ROOT, "hello").await.unwrap();
        let message = client.lookup(hello.node, "message").await.unwrap();
        let read = client.read(message.node, 0, 4096).await.unwrap();
        assert_eq!(read.bytes, b"Hello, world!");

        let records = drain(&mut live, 64).await;
        let read_end = records
            .iter()
            .find(|r| matches!(&r.event, InspectorEvent::FuseEnd { op, .. } if op == "read"));
        let Some(read_end) = read_end else {
            panic!(
                "expected a read span from the wire-relayed read; got: {:?}",
                records.iter().map(|r| &r.event).collect::<Vec<_>>()
            );
        };
        let trace_id = read_end.trace_id;

        // The same id also tags a provider-callout event underneath it: the
        // "engine -> provider callout" hop of the restored chain, not just an
        // isolated frontend-facing span.
        let has_provider_event = records.iter().any(|r| {
            r.trace_id == trace_id
                && matches!(
                    &r.event,
                    InspectorEvent::ProviderStart { .. } | InspectorEvent::ProviderEnd { .. }
                )
        });
        assert!(
            has_provider_event,
            "expected a provider-op event sharing the read's trace id {trace_id}; got: {:?}",
            records
                .iter()
                .map(|r| (r.trace_id, &r.event))
                .collect::<Vec<_>>()
        );
    }
}
